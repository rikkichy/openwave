import copy
import math
import os
import struct
import subprocess
import sys
import threading
import time
import unittest
from unittest.mock import patch

from wavexlr import calibrate


def pcm(left, right=None, frames=24000):
    if right is None:
        return struct.pack("<h", left) * frames
    return struct.pack("<hh", left, right) * frames


class CalibrationAnalysisTests(unittest.TestCase):
    def test_malformed_empty_and_inadequate_audio_is_not_a_measurement(self):
        for raw, channels in ((b"", 2), (b"\x00", 1), (b"\x00\x00", 2),
                              (pcm(20, frames=800), 1), (pcm(20), 3), (None, 2)):
            with self.subTest(raw_type=type(raw), channels=channels):
                with self.assertRaises(calibrate.CalibrationError):
                    calibrate.metrics_from_raw(raw, channels)

    def test_all_complete_windows_are_measured_without_stereo_phase_cancellation(self):
        metrics = calibrate.metrics_from_raw(pcm(8192, -8192))
        self.assertEqual(len(metrics["peaks_db"]), 30)
        self.assertAlmostEqual(metrics["peaks_db"][0], -12.041199826559248)
        self.assertEqual(metrics["balance"], 1)
        lopsided = calibrate.metrics_from_raw(pcm(8192, 0))
        self.assertEqual(lopsided["balance"], 0)
        self.assertEqual(lopsided["peaks_db"], metrics["peaks_db"])

    def test_clipping_cannot_hide_in_one_channel(self):
        with self.assertRaisesRegex(calibrate.CalibrationError, "clipping"):
            calibrate.metrics_from_raw(pcm(32767, 0))

    def test_silence_quiet_speech_and_noise_get_actionable_errors(self):
        with self.assertRaisesRegex(calibrate.CalibrationError, "Speech"):
            calibrate.analyze([-140] * 180, [-140] * 300)
        with self.assertRaisesRegex(calibrate.CalibrationError, "quiet"):
            calibrate.analyze([-90] * 180, [-60] * 300)
        with self.assertRaisesRegex(calibrate.CalibrationError, "noise floor"):
            calibrate.analyze([-20] * 180, [-5] * 300)
        with self.assertRaisesRegex(calibrate.CalibrationError, "noise floor"):
            calibrate.analyze([-40] * 180, [-25] * 300)

    def test_corrupt_or_insufficient_levels_do_not_produce_settings(self):
        for values in ([], [-30] * 2, [math.nan] * 30, [math.inf] * 30,
                       [1] * 30, [-141] * 30):
            with self.subTest(values=values[:2]), self.assertRaises(calibrate.CalibrationError):
                calibrate.analyze([-70] * 180, values)
        with self.assertRaisesRegex(calibrate.CalibrationError, "clipping"):
            calibrate.analyze([-70] * 180, [-0.01] * 300)

    def test_proposal_leaves_measurements_and_existing_settings_untouched(self):
        floor, speech = [-65] * 180, [-24] * 270 + [-12] * 30
        original = copy.deepcopy((floor, speech))
        proposal = calibrate.analyze(floor, speech)
        self.assertEqual((floor, speech), original)
        settings = proposal["fx"]
        self.assertGreater(settings["gate_thresh"], -65)
        self.assertLess(settings["gate_thresh"], -24)
        self.assertGreaterEqual(settings["comp_thresh"], -30)
        self.assertLess(settings["comp_thresh"], -12)
        self.assertEqual(settings["comp_ratio"], 3)
        self.assertNotIn("phantom", settings)

    def test_tone_proposal_protects_low_voice_and_bounds_shelf(self):
        floor = {"balance": 1, "sub_db": -2, "voice_low_db": -30, "tilt_db": -15}
        speech = {"balance": 0, "sub_db": -20, "voice_low_db": -8, "tilt_db": -40}
        original = copy.deepcopy((floor, speech))
        result = calibrate.analyze_tone(floor, speech)
        self.assertEqual(result, {"lowcut": 80, "eq_high": 4, "mono": True})
        self.assertEqual((floor, speech), original)
        speech.update(voice_low_db=-20, balance=1, tilt_db=-5)
        self.assertEqual(calibrate.analyze_tone(floor, speech), {"lowcut": 120, "eq_high": -4})
        speech["tilt_db"] = math.nan
        with self.assertRaises(calibrate.CalibrationError):
            calibrate.analyze_tone(floor, speech)


