"""The watchdogs for faults every byte-level check passes.

Both decisions are pure so they can be tested without a sound card, and
both mistakes are silent: missing the fault leaves robotic or inaudible
audio that every layer reports as healthy, and acting too eagerly cycles
hardware underneath someone who is using it.
"""

import threading
import unittest
from unittest import mock

from wavexlr import health


DOCK = ("alsa_input.usb-Elgato_Systems_Elgato_XLR_Dock_A8A9A40411NOP9-00"
        ".mono-fallback")
SINK = "alsa_output.usb-SteelSeries_Arctis_Nova_Pro_Wireless-00.analog-stereo"


# Real pw-top output shapes, including the quirks the parser must
# survive: the header, '---' placeholder rows, '???' warmup ratios, the
# FORMAT column being present, absent, or three tokens wide, and the
# first iteration printing zeros before the profiler warms up.
PW_TOP_OUTPUT = f"""\
S   ID  QUANT   RATE    WAIT    BUSY   W/Q   B/Q  ERR FORMAT           NAME
C   73      0      0    ---     ---   ---   ---     0                  {DOCK}
R   73      0      0   0.0us   0.0us  ???   ???     0    S24LE 1 48000 {DOCK}
S   ID  QUANT   RATE    WAIT    BUSY   W/Q   B/Q  ERR FORMAT           NAME
R   73      0      0  12.3us   4.2us  0.00  0.00  30367    S24LE 1 48000  + {DOCK}
R  199      0      0   1.2us   7.4us  0.00  0.00    5         F32P 1 0  + openwave_fx_2f216c26f5e3
"""


class ParsingPwTop(unittest.TestCase):
    def test_the_last_iteration_wins(self):
        counts = health._parse_pw_top(PW_TOP_OUTPUT)
        self.assertEqual(counts[DOCK], 30367)

    def test_every_printed_node_is_counted(self):
        counts = health._parse_pw_top(PW_TOP_OUTPUT)
        self.assertEqual(counts["openwave_fx_2f216c26f5e3"], 5)

    def test_headers_and_placeholder_rows_do_not_crash_or_count(self):
        counts = health._parse_pw_top(PW_TOP_OUTPUT)
        self.assertNotIn("NAME", counts)
        self.assertNotIn("FORMAT", counts)


