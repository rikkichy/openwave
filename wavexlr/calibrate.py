"""Raw microphone measurements and proposals, never automatic source updates.

Call capture_raw from a worker thread, not the GTK thread. Cancellation is a
callable (for example threading.Event.is_set); no mixer or GTK dependency exists.
"""

import math
import os
import select
import struct
import subprocess
import time
from collections.abc import Mapping

from .effects import FX_NODE_PREFIX, fx


RATE = 48000
WINDOW = 1600  # 800 signed-16 samples: 16.7 ms at 48 kHz
FLOOR_SECONDS = 3
SPEECH_SECONDS = 5
GRACE_SECONDS = 3
_POLL_SECONDS = 0.1
_MIN_WINDOWS = 30


class CalibrationError(Exception):
    """The input did not support a safe calibration proposal."""


class CalibrationCancelled(Exception):
    """The caller cancelled capture; its child has been reaped."""


def _check_cancel(cancel):
    if cancel is not None and cancel():
        raise CalibrationCancelled()


def _channels(channels):
    if type(channels) is not int or channels not in (1, 2):
        raise CalibrationError("recording must have one or two channels")


def _read_exactly(proc, budget, deadline, cancel):
    chunks, got = [], 0
    while got < budget:
        _check_cancel(cancel)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        ready, _, _ = select.select([proc.stdout], [], [], min(_POLL_SECONDS, remaining))
        if not ready:
            continue
        chunk = os.read(proc.stdout.fileno(), min(65536, budget - got))
        if not chunk:
            break
        chunks.append(chunk)
        got += len(chunk)
    _check_cancel(cancel)
    return b"".join(chunks)


def _stop_capture(proc):
    """Terminate, escalate, and unconditionally reap our owned direct child."""
    try:
        if proc.poll() is None:
            try:
                proc.terminate()
            except ProcessLookupError:
                pass
            try:
                proc.wait(timeout=0.5)
            except subprocess.TimeoutExpired:
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
    finally:
        # After SIGKILL, wait without abandoning the child on another timeout.
        # Also reaps an already exited child and closes stdout on every path.
        try:
            proc.wait()
        finally:
            if proc.stdout is not None:
                proc.stdout.close()


def _capture(node_name, seconds, channels, cancel):
    """Record requested audio plus a half-second startup transient."""
    frame = 2 * channels
    budget = int(RATE * seconds) * frame + RATE * frame // 2
    proc = None
    _check_cancel(cancel)
    deadline = time.monotonic() + seconds + 0.5 + GRACE_SECONDS
    try:
        # No preexec_fn: this is called in threaded Python. No new session or
        # detached process; ownership starts as soon as Popen returns, including
        # when cancellation was requested during process creation.
        proc = subprocess.Popen(
            ["pw-cat", "--record", "--target", node_name,
             "--rate", str(RATE), "--channels", str(channels),
             "--format", "s16", "--properties",
             '{ "media.name": "openwave_calibration", '
             '"node.name": "openwave_calibration", '
             '"application.name": "OpenWave", "node.dont-reconnect": true, '
             '"node.dont-fallback": true, "node.dont-move": true }', "-"],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        )
        return _read_exactly(proc, budget, deadline, cancel)
    except OSError as exc:
        raise CalibrationError(f"could not record the raw microphone: {exc}") from exc
    finally:
        if proc is not None:
            _stop_capture(proc)


