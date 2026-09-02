"""Audio mixer — manages pw-loopback subprocesses for the matrix.

A loopback exists for each non-zero cell in the matrix (mic → mix), plus one
that always routes Personal Mix → Wave XLR headphones so the user hears
anything routed there. Volume + mute per cell are pushed onto the loopback's
playback node via wpctl.

State is persisted to ~/.config/openwave/mixes.json so per-cell levels survive
restarts (the loopbacks themselves do not — they're respawned by start()).

PipeWire can destroy and recreate the nodes all of this is named after, and
nothing announces it, so the worker re-checks device names and loopback
liveness on a timer and rebuilds whatever no longer matches.
"""

import atexit
import ctypes
import json
import logging
import os
import signal
import subprocess
import threading
import time
from threading import Event, Lock

_log = logging.getLogger(__name__)

# Linux-only: make spawned children receive SIGTERM if our process dies.
# Survives SIGKILL on the parent, hard crashes, anything that skips Python
# cleanup paths. Without this, pw-loopback children leak on unclean exit.
_PR_SET_PDEATHSIG = 1
try:
    _libc = ctypes.CDLL("libc.so.6", use_errno=True)
    _libc.prctl.argtypes = (
        ctypes.c_int, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong,
    )
    _libc.prctl.restype = ctypes.c_int
except (OSError, AttributeError):
    _libc = None


def _set_pdeathsig():
    if _libc is not None:
        _libc.prctl(_PR_SET_PDEATHSIG, int(signal.SIGTERM), 0, 0, 0)

CONFIG_PATH = os.path.expanduser("~/.config/openwave/mixes.json")

MIX_SINKS = {
    "personal": "openwave_personal_mix",
    "chat":     "openwave_chat_mix",
    "record":   "openwave_record_mix",
}
# How often the worker re-resolves the Wave's node names and checks its
# loopbacks are alive. A recovery path, not a control one: nothing waits on it.
DEVICE_RECHECK_INTERVAL = 5.0

PERSONAL_MIX_SINK = "openwave_personal_mix"
HP_LOOPBACK_KEY = "_personal_to_hp"
HP_LOOPBACK_NODE = "openwave_loop_personal_to_hp"


def _pactl_short(kind):
    try:
        r = subprocess.run(
            ["pactl", "list", "short", kind],
            capture_output=True, text=True, timeout=3,
        )
    except (FileNotFoundError, subprocess.SubprocessError):
        return []
    return [line.split("\t") for line in r.stdout.splitlines() if line.strip()]


def find_wave_xlr_alsa():
    """Return (mic_node_name, hp_node_name); either may be None if unplugged."""
    mic = next(
        (p[1] for p in _pactl_short("sources")
         if len(p) > 1 and p[1].startswith("alsa_input") and "Elgato_Wave_" in p[1]),
        None,
    )
    hp = next(
        (p[1] for p in _pactl_short("sinks")
         if len(p) > 1 and p[1].startswith("alsa_output") and "Elgato_Wave_" in p[1]),
        None,
    )
    return mic, hp


def _node_id_by_name(name, retries=20):
    """Look up a PipeWire node's global id by node.name, polling briefly so
    we don't race a just-spawned pw-loopback. Returns None if not found."""
    for _ in range(retries):
        try:
            r = subprocess.run(
                ["pw-cli", "ls", "Node"],
                capture_output=True, text=True, timeout=3,
            )
        except (FileNotFoundError, subprocess.SubprocessError):
            return None
        current_id = None
        for raw in r.stdout.splitlines():
            line = raw.strip()
            if line.startswith("id "):
                try:
                    current_id = line.split()[1].rstrip(",")
                except (IndexError, ValueError):
                    current_id = None
            elif current_id and line == f'node.name = "{name}"':
                return current_id
        time.sleep(0.05)
    return None