class GlitchDeciding(unittest.TestCase):
    def setUp(self):
        self.w = health.GlitchWatch(
            threshold=50, confirm=2, cooldown_seconds=60, max_attempts=2)

    def feed(self, counts, start=0.0, step=10.0):
        verdicts = []
        for i, c in enumerate(counts):
            verdicts.append(self.w.observe(DOCK, c, start + i * step))
        return verdicts

    def test_first_sight_only_baselines(self):
        """A node first seen with a huge historical count has not been
        observed glitching — the count could be weeks old."""
        self.assertEqual(self.feed([61994]), [False])
        self.assertFalse(self.w.glitching(DOCK))

    def test_a_flat_counter_is_healthy(self):
        self.feed([100, 100, 102, 102])
        self.assertFalse(self.w.glitching(DOCK))

    def test_the_robotic_fault_is_confirmed_in_two_windows(self):
        # ~23 xruns/s over 10 s windows, as measured on hardware.
        self.feed([0, 230, 460])
        self.assertTrue(self.w.glitching(DOCK))
        self.assertTrue(self.w.should_recover(DOCK, now=100.0))

    def test_one_burst_is_an_event_not_a_state(self):
        """A single bad window (game launch, compile) must not cycle a
        card someone is speaking into."""
        self.feed([0, 230, 235])
        self.assertFalse(self.w.glitching(DOCK))

    def test_a_wireless_followers_own_jitter_stays_below_threshold(self):
        # The Arctis was observed bursting 23 in one window while healthy.
        self.feed([238, 261, 284])
        self.assertFalse(self.w.glitching(DOCK))

    def test_a_recreated_node_baselines_instead_of_panicking(self):
        """The profiler counter resets when a node is recreated; the
        shrink must start a fresh baseline, not be treated as glitching
        or as a 4-billion-xrun window."""
        self.feed([30000, 30230, 5])
        self.assertFalse(self.w.glitching(DOCK))

    def test_attempts_are_capped(self):
        self.feed([0, 230, 460])
        self.w.record_attempt(DOCK, 20.0)
        self.feed([690, 920], start=100.0)
        self.w.record_attempt(DOCK, 120.0)
        self.feed([1150, 1380], start=300.0)
        self.assertFalse(self.w.should_recover(DOCK, now=400.0))

    def test_cooldown_blocks_a_rapid_second_attempt(self):
        self.feed([0, 230, 460])
        self.w.record_attempt(DOCK, 20.0)
        self.feed([690], start=30.0)
        self.assertFalse(self.w.should_recover(DOCK, now=30.0))
        self.assertTrue(self.w.should_recover(DOCK, now=90.0))

    def test_one_clean_window_does_not_refill_the_budget(self):
        """The loop observed on hardware: a card cycle buys a quiet
        window while the capture reopens, the refill re-arms, and a
        persistent fault becomes a pop every two minutes. One quiet
        window is the incident still going, not recovery."""
        w = health.GlitchWatch(threshold=50, confirm=2,
                               cooldown_seconds=60, max_attempts=2,
                               clean_refill=3)
        for i, c in enumerate([0, 230, 460]):
            w.observe(DOCK, c, i * 10.0)
        w.record_attempt(DOCK, 20.0)
        w.record_attempt(DOCK, 90.0)
        w.observe(DOCK, 461, 100.0)            # one quiet window
        for i, c in enumerate([700, 940, 1180]):
            w.observe(DOCK, c, 200.0 + i * 10.0)
        self.assertFalse(w.should_recover(DOCK, now=300.0))

    def test_sustained_quiet_refills_the_budget(self):
        """Recovery is per incident, not per process lifetime — but the
        incident has to actually end first."""
        w = health.GlitchWatch(threshold=50, confirm=2,
                               cooldown_seconds=60, max_attempts=2,
                               clean_refill=3)
        for i, c in enumerate([0, 230, 460]):
            w.observe(DOCK, c, i * 10.0)
        w.record_attempt(DOCK, 20.0)
        w.record_attempt(DOCK, 90.0)
        for i, c in enumerate([461, 462, 463]):  # sustained quiet
            w.observe(DOCK, c, 100.0 + i * 10.0)
        for i, c in enumerate([700, 940, 1180]):  # a fresh incident
            w.observe(DOCK, c, 300.0 + i * 10.0)
        self.assertTrue(w.should_recover(DOCK, now=400.0))

    def test_a_post_cycle_counter_reset_is_not_a_clean_window(self):
        """The reset after a card cycle proves nothing about the fault;
        counting it toward refill would shave a window off the leash."""
        w = health.GlitchWatch(threshold=50, confirm=2,
                               cooldown_seconds=60, max_attempts=2,
                               clean_refill=2)
        for i, c in enumerate([0, 230, 460]):
            w.observe(DOCK, c, i * 10.0)
        w.record_attempt(DOCK, 20.0)
        w.record_attempt(DOCK, 90.0)
        w.observe(DOCK, 5, 100.0)     # recreated: baseline, not clean
        w.observe(DOCK, 6, 110.0)     # one genuinely clean window
        for i, c in enumerate([200, 440, 680]):
            w.observe(DOCK, c, 200.0 + i * 10.0)
        self.assertFalse(w.should_recover(DOCK, now=300.0))

    def test_confirmation_fires_exactly_once_per_incident(self):
        """The window that crosses `confirm` is the one to log; every
        later glitchy window would repeat the same warning every 10 s
        for the life of the fault."""
        self.feed([0, 230, 460])
        self.assertTrue(self.w.just_confirmed(DOCK))
        self.feed([690], start=100.0)
        self.assertFalse(self.w.just_confirmed(DOCK))
        self.assertTrue(self.w.glitching(DOCK))

    def test_forget_starts_clean(self):
        self.feed([0, 230, 460])
        self.w.record_attempt(DOCK, 20.0)
        self.w.forget(DOCK)
        self.assertEqual(self.feed([9000]), [False])
        self.assertFalse(self.w.glitching(DOCK))


