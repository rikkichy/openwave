"""Recovery decisions and transactions without a sound card or command."""

import json
import unittest
from unittest import mock

from wavexlr import recovery

DOCK = "alsa_input.usb-Elgato_XLR_Dock_SERIAL-00.mono-fallback"


class Deciding(unittest.TestCase):
    def test_absent_unmetered_and_recently_flowing_are_not_stalled(self):
        watch = recovery.StallWatch()
        self.assertFalse(watch.should_recover(DOCK, False, 999, 100))
        self.assertFalse(watch.should_recover(DOCK, True, None, 100))
        self.assertFalse(watch.should_recover(DOCK, True, 1, 100))
        self.assertTrue(watch.should_recover(DOCK, True, 9, 100))

    def test_attempts_have_cooldown_cap_and_separate_device_budgets(self):
        watch = recovery.StallWatch()
        watch.record_attempt(DOCK, 100)
        self.assertFalse(watch.should_recover(DOCK, True, 9, 110))
        self.assertTrue(watch.should_recover(DOCK, True, 9, 160))
        watch.record_attempt(DOCK, 160)
        self.assertFalse(watch.should_recover(DOCK, True, 9, 1000))
        self.assertTrue(watch.should_recover("other", True, 9, 1000))
        watch.forget(DOCK)
        self.assertTrue(watch.should_recover(DOCK, True, 9, 1010))

    def test_only_sustained_flow_refills_an_incident_budget(self):
        watch = recovery.StallWatch(clean_refill_seconds=300)
        watch.record_attempt(DOCK, 0)
        watch.record_attempt(DOCK, 60)
        watch.record_recovered(DOCK, 100)
        watch.record_recovered(DOCK, 110)
        self.assertFalse(watch.should_recover(DOCK, True, 9, 200))
        watch.record_recovered(DOCK, 210)
        watch.record_recovered(DOCK, 509)
        self.assertFalse(watch.should_recover(DOCK, True, 9, 509))
        watch.record_recovered(DOCK, 600)
        watch.record_recovered(DOCK, 900)
        self.assertTrue(watch.should_recover(DOCK, True, 9, 901))


class CardIdentity(unittest.TestCase):
    def test_uses_exact_node_card_index_not_name_stem_or_first_device(self):
        data = {
            "sources": [
                {"name": DOCK + "extra", "card": 1},
                {"name": DOCK, "card": 2},
            ],
            "cards": [
                {"name": "alsa_card.wrong", "index": 1},
                {"name": "alsa_card.correct", "index": 2},
            ],
        }
        with mock.patch.object(recovery, "_pactl", side_effect=lambda *a, **k: json.dumps(data[a[-1]])):
            self.assertEqual(recovery.card_name_for(DOCK), "alsa_card.correct")
            self.assertIsNone(recovery.card_name_for(DOCK + "missing"))
            data["sources"].append({"name": DOCK, "card": 1})
            self.assertIsNone(recovery.card_name_for(DOCK))

    def test_unknown_mapping_never_guesses_a_card(self):
        with mock.patch.object(recovery, "_pactl", return_value=None):
            self.assertIsNone(recovery.card_name_for(DOCK))


class Cycling(unittest.TestCase):
    def setUp(self):
        self.cards = {
            "alsa_card.other": "off",
            "alsa_card.dock": "output:analog-stereo+input:mono-fallback",
        }
        self.original = self.cards["alsa_card.dock"]
        self.closed = []
        self.runner = recovery.CommandRunner()
        self.cancel_on_off = False
        self.timeout_on_off = False
        patcher = mock.patch.object(recovery, "_pactl", side_effect=self.pactl)
        patcher.start()
        self.addCleanup(patcher.stop)

    def pactl(self, *args, **kwargs):
        if args[:3] == ("--format=json", "list", "cards"):
            return json.dumps([{"name": name, "active_profile": profile}
                               for name, profile in self.cards.items()])
        action, card, profile = args
        self.assertEqual(action, "set-card-profile")
        self.cards[card] = profile
        if profile == "off":
            self.closed.append(card)
            if self.cancel_on_off:
                self.runner.cancel()
            if self.timeout_on_off:
                return None
        return ""

    def test_restores_exact_original_profile_even_during_stop(self):
        self.cancel_on_off = True
        self.assertTrue(recovery.cycle_card("alsa_card.dock", self.runner))
        self.assertEqual(self.closed, ["alsa_card.dock"])
        self.assertEqual(self.cards["alsa_card.dock"], self.original)
        self.assertEqual(self.cards["alsa_card.other"], "off")

    def test_off_timeout_still_restores_potentially_applied_change(self):
        self.timeout_on_off = True
        self.assertFalse(recovery.cycle_card("alsa_card.dock", self.runner))
        self.assertEqual(self.cards["alsa_card.dock"], self.original)

    def test_off_missing_and_cancelled_cards_are_never_touched(self):
        self.assertFalse(recovery.cycle_card("alsa_card.other", self.runner))
        self.assertFalse(recovery.cycle_card("alsa_card.absent", self.runner))
        self.runner.cancel()
        self.assertFalse(recovery.cycle_card("alsa_card.dock", self.runner))
        self.assertEqual(self.closed, [])


if __name__ == "__main__":
    unittest.main()