def _wpctl(*args):
    try:
        subprocess.run(
            ["wpctl", *args],
            capture_output=True, text=True, timeout=3,
        )
    except (FileNotFoundError, subprocess.SubprocessError):
        pass


def _ports(direction_flag, node_name):
    """Return the list of `node:port` strings for one direction of a node.

    direction_flag is '-i' (inputs) or '-o' (outputs). Filters pw-link's
    global output to ports whose node.name equals `node_name`.
    """
    try:
        r = subprocess.run(
            ["pw-link", direction_flag],
            capture_output=True, text=True, timeout=3,
        )
    except (FileNotFoundError, subprocess.SubprocessError):
        return []
    prefix = f"{node_name}:"
    return [line.strip() for line in r.stdout.splitlines() if line.strip().startswith(prefix)]


def list_audio_streams():
    """Return [{id, app_name, media_name, node_name}, ...] for active output streams."""
    import json as _json
    try:
        r = subprocess.run(
            ["pw-dump"], capture_output=True, text=True, timeout=5,
        )
        if r.returncode != 0:
            return []
        objects = _json.loads(r.stdout)
    except (FileNotFoundError, subprocess.SubprocessError, _json.JSONDecodeError):
        return []

    out = []
    for obj in objects:
        if obj.get("type") != "PipeWire:Interface:Node":
            continue
        props = (obj.get("info") or {}).get("props") or {}
        if props.get("media.class") != "Stream/Output/Audio":
            continue
        app = props.get("application.name") or props.get("node.name") or "Unknown"
        # Skip our own loopbacks
        node_name = props.get("node.name", "")
        if node_name.startswith("openwave_"):
            continue
        out.append({
            "id": obj["id"],
            "app_name": app,
            "media_name": props.get("media.name", ""),
            "node_name": node_name,
            "binary": props.get("application.process.binary", ""),
        })
    return out


