M5-22 — content tightening recorded in decided reasons (review low item), stacked on M5-21.

INSPECTED lowers `task.privacy` itself (routing policy + intent detector must see the effective floor), so the pipeline never emitted `PrivacyTightenedByContent`. When content (not the kill switch) tightened the request, the selected decision now carries `privacy_tightened_by_content` first — where `content_tightening` would have put it. Decided `reason_codes` and, on Supervisor responses, `X-iDoris-Reason` reflect it (the proxy path never sets `X-iDoris-Reason`).

Test: `any` chat with an id, `any`-only + trusted-local cards → served locally, Decided lists the reason first; removing the insertion turns it red.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
