"""
Elgato Wave PipeWire audio manager (Wave XLR, Wave:3).

These are UAC1 USB devices whose capture and playback iso endpoints
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

Byte flow alone is not enough to call the keepalive healthy, because the
device has a second failure mode that satisfies it: the stream stays up
and every sample in it is zero. Nothing about that looks wrong from
outside -- pw-cat runs, bytes arrive at the full 96 kB/s, the config
block reads unmuted with gain set, and the device's own meter still
moves -- so the mic is silent while everything reports working. We watch
for a non-zero sample as well, which is what separates the two.

Recycling does not clear the silent state; neither does a USB
re-enumeration. Only a power cycle does. So silence is reported rather
than retried: one recycle in case this instance of it is the recoverable
kind, and after that the manager holds the stream and says what is
wrong, instead of tearing it down every few seconds forever.
"""

import json
import os
import signal
import subprocess
import threading
import time
import logging

log = logging.getLogger("wavexlr.audio")

SOURCE_MATCH = "alsa_input.usb-Elgato_Systems_Elgato_Wave_"

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

# Seconds of unbroken digital silence before the stream counts as dead.
# A capturing microphone always carries a noise floor: measured against a
# working Wave XLR in a quiet room with nobody speaking, ~91% of frames
# hold a non-zero sample. Exact zeros for this long is a dead stream
# rather than a quiet one.
SILENCE_TIMEOUT = 30.0

# How often to re-examine a stream already known to be silent, and how
# long to cache the source's mute state. Neither answer changes fast.
SILENCE_RECHECK = 5.0

# Recycles to spend on a silent stream before treating it as something
# that has to be reported instead. Silence has not been observed to
# recover from a recycle, and retrying forever is how the no-data path
# used to bury the fault it was reporting.
MAX_SILENCE_RECYCLES = 1

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


def _source_is_muted(node_name):
    """Whether the Wave source is muted, making digital silence expected.

    Mute sits on the device's input Route rather than on the node, so this
    walks node -> device.id -> Route. Worth the extra lookup: a muted mic
    is all zeros by definition, and reporting that as a broken stream
    would turn the mute button into a fault light.
    """
    if not node_name:
        return False
    dump = _pw_dump()
    device_id = None
    for obj in dump:
        if obj.get("type") != "PipeWire:Interface:Node":
            continue
        props = obj.get("info", {}).get("props", {})
        if props.get("node.name") == node_name:
            device_id = props.get("device.id")
            break
    if device_id is None:
        return False
    try:
        device_id = int(device_id)
    except (TypeError, ValueError):
        return False
    for obj in dump:
        if obj.get("id") != device_id:
            continue
        routes = obj.get("info", {}).get("params", {}).get("Route") or []
        for route in routes:
            if route.get("direction") == "Input":
                return bool(route.get("props", {}).get("mute"))
    return False


def _get_source_node_name():
    """Get the full node name of the Elgato Wave source."""
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
        self._last_signal_at = 0.0
        self._source_name = None
        self._silence_recycles = 0
        self._muted = False
        self._mute_checked_at = 0.0
        self._healthy = False
        self._state = "absent"
        self._device_present = False
        self._status_reported = False
        self.on_status_change = on_status_change

    @property
    def healthy(self):
        return self._healthy

    @property
    def state(self):
        """One of "ok", "wedged", "silent", "absent"."""
        return self._state

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
        now = time.monotonic()
        self._last_data_at = now
        self._last_signal_at = now
        self._source_name = source_name
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
                now = time.monotonic()
                self._last_data_at = now
                # count(0) is a C-level scan; `any(chunk)` would be a
                # Python loop over every byte of a 96 kB/s stream.
                if chunk.count(0) != len(chunk):
                    self._last_signal_at = now
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

    def _signal_flowing(self):
        return (time.monotonic() - self._last_signal_at) < SILENCE_TIMEOUT

    def _source_muted(self):
        """Cached mute state; pw-dump is too heavy for every watchdog tick."""
        now = time.monotonic()
        if now - self._mute_checked_at < SILENCE_RECHECK:
            return self._muted
        self._mute_checked_at = now
        self._muted = _source_is_muted(self._source_name)
        return self._muted

    def _update_status(self, present, healthy, state):
        changed = (
            not self._status_reported
            or present != self._device_present
            or healthy != self._healthy
            or state != self._state
        )
        self._device_present = present
        self._healthy = healthy
        self._state = state
        self._status_reported = True
        if changed and self.on_status_change:
            self.on_status_change(present, healthy, state)

    def _run(self):
        while self._running:
            try:
                if self._cat_alive():
                    if not self._data_flowing():
                        stalled_for = time.monotonic() - self._last_data_at
                        log.warning(
                            f"Capture keepalive wedged ({stalled_for:.1f}s "
                            "without data); recycling to release the shared "
                            "USB clock"
                        )
                        self._kill_cat()
                        self._update_status(True, False, "wedged")
                        # Brief settle so PipeWire fully releases the device
                        # before the new pw-cat reattaches.
                        time.sleep(0.5)
                        continue

                    if self._signal_flowing():
                        self._silence_recycles = 0
                        self._update_status(True, True, "ok")
                        time.sleep(WATCHDOG_INTERVAL)
                        continue

                    if self._source_muted():
                        # Zeros are the correct output for a muted mic.
                        # Move the clock along so an unmute is what starts
                        # the silence window, not the mute that preceded it.
                        self._last_signal_at = time.monotonic()
                        self._update_status(True, True, "ok")
                        time.sleep(WATCHDOG_INTERVAL)
                        continue

                    silent_for = time.monotonic() - self._last_signal_at
                    if self._silence_recycles < MAX_SILENCE_RECYCLES:
                        self._silence_recycles += 1
                        log.warning(
                            f"Capture stream silent ({silent_for:.0f}s of zero "
                            "samples while unmuted); recycling once"
                        )
                        self._kill_cat()
                        self._update_status(True, False, "silent")
                        time.sleep(0.5)
                        continue

                    self._update_status(True, False, "silent")
                    time.sleep(SILENCE_RECHECK)
                    continue

                if self._cat_proc is not None:
                    log.warning(
                        f"Capture keepalive exited unexpectedly "
                        f"(rc={self._cat_proc.poll()}); restarting"
                    )
                    self._cat_proc = None

                source_name = _get_source_node_name()
                if not source_name:
                    self._update_status(False, False, "absent")
                    time.sleep(5)
                    continue

                self._start_cat(source_name)
                time.sleep(STARTUP_GRACE)
                started = self._cat_alive() and self._data_flowing()
                self._update_status(True, started, "ok" if started else "wedged")

            except Exception as e:
                log.error(f"Audio manager error: {e}")
                self._update_status(self._device_present, False, self._state)
                time.sleep(2)