class Mixer:
    """Manages pw-loopback subprocesses for the matrix's mic row."""

    def __init__(self, on_devices_changed=None):
        self._lock = Lock()
        self._procs = {}
        self._state = self._load_state()
        self._sources = {}
        self._streams = {}
        self.mic, self.hp = find_wave_xlr_alsa()

        # Fired on the worker when mic/hp resolve to something different.
        self.on_devices_changed = on_devices_changed
        self._last_device_check = time.monotonic()

        # Background worker: every operation that talks to pw-loopback /
        # pw-cli / wpctl runs here so the GTK main thread never blocks on a
        # subprocess. Pending work is a dict keyed by (kind, …) so successive
        # set_cell calls on the same cell collapse to a single reconcile.
        self._pending = {}
        self._pending_lock = Lock()
        self._wake = Event()
        self._worker_running = True
        self._worker = threading.Thread(
            target=self._worker_loop, name="openwave-mixer", daemon=True,
        )
        self._worker.start()

        # Belt-and-suspenders: even if do_shutdown is skipped, the interpreter
        # almost always runs atexit before the process image goes away.
        atexit.register(self._atexit_cleanup)

    # ----- worker thread -----
    def _enqueue(self, key, task):
        """Coalesce a task by key. Latest task for the same key wins."""
        with self._pending_lock:
            self._pending[key] = task
            self._wake.set()

    def _worker_loop(self):
        while self._worker_running:
            self._wake.wait(timeout=1.0)
            while True:
                with self._pending_lock:
                    if not self._pending:
                        self._wake.clear()
                        break
                    key = next(iter(self._pending))
                    task = self._pending.pop(key)
                try:
                    task()
                except Exception:
                    _log.exception("mixer task failed: %s", key)
            if not self._worker_running:
                return
            self._device_watchdog()

    # ----- device watchdog -----
    def _device_watchdog(self):
        """Re-check what the mixer is built on, and rebuild what moved."""
        now = time.monotonic()
        if now - self._last_device_check < DEVICE_RECHECK_INTERVAL:
            return
        self._last_device_check = now
        try:
            devices_changed = self._refresh_devices()
            loopbacks_died = self._reap_dead_loopbacks()
        except Exception:
            _log.exception("device watchdog failed")
            return
        if devices_changed or loopbacks_died:
            try:
                self._reconcile_all()
            except Exception:
                _log.exception("watchdog reconcile failed")
        if devices_changed and self.on_devices_changed is not None:
            try:
                self.on_devices_changed()
            except Exception:
                _log.exception("on_devices_changed callback failed")

    def _refresh_devices(self):
        """Re-resolve the Wave's node names. True if either moved.

        A moved name leaves loopbacks wired to a node that no longer exists,
        which liveness alone misses: the process is still running.
        """
        mic, hp = find_wave_xlr_alsa()
        if (mic, hp) == (self.mic, self.hp):
            return False
        old_mic, old_hp = self.mic, self.hp
        self.mic, self.hp = mic, hp
        _log.info(
            "Wave nodes moved: mic %s -> %s, hp %s -> %s", old_mic, mic, old_hp, hp,
        )
        if mic != old_mic:
            for key in [
                k for k in list(self._procs)
                if isinstance(k, tuple) and k and k[0] == "mic"
            ]:
                self._destroy_loopback(key)
        if hp != old_hp:
            self._destroy_loopback(HP_LOOPBACK_KEY)
        return True

    def _reap_dead_loopbacks(self):
        """Drop entries whose child has exited. True if any had."""
        dead = [k for k, proc in list(self._procs.items()) if proc.poll() is not None]
        for key in dead:
            self._procs.pop(key, None)
            _log.info("loopback %s exited; rebuilding", key)
        return bool(dead)

    def _ensure_hp_loopback(self):
        """Personal Mix → headphones, whenever the Wave has a playback node.

        Reconciled rather than spawned once at start, so it also appears if the
        Wave arrives late and goes away when it leaves.
        """
        if self.hp:
            self._spawn_loopback(
                HP_LOOPBACK_KEY, PERSONAL_MIX_SINK, self.hp, HP_LOOPBACK_NODE,
            )
        else:
            self._destroy_loopback(HP_LOOPBACK_KEY)

    # ----- persistence -----
    def _load_state(self):
        try:
            with open(CONFIG_PATH) as f:
                return json.load(f)
        except (FileNotFoundError, json.JSONDecodeError):
            return {}

    def _save_state(self):
        os.makedirs(os.path.dirname(CONFIG_PATH), exist_ok=True)
        tmp = CONFIG_PATH + ".tmp"
        with open(tmp, "w") as f:
            json.dump(self._state, f, indent=2)
        os.replace(tmp, CONFIG_PATH)

    def get_cell(self, source_id, mix_id):
        return self._state.get(
            f"{source_id}.{mix_id}", {"volume": 0.0, "muted": False}
        )

    def cells(self):
        return dict(self._state)

    def streams(self):
        """Snapshot of currently-known PipeWire output streams (id → info)."""
        with self._lock:
            return dict(self._streams)

    # ----- subprocess lifecycle -----
    def _spawn_loopback(self, key, capture_source_name, playback_target, node_name):
        """Spawn a pw-loopback and *manually* link the capture side to
        `capture_source_name`'s output ports. We disable autoconnect on capture
        because the session manager will otherwise hijack the loopback by
        wiring the default source (the Wave XLR mic) into it whenever
        target.object can't be resolved to a Source node — which is exactly
        the case for null-sink monitors. The link is set up after a brief
        wait so the node has time to register.
        """
        proc = self._procs.get(key)
        if proc is not None:
            if proc.poll() is None:
                return
            # Child is gone but its entry outlived it, which is what stops this
            # from ever rebuilding.
            self._procs.pop(key, None)
        capture_node_name = f"{node_name}_cap"
        try:
            proc = subprocess.Popen(
                [
                    "pw-loopback",
                    "--capture-props="
                    f"node.autoconnect=false node.name={capture_node_name} "
                    "audio.channels=2 audio.position=[FL,FR]",
                    "--playback-props="
                    f"target.object={playback_target} node.name={node_name} "
                    "audio.channels=2 audio.position=[FL,FR]",
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                preexec_fn=_set_pdeathsig,
            )
        except (FileNotFoundError, OSError):
            return
        self._procs[key] = proc
        self._link_capture(capture_source_name, capture_node_name)

    @staticmethod
    def _link_capture(source_node_name, capture_node_name, retries=20):
        """Wire each output port of `source_node_name` to a corresponding
        input port of `capture_node_name`. Mono → stereo duplicates."""
        for _ in range(retries):
            src_ports = _ports("-o", source_node_name)
            dst_ports = _ports("-i", capture_node_name)
            if src_ports and dst_ports:
                break
            time.sleep(0.05)
        else:
            return
        for i, dst in enumerate(dst_ports):
            src = src_ports[i % len(src_ports)]
            try:
                subprocess.run(
                    ["pw-link", src, dst],
                    capture_output=True, text=True, timeout=2,
                )
            except (FileNotFoundError, subprocess.SubprocessError):
                return

    def _destroy_loopback(self, key):
        proc = self._procs.pop(key, None)
        if proc is None:
            return
        try:
            proc.terminate()
        except (OSError, ProcessLookupError):
            return
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            try:
                proc.kill()
                proc.wait(timeout=1)
            except (OSError, ProcessLookupError, subprocess.TimeoutExpired):
                pass

    def _atexit_cleanup(self):
        """Fast best-effort tear-down on interpreter exit. No locking, no waits."""
        for proc in list(self._procs.values()):
            try:
                proc.terminate()
            except (OSError, ProcessLookupError):
                continue
        self._procs.clear()

    # Below this, the slider snaps to 0 — sub-1% values keep the loopback alive
    # at imperceptible-but-not-silent volume and confuse "I put it back to 0".
    _ZERO_THRESHOLD = 0.01

    # ----- public API (returns immediately; subprocess work runs on worker) -----
    def start(self):
        """Spawn always-on Personal→HP loopback, snapshot streams, restore cells."""
        self._enqueue(("start",), self._do_start)

    def stop(self):
        """Stop the worker and tear down every loopback. Brief block expected."""
        self._worker_running = False
        self._wake.set()
        try:
            self._worker.join(timeout=3)
        except RuntimeError:
            pass
        with self._lock:
            for key in list(self._procs.keys()):
                self._destroy_loopback(key)

    def set_cell(self, source_id, mix_id, volume, muted):
        """Persist state synchronously; reconcile the cell on the worker."""
        volume = max(0.0, min(1.0, float(volume)))
        if volume < self._ZERO_THRESHOLD:
            volume = 0.0
        with self._lock:
            self._state[f"{source_id}.{mix_id}"] = {
                "volume": volume, "muted": bool(muted),
            }
            self._save_state()
        self._enqueue(
            ("cell", source_id, mix_id),
            lambda sid=source_id, mid=mix_id: self._reconcile_cell(sid, mid),
        )

    def set_sources(self, sources):
        """Update the app-source configuration; reconcile on worker."""
        with self._lock:
            self._sources = dict(sources)
        self._enqueue(("set_sources",), self._reconcile_all)

    def remove_source(self, source_id):
        """Forget persisted cells now; tear down loopbacks on worker."""
        with self._lock:
            prefix = f"{source_id}."
            for cell_key in [k for k in self._state if k.startswith(prefix)]:
                del self._state[cell_key]
            self._save_state()
            self._sources.pop(source_id, None)
        self._enqueue(
            ("remove", source_id),
            lambda sid=source_id: self._do_remove_source(sid),
        )

    def poll_streams(self):
        """Refresh the active-stream cache; reconcile on worker if anything moved.

        Returns (added, removed) stream-id sets for the caller's bookkeeping."""
        new = {s["id"]: s for s in list_audio_streams()}
        with self._lock:
            added = set(new) - set(self._streams)
            removed = set(self._streams) - set(new)
            self._streams = new
        if added or removed:
            self._enqueue(("poll",), self._reconcile_all)
        return added, removed

    # ----- worker-side implementations -----
    def _do_start(self):
        self._sweep_stale_loopbacks()
        with self._lock:
            self._streams = {s["id"]: s for s in list_audio_streams()}
        self._reconcile_all()

    def _do_remove_source(self, source_id):
        with self._lock:
            keys = [
                k for k in self._procs
                if isinstance(k, tuple) and k and k[0] == source_id
            ]
        for k in keys:
            self._destroy_loopback(k)

    @staticmethod
    def _sweep_stale_loopbacks():
        try:
            subprocess.run(
                ["pkill", "-f", "pw-loopback.*openwave_loop_"],
                capture_output=True, timeout=2,
            )
        except (FileNotFoundError, subprocess.SubprocessError):
            return
        time.sleep(0.2)  # give the kernel a beat to reap so we don't race

    # ----- internal -----
    def _reconcile_all(self):
        self._ensure_hp_loopback()
        for source_id in (["mic"] + list(self._sources.keys())):
            for mix_id in MIX_SINKS:
                self._reconcile_cell(source_id, mix_id)

    def _reconcile_cell(self, source_id, mix_id):
        state = self._state.get(
            f"{source_id}.{mix_id}", {"volume": 0.0, "muted": False}
        )
        if source_id == "mic":
            self._reconcile_mic_cell(mix_id, state["volume"], state["muted"])
        else:
            self._reconcile_app_cell(source_id, mix_id, state["volume"], state["muted"])

    def _reconcile_mic_cell(self, mix_id, volume, muted):
        if not self.mic:
            return
        mix_sink = MIX_SINKS.get(mix_id)
        if not mix_sink:
            return
        key = ("mic", mix_id)
        node_name = f"openwave_loop_mic_to_{mix_id}"
        if volume <= 0.0:
            self._destroy_loopback(key)
            return
        if key not in self._procs:
            self._spawn_loopback(key, self.mic, mix_sink, node_name)
        node_id = _node_id_by_name(node_name)
        if node_id is not None:
            _wpctl("set-volume", node_id, f"{volume:.3f}")
            _wpctl("set-mute", node_id, "1" if muted else "0")

    def _reconcile_app_cell(self, source_id, mix_id, volume, muted):
        source = self._sources.get(source_id)
        if not source:
            return
        mix_sink = MIX_SINKS.get(mix_id)
        if not mix_sink:
            return
        match = source.get("match_app_name")
        matching_stream_ids = {
            sid for sid, s in self._streams.items() if s.get("app_name") == match
        }
        existing_keys = {
            k for k in self._procs
            if len(k) == 3 and k[0] == source_id and k[1] == mix_id
        }

        # Tear down loopbacks for streams that vanished or for a zeroed cell
        for k in list(existing_keys):
            if volume <= 0.0 or k[2] not in matching_stream_ids:
                self._destroy_loopback(k)

        if volume <= 0.0:
            return

        # Spawn (or update volume on) loopbacks for each currently-matching stream
        for stream_id in matching_stream_ids:
            key = (source_id, mix_id, stream_id)
            node_name = f"openwave_loop_{source_id}_{mix_id}_{stream_id}"
            stream_node_name = self._streams.get(stream_id, {}).get("node_name", "")
            if not stream_node_name:
                continue
            if key not in self._procs:
                self._spawn_loopback(key, stream_node_name, mix_sink, node_name)
            node_id = _node_id_by_name(node_name)
            if node_id is not None:
                _wpctl("set-volume", node_id, f"{volume:.3f}")
                _wpctl("set-mute", node_id, "1" if muted else "0")