class SinkStallDeciding(unittest.TestCase):
    def setUp(self):
        self.w = health.SinkStallWatch(cooldown_seconds=60, max_attempts=2)

    def test_an_advancing_pointer_is_healthy(self):
        self.w.observe(SINK, True, 1000, "RUNNING", 0.0)
        stalled = self.w.observe(SINK, True, 49000, "RUNNING", 10.0)
        self.assertFalse(stalled)

    def test_the_first_observation_only_baselines(self):
        """A sink that just started gets a full window before being
        judged, even though its pointer has no history."""
        self.assertFalse(self.w.observe(SINK, True, 1000, "RUNNING", 0.0))
        self.assertFalse(self.w.should_recover(SINK, now=0.0))

    def test_a_static_pointer_while_running_is_a_stall(self):
        self.w.observe(SINK, True, 1000, "RUNNING", 0.0)
        stalled = self.w.observe(SINK, True, 1000, "RUNNING", 10.0)
        self.assertTrue(stalled)
        self.assertTrue(self.w.should_recover(SINK, now=10.0))

    def test_an_idle_sink_holding_still_is_not_a_stall(self):
        """Suspended and idle sinks legitimately stop consuming; cycling
        one would wake hardware nobody is playing to."""
        self.w.observe(SINK, False, 1000, "SETUP", 0.0)
        stalled = self.w.observe(SINK, False, 1000, "SETUP", 10.0)
        self.assertFalse(stalled)

    def test_xrun_state_is_an_immediate_stall(self):
        stalled = self.w.observe(SINK, True, 1000, "XRUN", 0.0)
        self.assertTrue(stalled)

    def test_a_missing_proc_entry_is_not_ours_to_judge(self):
        self.w.observe(SINK, True, None, None, 0.0)
        stalled = self.w.observe(SINK, True, None, None, 10.0)
        self.assertFalse(stalled)

    def test_a_recycle_does_not_feed_its_own_reset_back_as_a_stall(self):
        """suspend/resume resets hw_ptr to zero; comparing the next
        window against the pre-recycle value would misread recovery."""
        self.w.observe(SINK, True, 1000, "RUNNING", 0.0)
        self.w.observe(SINK, True, 1000, "RUNNING", 10.0)
        self.w.record_attempt(SINK, 10.0)
        self.assertFalse(self.w.observe(SINK, True, 0, "RUNNING", 20.0))

    def test_attempts_are_capped_and_cooled_down(self):
        self.w.observe(SINK, True, 1000, "RUNNING", 0.0)
        self.w.observe(SINK, True, 1000, "RUNNING", 10.0)
        self.w.record_attempt(SINK, 10.0)
        self.w.observe(SINK, True, 500, "RUNNING", 20.0)
        self.w.observe(SINK, True, 500, "RUNNING", 30.0)
        self.assertFalse(self.w.should_recover(SINK, now=30.0))   # cooling
        self.assertTrue(self.w.should_recover(SINK, now=80.0))
        self.w.record_attempt(SINK, 80.0)
        self.w.observe(SINK, True, 500, "RUNNING", 150.0)
        self.w.observe(SINK, True, 500, "RUNNING", 160.0)
        self.assertFalse(self.w.should_recover(SINK, now=300.0))  # spent

    def test_sustained_movement_refills_the_budget(self):
        w = health.SinkStallWatch(cooldown_seconds=60, max_attempts=2,
                                  clean_refill=2)
        w.observe(SINK, True, 1000, "RUNNING", 0.0)
        w.observe(SINK, True, 1000, "RUNNING", 10.0)
        w.record_attempt(SINK, 10.0)
        w.record_attempt(SINK, 80.0)
        w.observe(SINK, True, 2000, "RUNNING", 90.0)    # moving…
        w.observe(SINK, True, 50000, "RUNNING", 100.0)  # …recovered
        w.observe(SINK, True, 90000, "RUNNING", 110.0)
        w.observe(SINK, True, 90000, "RUNNING", 120.0)  # new stall
        self.assertTrue(w.should_recover(SINK, now=200.0))

    def test_one_moving_window_does_not_refill(self):
        """A recycle resets the pointer, and the window after can move
        once without the PCM being healthy."""
        w = health.SinkStallWatch(cooldown_seconds=60, max_attempts=2,
                                  clean_refill=2)
        w.observe(SINK, True, 1000, "RUNNING", 0.0)
        w.observe(SINK, True, 1000, "RUNNING", 10.0)
        w.record_attempt(SINK, 10.0)
        w.record_attempt(SINK, 80.0)
        w.observe(SINK, True, 2000, "RUNNING", 90.0)    # moved once
        w.observe(SINK, True, 2000, "RUNNING", 100.0)   # stalled again
        w.observe(SINK, True, 2000, "RUNNING", 110.0)
        self.assertFalse(w.should_recover(SINK, now=300.0))

    def test_a_stall_announces_itself_exactly_once(self):
        self.w.observe(SINK, True, 1000, "RUNNING", 0.0)
        self.w.observe(SINK, True, 1000, "RUNNING", 10.0)
        self.assertTrue(self.w.just_stalled(SINK))
        self.w.observe(SINK, True, 1000, "RUNNING", 20.0)
        self.assertFalse(self.w.just_stalled(SINK))


