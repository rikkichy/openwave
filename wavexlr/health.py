"""Observe audio health by default; disruptive remedies require explicit opt-in.

No graph-driver changes or profile preferences are imposed. stop() cancels
collection and waits for restoration of any transaction already started.
"""

import json
import logging
import re
import threading
import time

from . import recovery

log = logging.getLogger("wavexlr.health")
CHECK_INTERVAL = 10.0
GLITCH_XRUNS_PER_CHECK = 50
GLITCH_CONFIRM_CHECKS = 2
COOLDOWN_SECONDS = 60.0
MAX_ATTEMPTS = 2
GLITCH_CLEAN_REFILL_CHECKS = 30
STALL_CLEAN_REFILL_CHECKS = 6


def sample_xruns(runner=None):
    runner = runner or recovery.CommandRunner()
    text = runner.run(["pw-top", "--batch-mode", "--iterations", "3"], timeout=15)
    return _parse_pw_top(text) if text is not None else {}


def _parse_pw_top(text):
    counts = {}
    for line in text.splitlines():
        tokens = line.split()
        # S ID QUANT RATE WAIT BUSY W/Q B/Q ERR [FORMAT...] NAME
        if len(tokens) < 10 or not tokens[8].isdigit():
            continue
        # Later iterations overwrite earlier ones: last wins.
        counts[tokens[-1]] = int(tokens[8])
    return counts


def read_playback_status(card, device, subdevice):
    """(hw_ptr, state) of one ALSA playback substream, or (None, None).

    hw_ptr is the DMA position the hardware has consumed up to. A
    running playback stream always advances it — even one playing pure
    silence — which is what makes a static pointer a hardware verdict
    rather than a signal-level one.
    """
    path = (f"/proc/asound/card{card}/pcm{device}p/"
            f"sub{subdevice}/status")
    try:
        with open(path) as f:
            text = f.read()
    except OSError:
        return None, None
    ptr = re.search(r"^hw_ptr\s*:\s*(\d+)", text, re.MULTILINE)
    state = re.search(r"^state:\s*(\S+)", text, re.MULTILINE)
    return (int(ptr.group(1)) if ptr else None,
            state.group(1) if state else None)


def sample_source_mutes(runner=None):
    """Unknown mute state cannot enable recovery."""
    sources = recovery._listing("sources", runner)
    return {source["name"]: source["mute"] for source in sources
            if isinstance(source.get("name"), str) and isinstance(source.get("mute"), bool)}


def recycle_sink(sink_name, runner=None):
    """Resume even if cancellation or timeout interrupts suspension."""
    if runner and runner.stopped.is_set():
        return False
    suspended = False
    try:
        suspended = recovery._pactl("suspend-sink", sink_name, "1") is not None
        if suspended:
            (runner.stopped if runner else threading.Event()).wait(1.0)
    finally:
        resumed = recovery._pactl("suspend-sink", sink_name, "0") is not None
        if not resumed:
            log.error("Could not resume %s after suspension", sink_name)
    return suspended and resumed


def snapshot_graph(runner=None):
    """Return {capture: running}, watched sinks; None means unknown graph."""
    from .audio import SOURCE_MATCHES
    runner = runner or recovery.CommandRunner()
    text = runner.run(["pw-dump"])
    try:
        dump = json.loads(text)
    except (ValueError, TypeError):
        return None
    if not isinstance(dump, list):
        return None
    nodes = {}
    targets = set()
    captures = {}
    for obj in dump:
        if not isinstance(obj, dict) or obj.get("type") != "PipeWire:Interface:Node":
            continue
        info = obj.get("info") or {}
        props = info.get("props") or {}
        name = props.get("node.name", "")
        nodes[name] = (info, props)
        if name.startswith(SOURCE_MATCHES):
            captures[name] = info.get("state") == "running"
        if name.startswith("openwave_loop_out") and not name.endswith("_cap"):
            target = props.get("target.object")
            if isinstance(target, str) and target.startswith("alsa_output."):
                targets.add(target)
    sinks = {}
    for name in targets:
        if name not in nodes:
            continue
        info, props = nodes[name]
        try:
            sinks[name] = {
                "running": info.get("state") == "running",
                "card": int(props["alsa.card"]),
                "device": int(props["alsa.device"]),
                "subdevice": int(props.get("alsa.subdevice", 0)),
            }
        except (KeyError, TypeError, ValueError):
            continue
    return captures, sinks