def capture_raw(node_name, seconds, channels=2, cancel=None):
    """Return exactly the requested raw little-endian s16 audio, transient dropped.

    Pass the physical source's node_name, never its processed FX node. A missing
    or stalled target fails rather than switching to the default microphone.
    Cancellation during spawn/read always tears down and reaps the direct child.
    """
    _channels(channels)
    if (not isinstance(node_name, str) or not node_name or "\0" in node_name
            or node_name in ("0", "-1") or node_name.startswith(FX_NODE_PREFIX)):
        raise CalibrationError("select a raw microphone node for calibration")
    if not _finite(seconds) or not 0.5 <= seconds <= 60:
        raise CalibrationError("capture duration must be between 0.5 and 60 seconds")
    frame = channels * 2
    expected = int(RATE * seconds) * frame
    raw = _capture(node_name, seconds, channels, cancel)[RATE * frame // 2:]
    if len(raw) != expected:
        raise CalibrationError(
            "The microphone delivered incomplete audio; check that it is connected "
            "and not suspended, then try again.")
    _check_cancel(cancel)
    return raw


def _one_pole_energy(samples, cutoff):
    a = math.exp(-2.0 * math.pi * cutoff / RATE)
    b = 1.0 - a
    y = acc = 0.0
    for sample in samples:
        y = b * sample + a * y
        acc += y * y
    return acc / len(samples)


def metrics_from_raw(raw, channels=2):
    """Measure levels, tone, and stereo balance from complete s16 frames.

    Levels use the louder channel per window; tone uses the higher-energy channel.
    Averaging first would hide clipping or cancel opposite-polarity stereo input.
    """
    _channels(channels)
    if not isinstance(raw, (bytes, bytearray)) or len(raw) % (2 * channels):
        raise CalibrationError("malformed audio: expected complete signed-16 PCM frames")
    half = WINDOW // 2
    frames = len(raw) // (2 * channels)
    if frames < half * _MIN_WINDOWS:
        raise CalibrationError("not enough audio was measured; record at least half a second")
    ints = struct.unpack(f"<{frames * channels}h", raw)
    clipped = sum(abs(sample) >= 32760 for sample in ints)
    if clipped >= max(3, len(ints) * 0.001):
        raise CalibrationError("The microphone is clipping; lower its hardware gain and try again.")
    tracks = [ints[channel::channels] for channel in range(channels)]
    energies = [sum(sample * sample for sample in track) / frames for track in tracks]
    total = max(energies)
    tone = tracks[energies.index(total)]
    balance = min(energies) / total if total else 1.0
    peaks = []
    for start in range(0, frames - half + 1, half):
        peak = max(max(abs(sample) for sample in track[start:start + half])
                   for track in tracks) / 32768.0
        peaks.append(20 * math.log10(max(peak, 1e-7)))
    e90 = _one_pole_energy(tone, 90)
    e180 = _one_pole_energy(tone, 180)
    e2k = _one_pole_energy(tone, 2000)

    def db(energy):
        return 10 * math.log10(max(energy, 1e-9) / max(total, 1e-9))

    return {"peaks_db": peaks, "balance": balance,
            "sub_db": db(e90), "voice_low_db": db(e180 - e90),
            "tilt_db": db(total - e2k)}


def _finite(value):
    try:
        return not isinstance(value, bool) and isinstance(value, (int, float)) and math.isfinite(value)
    except OverflowError:
        return False


def analyze_tone(floor_metrics, speech_metrics):
    """Propose bounded low cut, shelf, and optional mono; never modify input."""
    for metrics in (floor_metrics, speech_metrics):
        if not isinstance(metrics, Mapping):
            raise CalibrationError("malformed tone measurements")
        for key in ("balance", "sub_db", "voice_low_db", "tilt_db"):
            if key not in metrics or not _finite(metrics[key]):
                raise CalibrationError("malformed tone measurements")
        if not 0 <= metrics["balance"] <= 1:
            raise CalibrationError("malformed channel balance")
    deep_voice = speech_metrics["voice_low_db"] > -12.0
    rumbly_floor = floor_metrics["sub_db"] > -6.0
    proposal = {
        "lowcut": 80 if deep_voice else (120 if rumbly_floor else 80),
        "eq_high": float(max(-4.0, min(4.0, round((-15.0 - speech_metrics["tilt_db"]) * 0.5)))),
    }
    if speech_metrics["balance"] < 0.05:
        proposal["mono"] = True
    return proposal


def _percentile(values, pct):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(len(ordered) * pct / 100))]


def analyze(floor_peaks_db, speech_peaks_db):
    """Return measured levels and a gate/compressor proposal, without applying it."""
    for peaks in (floor_peaks_db, speech_peaks_db):
        if not isinstance(peaks, (list, tuple)) or len(peaks) < _MIN_WINDOWS:
            raise CalibrationError("not enough audio was measured; repeat both recording phases")
        if any(not _finite(peak) or not -140 <= peak <= 0 for peak in peaks):
            raise CalibrationError("malformed audio level measurements")
    if max(speech_peaks_db) >= -0.1 or max(floor_peaks_db) >= -0.1:
        raise CalibrationError("The microphone is clipping; lower its hardware gain and try again.")
    floor = _percentile(floor_peaks_db, 50)
    if floor > -25:
        raise CalibrationError("The noise floor is too high; reduce room noise or hardware gain and try again.")
    voiced = [peak for peak in speech_peaks_db if peak > floor + 10]
    if len(voiced) < max(_MIN_WINDOWS, len(speech_peaks_db) * 0.1):
        raise CalibrationError(
            "Speech was not clearly above the noise floor; speak longer and closer to the microphone.")
    quiet_voice = _percentile(voiced, 10)
    loud_voice = _percentile(voiced, 90)
    if loud_voice < -45 or quiet_voice < -58:
        raise CalibrationError("Speech is too quiet; move closer or increase hardware gain, then try again.")
    if quiet_voice - floor < 18:
        raise CalibrationError("Speech is too close to the noise floor; reduce room noise or move closer.")
    gate_thresh = max(-70.0, min(-20.0, floor + 8.0, quiet_voice - 6.0))
    proposal = {"gate": True, "gate_thresh": round(gate_thresh, 1),
                "comp": True, "comp_thresh": round(loud_voice - 6.0, 1),
                "comp_ratio": 3.0}
    validated = fx({"fx": proposal})
    return {
        "measured": {"floor_db": round(floor, 1),
                     "quiet_voice_db": round(quiet_voice, 1),
                     "loud_voice_db": round(loud_voice, 1)},
        "fx": {key: validated[key] for key in proposal},
    }
