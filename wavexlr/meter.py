"""Low-rate PipeWire peak meters with worker-owned process lifetimes.

start(id, node_name, callback, capture_sink=False) replaces that id's generation.
callback(peak: float) runs on the GLib main thread, with normalized linear peaks.
stop(id) and stop_all() cancel immediately without waiting on the GTK thread.
wait_stopped(timeout=3.0) joins cancelled workers during off-thread shutdown;
it returns False if the timeout expires. A worker owns spawn, read, pipe closure
and bounded terminate/kill/reap, including when cancelled during process startup.
ui_suspended suppresses visual callbacks only; byte-flow timestamps still update.
"""

import json
import os
import selectors
import struct
import subprocess
import threading
import time
from dataclasses import dataclass, field

import gi

gi.require_version("GLib", "2.0")
from gi.repository import GLib  # noqa: E402

from . import child


@dataclass(eq=False)
class _Meter:
    callback: object
    node_name: str = ""
    identity: object = None
    received: bool = False
    stop: threading.Event = field(default_factory=threading.Event)
    proc: object = None
    thread: object = None
    last_data: float = field(default_factory=time.monotonic)


class MeterMonitor:
    SAMPLE_RATE = 8000
    CHUNK_BYTES = 1024
    _QUIET = 0.004
    _TAIL_FRAMES = 20
    _POLL_SECONDS = 0.1
    _REAP_SECONDS = 0.5

    def __init__(self):
        self.ui_suspended = False
        self._meters = {}
        self._workers = set()
        self._lock = threading.RLock()

    def start(self, source_id, source_node_name, callback, capture_sink=False, *, identity=None):
        """Start a source tap, or a sink-monitor tap with capture_sink=True.

        Spawn errors produce a final zero callback; running() stays False.
        No callback or byte timestamp from an older generation can affect the
        replacement, including callbacks already queued on the GLib main loop.
        """
        state = _Meter(callback, source_node_name, identity)
        state.thread = threading.Thread(
            target=self._reader,
            args=(source_id, source_node_name, capture_sink, state),
            name=f"openwave-meter-{source_id}",
            daemon=False,
        )
        with self._lock:
            self.stop(source_id)
            self._meters[source_id] = state
            self._workers.add(state.thread)
            state.thread.start()

    def running(self, source_id):
        """Whether this id has a live, uncancelled meter subprocess."""
        with self._lock:
            state = self._meters.get(source_id)
            return bool(state and state.proc and state.proc.poll() is None
                        and not state.stop.is_set())

    def active(self, source_id):
        """Include a generation whose worker is still starting its process."""
        with self._lock:
            state = self._meters.get(source_id)
            return bool(state and state.thread.is_alive() and not state.stop.is_set())

    def ready(self, node_name, identity):
        """True only after this exact capture generation has delivered bytes."""
        with self._lock:
            return any(state.node_name == node_name and state.identity == identity
                       and state.received and self.running(source_id)
                       for source_id, state in self._meters.items())

    def stop(self, source_id):
        """Invalidate queued callbacks and request bounded off-thread cleanup."""
        with self._lock:
            state = self._meters.pop(source_id, None)
            if state is not None:
                state.stop.set()

    def stop_all(self):
        """Cancel all generations, including workers still spawning pw-cat."""
        with self._lock:
            for state in self._meters.values():
                state.stop.set()
            self._meters.clear()

    def wait_stopped(self, timeout=3.0):
        """Join workers within one total deadline; do not call from GTK.

        Call stop_all() first. This does not cancel currently running meters.
        """
        deadline = time.monotonic() + max(0.0, timeout)
        with self._lock:
            workers = tuple(self._workers)
        for worker in workers:
            if worker is threading.current_thread():
                return False
            worker.join(max(0.0, deadline - time.monotonic()))
        return all(not worker.is_alive() for worker in workers)

    def silent_for(self, source_id):
        """Seconds since any bytes arrived, or None when no tap is running.

        Silence (zero-valued samples) is data. A failed pw-cat does not imply a
        stalled device, and therefore returns None rather than a growing age.
        """
        with self._lock:
            state = self._meters.get(source_id)
            if not self.running(source_id):
                return None
            return time.monotonic() - state.last_data

    def _reader(self, source_id, node_name, capture_sink, state):
        proc = None
        try:
            if state.stop.is_set():
                return
            props = {
                "node.name": f"openwave_meter_{source_id}",
                "node.description": f"OpenWave level meter ({source_id})",
                "application.name": "OpenWave",
                "media.name": f"OpenWave meter: {source_id}",
                "node.dont-fallback": True,
                "node.dont-reconnect": True,
                "node.dont-move": True,
            }
            if capture_sink:
                props["stream.capture.sink"] = True
            proc = child.spawn(
                ["pw-cat", "--record", "--target", node_name,
                 "--properties", json.dumps(props),
                 "--rate", str(self.SAMPLE_RATE), "--channels", "1",
                 "--format", "s16", "-"],
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                bufsize=0,
            )
            with self._lock:
                state.proc = proc
                state.last_data = time.monotonic()
            os.set_blocking(proc.stdout.fileno(), False)
            tail = 0
            settled = False
            pending = b""
            with selectors.DefaultSelector() as selector:
                selector.register(proc.stdout, selectors.EVENT_READ)
                while not state.stop.is_set():
                    if not selector.select(self._POLL_SECONDS):
                        continue
                    try:
                        data = os.read(proc.stdout.fileno(), self.CHUNK_BYTES)
                    except BlockingIOError:
                        continue
                    if not data:
                        break
                    with self._lock:
                        if self._meters.get(source_id) is not state:
                            break
                        state.last_data = time.monotonic()
                        state.received = True
                    if self.ui_suspended:
                        settled = False
                        tail = 0
                    data = pending + data
                    size = len(data) & ~1
                    pending = data[size:]
                    if not size or self.ui_suspended:
                        continue
                    samples = struct.unpack(f"<{size // 2}h", data[:size])
                    peak = max(abs(sample) for sample in samples) / 32768.0
                    if peak >= self._QUIET:
                        tail = self._TAIL_FRAMES
                        settled = False
                    elif tail:
                        tail -= 1
                    elif settled:
                        continue
                    else:
                        peak = 0.0
                        settled = True
                    GLib.idle_add(self._dispatch, source_id, state, peak)
        except (OSError, ValueError):
            pass
        finally:
            if proc is not None:
                self._reap(proc)
                if proc.stdout is not None:
                    proc.stdout.close()
            if not state.stop.is_set() and not self.ui_suspended:
                GLib.idle_add(self._dispatch, source_id, state, 0.0)
            with self._lock:
                self._workers.discard(state.thread)

    @classmethod
    def _reap(cls, proc):
        try:
            proc.terminate()
        except OSError:
            pass
        try:
            proc.wait(timeout=cls._REAP_SECONDS)
            return
        except subprocess.TimeoutExpired:
            pass
        try:
            proc.kill()
        except OSError:
            pass
        try:
            proc.wait(timeout=cls._REAP_SECONDS)
        except subprocess.TimeoutExpired:
            pass

    def _dispatch(self, source_id, state, peak):
        with self._lock:
            if (self._meters.get(source_id) is state
                    and not state.stop.is_set() and not self.ui_suspended):
                state.callback(peak)
        return False
