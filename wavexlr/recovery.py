"""Explicit, bounded recovery; never select a profile on the user's behalf.

Callers must opt in before invoking a remedy. Collection is cancellable;
restoring a profile already switched off is mandatory even during shutdown.
"""

import json
import logging
import subprocess
import threading

STALL_SECONDS = 8.0
COOLDOWN_SECONDS = 60.0
MAX_ATTEMPTS = 2
CLEAN_REFILL_SECONDS = 300.0
log = logging.getLogger(__name__)


class CommandRunner:
    """One collector's child process, cancelled and reaped before returning."""

    def __init__(self, stopped=None):
        self.stopped = stopped if stopped is not None else threading.Event()
        self._lock = threading.Lock()
        self._process = None

    def cancel(self):
        with self._lock:
            self.stopped.set()
            if self._process is not None:
                try:
                    self._process.kill()
                except ProcessLookupError:
                    pass

    def run(self, argv, timeout=5):
        with self._lock:
            if self.stopped.is_set():
                return None
            try:
                process = subprocess.Popen(argv, stdout=subprocess.PIPE,
                                           stderr=subprocess.PIPE, text=True)
            except OSError:
                return None
            self._process = process
        try:
            try:
                stdout, _ = process.communicate(timeout=timeout)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate()
                return None
            if self.stopped.is_set() or process.returncode:
                return None
            return stdout
        finally:
            with self._lock:
                self._process = None


def _pactl(*args, timeout=5, runner=None):
    if runner is not None:
        return runner.run(["pactl", *args], timeout=timeout)
    try:
        result = subprocess.run(["pactl", *args], capture_output=True,
                                text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError):
        return None
    return result.stdout if result.returncode == 0 else None


def _listing(kind, runner=None):
    out = _pactl("--format=json", "list", kind, runner=runner)
    try:
        value = json.loads(out)
    except (ValueError, TypeError):
        return []
    return [item for item in value if isinstance(item, dict)] if isinstance(value, list) else []


def card_name_for(node_name, runner=None):
    """Resolve an exact source/sink name via its card index, never a prefix."""
    if not node_name or not node_name.startswith(("alsa_input.", "alsa_output.")):
        return None
    kind = "sources" if node_name.startswith("alsa_input.") else "sinks"
    matches = [item for item in _listing(kind, runner) if item.get("name") == node_name]
    if len(matches) != 1 or matches[0].get("card") is None:
        return None
    index = str(matches[0]["card"])
    cards = [item for item in _listing("cards", runner)
             if str(item.get("index")) == index]
    if len(cards) == 1 and isinstance(cards[0].get("name"), str):
        return cards[0]["name"]
    return None


def active_profile(card_name, runner=None):
    """Read the exact card's original profile from locale-independent JSON."""
    cards = [card for card in _listing("cards", runner) if card.get("name") == card_name]
    if len(cards) != 1:
        return None
    profile = cards[0].get("active_profile")
    if isinstance(profile, dict):
        profile = profile.get("name")
    return profile if isinstance(profile, str) and profile else None


def cycle_card(card_name, runner=None):
    """Close/reopen a card, always attempting restoration of its original profile.

    Cancellation may abort discovery, but not half of a transaction. The
    monitor waits for this bounded transaction before stop() returns.
    """
    profile = active_profile(card_name, runner)
    if not profile or profile == "off" or (runner and runner.stopped.is_set()):
        return False
    switched = False
    try:
        switched = _pactl("set-card-profile", card_name, "off") is not None
    finally:
        # A timeout may have applied the off command: restore even on failure.
        restored = _pactl("set-card-profile", card_name, profile) is not None
        if not restored:
            log.error("Could not restore %s to original profile %s", card_name, profile)
    return switched and restored


class StallWatch:
    """Pure no-data capture decision; silence/mute is not evidence of a stall."""

    def __init__(self, stall_seconds=STALL_SECONDS,
                 cooldown_seconds=COOLDOWN_SECONDS, max_attempts=MAX_ATTEMPTS,
                 clean_refill_seconds=CLEAN_REFILL_SECONDS):
        self.stall_seconds = stall_seconds
        self.cooldown_seconds = cooldown_seconds
        self.max_attempts = max_attempts
        self.clean_refill_seconds = clean_refill_seconds
        self._attempts = {}
        self._last_attempt = {}
        self._clean_since = {}

    def forget(self, node_name):
        for state in (self._attempts, self._last_attempt, self._clean_since):
            state.pop(node_name, None)

    def should_recover(self, node_name, node_present, silent_for, now):
        if not node_present or node_name is None:
            self._clean_since.pop(node_name, None)
            return False
        if silent_for is None:
            self._clean_since.pop(node_name, None)
            return False
        if silent_for < self.stall_seconds:
            return False
        self._clean_since.pop(node_name, None)
        if self._attempts.get(node_name, 0) >= self.max_attempts:
            return False
        last = self._last_attempt.get(node_name)
        return last is None or now - last >= self.cooldown_seconds

    def record_attempt(self, node_name, now):
        self._attempts[node_name] = self._attempts.get(node_name, 0) + 1
        self._last_attempt[node_name] = now
        self._clean_since.pop(node_name, None)

    def record_recovered(self, node_name, now):
        """Account sustained flowing audio, not one lucky frame after cycling."""
        since = self._clean_since.setdefault(node_name, now)
        if now - since >= self.clean_refill_seconds:
            self.forget(node_name)
