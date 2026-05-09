"""
Wave XLR PipeWire audio manager.

The Wave XLR is a UAC1 USB device whose capture and playback iso endpoints
share a single audio clock. Anything that triggers a format renegotiation
or stream tear-down on the kernel-side ALSA stream takes both directions
silent for the duration. WirePlumber's idle-suspend behavior, plus apps
opening the device at different rates, makes that happen often enough to
be annoying — so we keep a permanent capture stream open as a "pin".

The pin is `pw-cat --record` targeted at the Wave XLR source. The non-
trivial part is the failure mode: `pw-cat` can end up *alive but not
receiving data* (PipeWire's view of the stream stalls without an EOF, so
pw-cat blocks on read forever). When that happens, the keepalive is no
longer keeping anything alive, but `proc.poll()` reports it healthy.

This module watches the byte stream coming out of pw-cat. At 48 kHz mono
s16, a healthy keepalive emits ~96 kB/s. If the byte counter doesn't
advance for WEDGE_TIMEOUT seconds while pw-cat is supposedly running,
we recycle it to release the shared USB clock.
"""

import json
import os
import signal
import subprocess
import threading
import time
import logging

log = logging.getLogger("wavexlr.audio")

SOURCE_MATCH = "alsa_input.usb-Elgato_Systems_Elgato_Wave_XLR"

# Seconds without byte flow before we consider the keepalive wedged. At
# 48 kHz mono s16 the healthy rate is ~96 kB/s, so even 1s of silence is
# already pathological; 3s allows generous slack for scheduler hiccups
# under heavy CPU load (game launches, kernel compiles, etc).
WEDGE_TIMEOUT = 3.0

# Watchdog tick. Short enough that recovery feels instant (under 4s
# total: WEDGE_TIMEOUT + WATCHDOG_INTERVAL), long enough not to burn CPU.
WATCHDOG_INTERVAL = 1.0

# pw-cat stdout drain buffer. Big enough to amortize syscalls, small
# enough that the watchdog notices the data flow promptly.
DRAIN_CHUNK = 4096

# Grace period after start before health-checking, so pw-cat has time
# to attach, negotiate, and start emitting samples.
STARTUP_GRACE = 1.0

# How long to wait for SIGTERM before escalating to SIGKILL when killing
# a wedged keepalive. A wedged pw-cat may not respond to SIGTERM at all
# (its main thread is blocked on a stalled stream read), so we don't
# wait long.
SIGTERM_GRACE = 0.5


def _pw_dump():
    """Get PipeWire object dump as JSON."""
    try:
        r = subprocess.run(
            ["pw-dump", "--no-colors"], capture_output=True, text=True, timeout=5
        )
        if r.returncode == 0:
            return json.loads(r.stdout)
    except Exception:
        pass
    return []


def _get_source_node_name():
    """Get the full node name of the Wave XLR source."""
    for obj in _pw_dump():
        if obj.get("type") != "PipeWire:Interface:Node":
            continue
        props = obj.get("info", {}).get("props", {})
        name = props.get("node.name", "")
        if name.startswith(SOURCE_MATCH):
            return name
    return None


class AudioManager:
    """Keeps the Wave XLR capture stream active via a watched pw-cat subprocess.

    The subprocess's stdout is drained by a reader thread; the main loop
    detects wedge ("alive but no data") and recycles the subprocess.
    """

    def __init__(self, on_status_change=None):
        self._running = False
        self._loop_thread = None
        self._cat_proc = None
        self._reader_thread = None
        self._last_data_at = 0.0
        self._healthy = False
        self._device_present = False
        self.on_status_change = on_status_change

    @property
    def healthy(self):
        return self._healthy

    @property
    def device_present(self):
        return self._device_present

    def start(self):
        if self._running:
            return
        self._running = True
        self._loop_thread = threading.Thread(target=self._run, daemon=True)
        self._loop_thread.start()

    def stop(self):
        self._running = False
        self._kill_cat()
        if self._loop_thread:
            self._loop_thread.join(timeout=3)

    def _kill_cat(self):
        proc = self._cat_proc
        reader = self._reader_thread
        self._cat_proc = None
        self._reader_thread = None
        if proc and proc.poll() is None:
            try:
                proc.terminate()
                proc.wait(timeout=SIGTERM_GRACE)
            except subprocess.TimeoutExpired:
                # Wedged streams sometimes ignore SIGTERM — kill the
                # whole process group to be sure.
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except (ProcessLookupError, PermissionError):
                    proc.kill()
                try:
                    proc.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    pass
            log.info("Stopped capture keepalive")
        # Reader thread exits when the pipe closes.
        if reader and reader.is_alive():
            reader.join(timeout=2)

    def _start_cat(self, source_name):
        """Spawn pw-cat with stdout piped so we can monitor byte flow."""
        self._kill_cat()
        self._last_data_at = time.monotonic()
        self._cat_proc = subprocess.Popen(
            [
                "pw-cat", "--record",
                "--target", source_name,
                "--channels", "1",
                "--format", "s16",
                "--rate", "48000",
                "--latency", "200ms",
                "-",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            # New process group so SIGKILL on the leader cleans up any
            # children too. start_new_session=True is the portable spelling.
            start_new_session=True,
        )
        self._reader_thread = threading.Thread(
            target=self._drain, args=(self._cat_proc,), daemon=True
        )
        self._reader_thread.start()
        log.info(f"Started capture keepalive (PID {self._cat_proc.pid})")

    def _drain(self, proc):
        """Drain pw-cat's stdout, updating the last-data-received timestamp.

        Healthy flow at 48 kHz mono s16 is ~96 kB/s — when the device
        wedges, this read blocks indefinitely. The watchdog notices via
        the timestamp.
        """
        try:
            while True:
                chunk = proc.stdout.read(DRAIN_CHUNK)
                if not chunk:
                    return
                self._last_data_at = time.monotonic()
        except Exception:
            return
        finally:
            try:
                proc.stdout.close()
            except Exception:
                pass

    def _cat_alive(self):
        return self._cat_proc is not None and self._cat_proc.poll() is None

    def _data_flowing(self):
        return (time.monotonic() - self._last_data_at) < WEDGE_TIMEOUT

    def _update_status(self, present, healthy):
        changed = (present != self._device_present) or (healthy != self._healthy)
        self._device_present = present
        self._healthy = healthy
        if changed and self.on_status_change:
            self.on_status_change(present, healthy)

    def _run(self):
        while self._running:
            try:
                if self._cat_alive():
                    if self._data_flowing():
                        self._update_status(True, True)
                        time.sleep(WATCHDOG_INTERVAL)
                        continue
                    stalled_for = time.monotonic() - self._last_data_at
                    log.warning(
                        f"Capture keepalive wedged ({stalled_for:.1f}s without "
                        "data); recycling to release the shared USB clock"
                    )
                    self._kill_cat()
                    self._update_status(True, False)
                    # Brief settle so PipeWire fully releases the device
                    # before the new pw-cat reattaches.
                    time.sleep(0.5)
                    continue

                if self._cat_proc is not None:
                    log.warning(
                        f"Capture keepalive exited unexpectedly "
                        f"(rc={self._cat_proc.poll()}); restarting"
                    )
                    self._cat_proc = None

                source_name = _get_source_node_name()
                if not source_name:
                    self._update_status(False, False)
                    time.sleep(5)
                    continue

                self._start_cat(source_name)
                time.sleep(STARTUP_GRACE)
                self._update_status(True, self._cat_alive() and self._data_flowing())

            except Exception as e:
                log.error(f"Audio manager error: {e}")
                self._update_status(self._device_present, False)
                time.sleep(2)
