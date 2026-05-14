"""Audio mixer — manages pw-loopback subprocesses for the matrix.

A loopback exists for each non-zero cell in the matrix (mic → mix), plus one
that always routes Personal Mix → Wave XLR headphones so the user hears
anything routed there. Volume + mute per cell are pushed onto the loopback's
playback node via wpctl.

State is persisted to ~/.config/openwave/mixes.json so per-cell levels survive
restarts (the loopbacks themselves do not — they're respawned by start()).
"""

import json
import os
import subprocess
import time
from threading import Lock

CONFIG_PATH = os.path.expanduser("~/.config/openwave/mixes.json")

MIX_SINKS = {
    "personal": "openwave_personal_mix",
    "chat":     "openwave_chat_mix",
    "record":   "openwave_record_mix",
}
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
         if len(p) > 1 and p[1].startswith("alsa_input") and "Wave_XLR" in p[1]),
        None,
    )
    hp = next(
        (p[1] for p in _pactl_short("sinks")
         if len(p) > 1 and p[1].startswith("alsa_output") and "Wave_XLR" in p[1]),
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


class Mixer:
    """Manages pw-loopback subprocesses for the matrix's mic row."""

    def __init__(self):
        self._lock = Lock()
        self._procs = {}
        self._state = self._load_state()
        self.mic, self.hp = find_wave_xlr_alsa()

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

    # ----- subprocess lifecycle -----
    def _spawn_loopback(self, key, capture_target, playback_target, node_name):
        if key in self._procs:
            return
        try:
            proc = subprocess.Popen(
                [
                    "pw-loopback",
                    f"--capture-props=target.object={capture_target}",
                    f"--playback-props=target.object={playback_target} node.name={node_name}",
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except (FileNotFoundError, OSError):
            return
        self._procs[key] = proc

    def _destroy_loopback(self, key):
        proc = self._procs.pop(key, None)
        if proc is None:
            return
        try:
            proc.terminate()
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()

    # ----- public API -----
    def start(self):
        """Spawn the always-on Personal Mix → HP loopback and restore non-zero cells."""
        with self._lock:
            if self.hp:
                self._spawn_loopback(
                    HP_LOOPBACK_KEY, PERSONAL_MIX_SINK, self.hp, HP_LOOPBACK_NODE,
                )
            for cell_key, state in list(self._state.items()):
                source_id, _, mix_id = cell_key.partition(".")
                if not mix_id:
                    continue
                self._apply_cell(
                    source_id, mix_id,
                    state.get("volume", 0.0), state.get("muted", False),
                )

    def stop(self):
        with self._lock:
            for key in list(self._procs.keys()):
                self._destroy_loopback(key)

    def set_cell(self, source_id, mix_id, volume, muted):
        volume = max(0.0, min(1.0, float(volume)))
        with self._lock:
            self._state[f"{source_id}.{mix_id}"] = {
                "volume": volume, "muted": bool(muted),
            }
            self._save_state()
            self._apply_cell(source_id, mix_id, volume, muted)

    # ----- internal -----
    def _apply_cell(self, source_id, mix_id, volume, muted):
        # Phase 2b: only the mic source is wired.
        if source_id != "mic" or not self.mic:
            return
        mix_sink = MIX_SINKS.get(mix_id)
        if not mix_sink:
            return
        key = (source_id, mix_id)
        node_name = f"openwave_loop_{source_id}_to_{mix_id}"

        if volume <= 0.0:
            self._destroy_loopback(key)
            return

        if key not in self._procs:
            self._spawn_loopback(key, self.mic, mix_sink, node_name)

        node_id = _node_id_by_name(node_name)
        if node_id is None:
            return
        _wpctl("set-volume", node_id, f"{volume:.3f}")
        _wpctl("set-mute", node_id, "1" if muted else "0")