class GlitchWatch:
    """Decides when a capture node's xrun counter means glitching audio.

    Fed one cumulative count per check window. The counter resets to
    zero when a node is recreated; a shrinking count therefore starts a
    fresh baseline instead of celebrating a negative delta.
    """

    def __init__(self, threshold=GLITCH_XRUNS_PER_CHECK,
                 confirm=GLITCH_CONFIRM_CHECKS,
                 cooldown_seconds=COOLDOWN_SECONDS,
                 max_attempts=MAX_ATTEMPTS,
                 clean_refill=GLITCH_CLEAN_REFILL_CHECKS):
        self.threshold = threshold
        self.confirm = confirm
        self.cooldown_seconds = cooldown_seconds
        self.max_attempts = max_attempts
        self.clean_refill = clean_refill
        self._prev = {}          # node_name -> last cumulative count
        self._streak = {}        # node_name -> consecutive bad windows
        self._clean = {}         # node_name -> consecutive clean windows
        self._delta = {}         # node_name -> xruns in the last window
        self._attempts = {}      # node_name -> remedies spent
        self._last_attempt = {}  # node_name -> monotonic time

    def forget(self, node_name):
        for d in (self._prev, self._streak, self._clean, self._delta,
                  self._attempts, self._last_attempt):
            d.pop(node_name, None)

    def pause(self, node_name):
        """Unknown, idle or muted samples break continuity, not remedy budgets."""
        for state in (self._prev, self._streak, self._clean, self._delta):
            state.pop(node_name, None)

    def observe(self, node_name, xruns, now):
        """Account one window; True when that window was glitchy."""
        prev = self._prev.get(node_name)
        self._prev[node_name] = xruns
        if prev is None or xruns < prev:
            # First sight, or the node was recreated: baseline only.
            # Deliberately not a clean window — the reset after a card
            # cycle proves nothing about the fault.
            self._streak[node_name] = 0
            self._clean[node_name] = 0
            self._delta[node_name] = 0
            return False
        self._delta[node_name] = xruns - prev
        if xruns - prev >= self.threshold:
            self._streak[node_name] = self._streak.get(node_name, 0) + 1
            self._clean[node_name] = 0
            return True
        # One clean window is not recovery — a card cycle buys a quiet
        # window while the capture reopens, and refilling on it turns a
        # persistent fault into an endless cycle-pop loop. The budget
        # refills only after a sustained stretch of quiet.
        self._streak[node_name] = 0
        clean = self._clean.get(node_name, 0) + 1
        self._clean[node_name] = clean
        if clean >= self.clean_refill:
            self._attempts.pop(node_name, None)
        return False

    def glitching(self, node_name):
        return self._streak.get(node_name, 0) >= self.confirm

    def just_confirmed(self, node_name):
        """True exactly once per incident, when it crosses `confirm`."""
        return self._streak.get(node_name, 0) == self.confirm

    def last_delta(self, node_name):
        """xruns accumulated in the last observed window, for logging."""
        return self._delta.get(node_name, 0)

    def spent(self, node_name):
        """Remedy attempts spent on the current incident."""
        return self._attempts.get(node_name, 0)

    def should_recover(self, node_name, now):
        if not self.glitching(node_name):
            return False
        if self._attempts.get(node_name, 0) >= self.max_attempts:
            return False
        last = self._last_attempt.get(node_name)
        if last is not None and now - last < self.cooldown_seconds:
            return False
        return True

    def record_attempt(self, node_name, now):
        self._attempts[node_name] = self._attempts.get(node_name, 0) + 1
        self._last_attempt[node_name] = now