class MonitorBehavior(unittest.TestCase):
    """Every hardware/command seam is fake; synchronization uses events."""

    def setUp(self):
        self.graph = ({DOCK: True}, {SINK: {
            "running": True, "card": 1, "device": 0, "subdevice": 0}})
        for target, value in (
            ("snapshot_graph", self.graph),
            ("sample_source_mutes", {DOCK: False}),
            ("read_playback_status", (1000, "RUNNING")),
        ):
            patcher = mock.patch.object(health, target, return_value=value)
            setattr(self, target, patcher.start())
            self.addCleanup(patcher.stop)
        patcher = mock.patch.object(health.recovery, "_listing",
                                   return_value=[{"name": SINK, "mute": False}])
        patcher.start()
        self.addCleanup(patcher.stop)
        patcher = mock.patch.object(health.recovery, "card_name_for", return_value="alsa_card.dock")
        patcher.start()
        self.addCleanup(patcher.stop)
        self.cycle = self.enterContext(mock.patch.object(health.recovery, "cycle_card"))
        self.recycle = self.enterContext(mock.patch.object(health, "recycle_sink"))

    def feed(self, monitor, counts, start=0):
        for index, count in enumerate(counts):
            with mock.patch.object(health, "sample_xruns", return_value={DOCK: count}):
                monitor.check_once(start + index * 70)

    def test_default_observes_both_faults_without_modifying_audio(self):
        monitor = health.HealthMonitor()
        self.feed(monitor, [0, 230, 460, 690])
        self.assertTrue(monitor.glitch.glitching(DOCK))
        self.assertTrue(monitor.stall.should_recover(SINK, 300))
        self.cycle.assert_not_called()
        self.recycle.assert_not_called()

    def test_opt_in_remedies_are_capped_at_two(self):
        monitor = health.HealthMonitor(auto_recover=True)
        self.feed(monitor, [0, 230, 460, 690, 920, 1150, 1380])
        self.assertEqual(self.cycle.call_count, 2)
        self.assertEqual(self.recycle.call_count, 2)

    def test_muted_idle_or_unknown_capture_needs_a_new_baseline(self):
        monitor = health.HealthMonitor(auto_recover=True)
        self.feed(monitor, [0, 230])
        self.sample_source_mutes.return_value = {DOCK: True}
        self.feed(monitor, [460], 140)
        self.sample_source_mutes.return_value = {DOCK: False}
        self.feed(monitor, [10000, 10230], 210)
        self.cycle.assert_not_called()
        self.sample_source_mutes.return_value = {}
        self.feed(monitor, [10460], 350)
        self.sample_source_mutes.return_value = {DOCK: False}
        self.graph[0][DOCK] = False
        self.feed(monitor, [10690], 420)
        self.cycle.assert_not_called()

    def test_missing_graph_and_mute_do_not_refill_spent_budget(self):
        monitor = health.HealthMonitor(auto_recover=True)
        self.feed(monitor, [0, 230, 460, 690], 0)
        self.snapshot_graph.return_value = None
        self.feed(monitor, [920], 300)
        self.snapshot_graph.return_value = self.graph
        self.sample_source_mutes.return_value = {DOCK: True}
        self.feed(monitor, [1150], 400)
        self.sample_source_mutes.return_value = {DOCK: False}
        self.feed(monitor, [1380, 1610, 1840], 500)
        self.assertEqual(self.cycle.call_count, 2)

    def test_stop_cancels_and_reaps_a_blocked_sampler_before_returning(self):
        entered = threading.Event()
        killed = threading.Event()
        reaped = threading.Event()

        class Child:
            returncode = -9

            def communicate(self, timeout=None):
                entered.set()
                if not killed.wait(2):
                    raise AssertionError("collector was not cancelled")
                reaped.set()
                return "", ""

            def kill(self):
                killed.set()

        monitor = health.HealthMonitor(auto_recover=True)
        with mock.patch.object(health.recovery.subprocess, "Popen", return_value=Child()):
            monitor.start()
            try:
                self.assertTrue(entered.wait(2), "sampler never started")
            finally:
                monitor.stop()
        self.assertTrue(reaped.is_set())
        self.feed(monitor, [0, 230, 460])
        self.cycle.assert_not_called()
        self.recycle.assert_not_called()

    def test_stop_waits_for_an_in_flight_remedy_to_restore_audio(self):
        entered = threading.Event()
        release = threading.Event()
        restored = threading.Event()
        stopped = threading.Event()
        monitor = health.HealthMonitor(auto_recover=True)

        def remedy(*args):
            entered.set()
            if not release.wait(2):
                raise AssertionError("restoration was never released")
            restored.set()
            return True

        def stop():
            monitor.stop()
            stopped.set()

        self.recycle.side_effect = remedy
        self.feed(monitor, [0])
        with mock.patch.object(health, "sample_xruns", return_value={DOCK: 0}):
            monitor.start()
            stopper = threading.Thread(target=stop)
            try:
                self.assertTrue(entered.wait(2), "remedy never started")
                stopper.start()
                self.assertTrue(monitor._stop.wait(2), "shutdown never started")
                self.assertFalse(stopped.is_set(), "stop returned before restoration")
            finally:
                release.set()
                if stopper.ident is not None:
                    stopper.join(2)
                monitor.stop()
        self.assertTrue(restored.is_set())
        self.assertTrue(stopped.is_set())


if __name__ == "__main__":
    unittest.main()
