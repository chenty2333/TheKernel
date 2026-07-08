from __future__ import annotations

import unittest

from tools.oscomp_eval import config


class ConfigTimeoutTests(unittest.TestCase):
    def test_replay_timeout_constants_are_ordered(self) -> None:
        self.assertLess(config.REPLAY_TIMEOUT_SMOKE_SECS, config.REPLAY_TIMEOUT_FOCUSED_SECS)
        self.assertLess(config.REPLAY_TIMEOUT_FOCUSED_SECS, config.REPLAY_TIMEOUT_FULL_SECS)

    def test_eval_config_uses_shared_timeout_defaults(self) -> None:
        defaults = config.EvalConfig()

        self.assertEqual(defaults.judge_timeout_secs, config.JUDGE_TIMEOUT_SECS)
        self.assertEqual(defaults.replay_timeout_secs, config.REPLAY_TIMEOUT_FULL_SECS)


if __name__ == "__main__":
    unittest.main()