class SinkStallWatch:
    """Decides when a running sink's hardware has stopped consuming.

    Fed (running, hw_ptr, alsa_state) per check window. A stall is a
    pointer that did not move between two windows while the node claims
    to be running, or the kernel reporting the stream in XRUN. The first
    observation of a sink only baselines the pointer — a sink that just
    started gets a full window before being judged.
    """

    def __init__(self, cooldown_seconds=COOLDOWN_SECONDS,
                 max_attempts=MAX_ATTEMPTS,
                 clean_refill=STALL_CLEAN_REFILL_CHECKS):
        self.cooldown_seconds = cooldown_seconds
        self.max_attempts = max_attempts
        self.clean_refill = clean_refill
        self._prev_ptr = {}      # sink_name -> last hw_ptr
        self._stalled = {}       # sink_name -> bool
        self._was_stalled = {}   # sink_name -> stalled on previous window
        self._clean = {}         # sink_name -> consecutive moving windows
        self._attempts = {}      # sink_name -> remedies spent
        self._last_attempt = {}  # sink_name -> monotonic time

    def forget(self, sink_name):
        for d in (self._prev_ptr, self._stalled, self._was_stalled,
                  self._clean, self._attempts, self._last_attempt):
            d.pop(sink_name, None)

    def observe(self, sink_name, running, hw_ptr, alsa_state, now):
        """Account one window; True when the sink is stalled."""
        self._was_stalled[sink_name] = self._stalled.get(sink_name, False)
        prev = self._prev_ptr.get(sink_name)
        self._prev_ptr[sink_name] = hw_ptr
        if not running or hw_ptr is None or alsa_state not in ("RUNNING", "XRUN"):
            # Idle and suspended sinks legitimately hold still, and a
            # sink whose /proc entry vanished is not ours to judge.
            self._stalled[sink_name] = False
            self._prev_ptr.pop(sink_name, None)
            self._clean[sink_name] = 0
            return False
        if alsa_state == "XRUN":
            self._stalled[sink_name] = True
            self._clean[sink_name] = 0
            return True
        if prev is None:
            self._stalled[sink_name] = False
            return False
        stalled = hw_ptr == prev
        self._stalled[sink_name] = stalled
        if stalled:
            self._clean[sink_name] = 0
        else:
            # Same reasoning as the glitch watch, shorter leash: a
            # recycle resets the pointer and the next window can move
            # once without the PCM being healthy, so refill only after
            # a sustained stretch of movement.
            clean = self._clean.get(sink_name, 0) + 1
            self._clean[sink_name] = clean
            if clean >= self.clean_refill:
                self._attempts.pop(sink_name, None)
        return stalled

    def just_stalled(self, sink_name):
        """True on the window a stall begins, for logging it once."""
        return (self._stalled.get(sink_name, False)
                and not self._was_stalled.get(sink_name, False))

    def spent(self, sink_name):
        """Remedy attempts spent on the current incident."""
        return self._attempts.get(sink_name, 0)

    def should_recover(self, sink_name, now):
        if not self._stalled.get(sink_name):
            return False
        if self._attempts.get(sink_name, 0) >= self.max_attempts:
            return False
        last = self._last_attempt.get(sink_name)
        if last is not None and now - last < self.cooldown_seconds:
            return False
        return True

    def record_attempt(self, sink_name, now):
        self._attempts[sink_name] = self._attempts.get(sink_name, 0) + 1
        self._last_attempt[sink_name] = now
        # The recycle itself resets the pointer; don't let the next
        # window compare against a pre-recycle value.
        self._prev_ptr.pop(sink_name, None)


