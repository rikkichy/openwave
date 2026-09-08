"""Real pipe/process regressions; no GTK display or PipeWire server required."""

import importlib.util
from pathlib import Path
import queue
import subprocess
import sys
import threading
import time
import types
import unittest
from unittest import mock


_CHILD = """
import os, signal
signal.signal(signal.SIGTERM, signal.SIG_IGN)
while True:
    data = os.read(0, 4096)
    if not data:
        break
    os.write(1, data)
"""


class MeterLifecycleTests(unittest.TestCase):
    def setUp(self):
        self.pending = queue.Queue()
        glib = types.SimpleNamespace(idle_add=lambda fn, *args: self.pending.put((fn, args)))
        gi = types.ModuleType("gi")
        gi.require_version = lambda *_args: None
        repository = types.ModuleType("gi.repository")
        repository.GLib = glib
        name = "wavexlr._meter_regression"
        spec = importlib.util.spec_from_file_location(
            name, Path(__file__).parents[1] / "wavexlr" / "meter.py")
        self.module = importlib.util.module_from_spec(spec)
        with mock.patch.dict(sys.modules, {
            "gi": gi, "gi.repository": repository, name: self.module,
        }):
            spec.loader.exec_module(self.module)
        self.monitor = self.module.MeterMonitor()
        self.processes = []
        self.real_popen = subprocess.Popen
        self.addCleanup(self.cleanup_processes)
        self.spawn_patch = mock.patch.object(
            self.module.subprocess, "Popen", side_effect=self.spawn)
        self.spawn_patch.start()
        self.addCleanup(self.spawn_patch.stop)

    def spawn(self, _args, **_kwargs):
        proc = self.real_popen(
            [sys.executable, "-u", "-c", _CHILD],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, bufsize=0)
        self.processes.append(proc)
        return proc

    def cleanup_processes(self):
        self.monitor.stop_all()
        stopped = self.monitor.wait_stopped()
        for proc in self.processes:
            if proc.poll() is None:
                proc.kill()
            proc.wait(timeout=2)
            proc.stdin.close()
            proc.stdout.close()
        self.assertTrue(stopped, "meter worker outlived bounded teardown")

    def test_capture_readiness_requires_bytes_from_exact_generation(self):
        self.monitor.start("mic", "capture", lambda _: None, identity=("server", 1))
        self.await_condition(lambda: self.monitor.running("mic"))
        self.assertFalse(self.monitor.ready("capture", ("server", 1)))
        self.processes[-1].stdin.write(bytes(128))
        self.processes[-1].stdin.flush()
        self.await_condition(lambda: self.monitor.ready("capture", ("server", 1)))
        self.monitor.start("mic", "capture", lambda _: None, identity=("server", 2))
        self.assertFalse(self.monitor.ready("capture", ("server", 1)))
        self.assertFalse(self.monitor.ready("capture", ("server", 2)))

    def await_condition(self, predicate):
        deadline = time.monotonic() + 3
        while not predicate():
            if time.monotonic() >= deadline:
                self.fail("timed out waiting for meter event")
            time.sleep(0.005)

    def flush_ui(self):
        while True:
            try:
                fn, args = self.pending.get_nowait()
            except queue.Empty:
                return
            fn(*args)

    def test_replacement_rejects_already_queued_old_peaks_and_final_zero(self):
        old_levels, new_levels = [], []
        self.monitor.start("mic", "old", old_levels.append)
        self.await_condition(lambda: self.monitor.running("mic"))
        old = self.processes[0]
        old.stdin.write(b"\x00\x20" * 512)
        self.await_condition(lambda: not self.pending.empty())

        self.monitor.start("mic", "new", new_levels.append)
        self.await_condition(lambda: len(self.processes) == 2 and self.monitor.running("mic"))
        self.processes[1].stdin.write(b"\x00\x40" * 512)
        self.await_condition(lambda: self.pending.qsize() >= 2)
        self.await_condition(lambda: old.poll() is not None)
        self.flush_ui()
        self.assertEqual(old_levels, [])
        self.assertEqual(new_levels, [0.5])
        self.monitor.stop_all()
        self.assertTrue(self.monitor.wait_stopped())
        self.flush_ui()
        self.assertEqual(new_levels, [0.5])
        for proc in self.processes:
            self.assertIsNotNone(proc.returncode)
            self.assertTrue(proc.stdout.closed)

    def test_suspended_visuals_still_refresh_byte_flow_on_silent_samples(self):
        clock = [10.0]
        self.module.time = types.SimpleNamespace(monotonic=lambda: clock[0])
        levels = []
        self.monitor.start("mic", "capture", levels.append)
        self.await_condition(lambda: self.monitor.running("mic"))
        self.monitor.ui_suspended = True
        clock[0] = 20.0
        self.assertEqual(self.monitor.silent_for("mic"), 10.0)
        self.processes[0].stdin.write(b"\x00\x00" * 512)
        self.await_condition(lambda: self.monitor.silent_for("mic") == 0.0)
        self.flush_ui()
        self.assertEqual(levels, [])
        self.monitor.ui_suspended = False
        self.processes[0].stdin.write(b"\x00\x20" * 512)
        self.await_condition(lambda: not self.pending.empty())
        self.flush_ui()
        self.assertEqual(levels, [0.25])
        self.monitor.stop_all()
        self.assertIsNone(self.monitor.silent_for("mic"))

    def test_one_flowing_tap_disproves_a_shared_capture_stall(self):
        clock = [10.0]
        self.module.time = types.SimpleNamespace(monotonic=lambda: clock[0])
        self.monitor.start("flowing", "capture", lambda _peak: None)
        self.await_condition(lambda: self.monitor.running("flowing"))
        self.monitor.start("stalled", "capture", lambda _peak: None)
        self.await_condition(lambda: self.monitor.running("stalled"))
        clock[0] = 20.0
        self.processes[0].stdin.write(b"\x00\x00" * 512)
        self.await_condition(lambda: self.monitor.capture_gaps() == {"capture": 0.0})
        self.assertEqual(self.monitor.silent_for("stalled"), 10.0)
        self.monitor.stop("flowing")
        self.assertEqual(self.monitor.capture_gaps(), {"capture": 10.0})

    def test_cancel_during_spawn_still_closes_pipe_and_reaps_child(self):
        entered, release = threading.Event(), threading.Event()
        original_spawn = self.spawn

        def held_spawn(*args, **kwargs):
            entered.set()
            if not release.wait(3):
                raise OSError("test failed to release spawn")
            return original_spawn(*args, **kwargs)

        self.spawn_patch.stop()
        with mock.patch.object(self.module.subprocess, "Popen", side_effect=held_spawn):
            self.monitor.start("mic", "capture", lambda _peak: self.fail("late callback"))
            self.assertTrue(entered.wait(3))
            self.monitor.stop_all()
            release.set()
            self.assertTrue(self.monitor.wait_stopped())
        self.flush_ui()
        self.assertFalse(self.monitor.running("mic"))
        proc = self.processes[0]
        self.assertIsNotNone(proc.returncode)
        self.assertTrue(proc.stdout.closed)


if __name__ == "__main__":
    unittest.main()
