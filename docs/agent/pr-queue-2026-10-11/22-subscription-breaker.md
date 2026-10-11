M5-19 — subscription relay faults feed the OAuth breaker (B-8: 3 faults / 60s), stacked on M5-18.

- Chat's subscription branch reports its relay outcome under the subscription provider id: CliFailed / EmptyOutput / SpawnFailed / Timeout / CleanupFailed count; success resets; Cancelled, OutputLimit and all local rejections are ignored.
- Selection already avoids any open provider in the candidate set (M5-18), so an open subscription breaker routes elsewhere or is rejected before a CLI spawns.
- `ChatProxy::report_outcome(provider, class, Option<bool>)` replaces the local-only helper.

Tests: classifier for every relay code/local rejection; OAuth threshold behaviour. **Not exercised end to end** through a real CLI relay (needs the subscription CLI fixture harness).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