class HealthMonitor:
    """Log confirmed faults; only auto_recover=True permits remedies."""

    def __init__(self, auto_recover=False):
        self.auto_recover = auto_recover
        self._stop = threading.Event()
        self._runner = recovery.CommandRunner(self._stop)
        self._thread = None
        self._lifecycle = threading.Lock()
        self._check_lock = threading.Lock()
        self.glitch = GlitchWatch()
        self.stall = SinkStallWatch()
        self._known_captures = set()
        self._known_sinks = set()

    def start(self):
        with self._lifecycle:
            if self._thread is not None and self._thread.is_alive():
                return
            self._stop.clear()
            self._thread = threading.Thread(target=self._run, daemon=True,
                                            name="openwave-health")
            self._thread.start()

    def stop(self):
        """Cancel/reap collection and wait for any owed restoration.

        No timed join: a fifteen-second sampler cannot outlive shutdown.
        Also waits for callers driving check_once directly.
        """
        with self._lifecycle:
            self._runner.cancel()
            if self._thread is not None:
                self._thread.join()
                self._thread = None
            with self._check_lock:
                pass

    def check_once(self, now=None):
        with self._check_lock:
            if self._stop.is_set():
                return
            self._check_once(time.monotonic() if now is None else now)

    def _check_once(self, now):
        graph = snapshot_graph(self._runner)
        if graph is None or self._stop.is_set():
            for name in self._known_captures:
                self.glitch.pause(name)
            for name in self._known_sinks:
                self.stall.observe(name, False, None, None, now)
            return
        captures, sinks = graph
        for gone in self._known_captures - set(captures):
            self.glitch.forget(gone)
        for gone in self._known_sinks - set(sinks):
            self.stall.forget(gone)
        self._known_captures = set(captures)
        self._known_sinks = set(sinks)
        if captures:
            counts = sample_xruns(self._runner)
            mutes = sample_source_mutes(self._runner)
            for name, running in captures.items():
                if self._stop.is_set():
                    return
                if not running or name not in counts or mutes.get(name) is not False:
                    self.glitch.pause(name)
                    continue
                self.glitch.observe(name, counts[name], now)
                if self.glitch.just_confirmed(name):
                    log.warning("%s has sustained capture xruns (%d/window); auto-recover %s",
                                name, self.glitch.last_delta(name), self.auto_recover)
                if self.auto_recover and self.glitch.should_recover(name, now):
                    card = recovery.card_name_for(name, self._runner)
                    if card and not self._stop.is_set():
                        self.glitch.record_attempt(name, now)
                        recovered = recovery.cycle_card(card, self._runner)
                        log.warning("Card recovery for %s: %s (attempt %d/%d)",
                                    name, recovered, self.glitch.spent(name),
                                    self.glitch.max_attempts)
        sink_mutes = {item["name"]: item["mute"]
                      for item in recovery._listing("sinks", self._runner)
                      if isinstance(item.get("name"), str) and isinstance(item.get("mute"), bool)} if sinks else {}
        for name, sink in sinks.items():
            if self._stop.is_set():
                return
            ptr, state = read_playback_status(sink["card"], sink["device"], sink["subdevice"])
            running = sink["running"] and sink_mutes.get(name) is False
            self.stall.observe(name, running, ptr, state, now)
            if self.stall.just_stalled(name):
                log.warning("%s playback hardware is stalled; auto-recover %s",
                            name, self.auto_recover)
            if self.auto_recover and self.stall.should_recover(name, now):
                if self._stop.is_set():
                    return
                self.stall.record_attempt(name, now)
                recovered = recycle_sink(name, self._runner)
                log.warning("Sink recovery for %s: %s (attempt %d/%d)",
                            name, recovered, self.stall.spent(name), self.stall.max_attempts)

    def _run(self):
        while not self._stop.is_set():
            try:
                self.check_once()
            except Exception:
                log.exception("Health monitor collection failed")
            self._stop.wait(CHECK_INTERVAL)