class CaptureLifecycleTests(unittest.TestCase):
    def assert_reaped(self, child):
        self.assertIsNotNone(child.returncode)
        self.assertTrue(child.stdout.closed)
        with self.assertRaises(ChildProcessError):
            os.waitpid(child.pid, os.WNOHANG)

    def test_cancelled_before_spawn_does_not_open_microphone(self):
        with patch.object(calibrate.subprocess, "Popen") as popen:
            with self.assertRaises(calibrate.CalibrationCancelled):
                calibrate.capture_raw("raw_mic", 1, cancel=lambda: True)
            popen.assert_not_called()

    def test_cancellation_during_spawn_kills_and_reaps_real_child(self):
        original_popen = subprocess.Popen
        cancelled = threading.Event()
        children = []

        def spawn(*args, **kwargs):
            # Wait until SIGTERM is ignored, then request cancellation before
            # Popen returns to capture. The real cleanup must escalate to KILL.
            child = original_popen(
                [sys.executable, "-c", "import signal,time; "
                 "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                 "print('ready', flush=True); time.sleep(60)"],
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            )
            children.append(child)
            self.assertEqual(child.stdout.readline(), b"ready\n")
            cancelled.set()
            return child

        try:
            with patch.object(calibrate.subprocess, "Popen", side_effect=spawn):
                with self.assertRaises(calibrate.CalibrationCancelled):
                    calibrate.capture_raw("raw_mic", 1, cancel=cancelled.is_set)
            self.assertLess(children[0].returncode, 0)
            self.assert_reaped(children[0])
        finally:
            for child in children:
                if child.poll() is None:
                    child.kill()
                child.wait()
                child.stdout.close()

    def test_read_cancellation_does_not_wait_for_silent_child(self):
        original_popen = subprocess.Popen
        children = []
        cancelled = threading.Event()
        timer = None

        def spawn(*args, **kwargs):
            nonlocal timer
            child = original_popen([sys.executable, "-c", "import time; time.sleep(60)"],
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            children.append(child)
            timer = threading.Timer(0.1, cancelled.set)
            timer.start()
            return child

        started = time.monotonic()
        try:
            with patch.object(calibrate.subprocess, "Popen", side_effect=spawn):
                with self.assertRaises(calibrate.CalibrationCancelled):
                    calibrate.capture_raw("raw_mic", 5, cancel=cancelled.is_set)
            self.assertLess(time.monotonic() - started, 3)
            self.assert_reaped(children[0])
        finally:
            if timer:
                timer.cancel()
                timer.join()
            for child in children:
                if child.poll() is None:
                    child.kill()
                child.wait()
                child.stdout.close()

    def test_deadline_reaps_real_stalled_capture(self):
        original_popen = subprocess.Popen
        children = []

        def spawn(*args, **kwargs):
            child = original_popen([sys.executable, "-c", "import time; time.sleep(60)"],
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            children.append(child)
            return child

        try:
            with patch.object(calibrate.subprocess, "Popen", side_effect=spawn), \
                    patch.object(calibrate, "GRACE_SECONDS", -1):
                with self.assertRaisesRegex(calibrate.CalibrationError, "incomplete audio"):
                    calibrate.capture_raw("raw_mic", 0.5)
            self.assert_reaped(children[0])
        finally:
            for child in children:
                if child.poll() is None:
                    child.kill()
                child.wait()
                child.stdout.close()

    def test_raw_capture_rejects_processed_and_default_targets(self):
        with patch.object(calibrate.subprocess, "Popen") as popen:
            for target in ("openwave_fx_mic", "0", "-1", "", "raw\0mic"):
                with self.subTest(target=target), self.assertRaises(calibrate.CalibrationError):
                    calibrate.capture_raw(target, 1)
            popen.assert_not_called()


if __name__ == "__main__":
    unittest.main()
