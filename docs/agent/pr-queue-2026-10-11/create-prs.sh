#!/bin/bash
# Opens the 2026-10-11 night stack in order. Each line: base head title body [draft].
# Requires a valid `gh auth status`. Safe to re-run: skips heads that already have an open PR.
# Run from anywhere: bash docs/agent/pr-queue-2026-10-11/create-prs.sh (needs a checkout of this branch).
set -euo pipefail
cd "$(dirname "$0")"
R=iDoris-ai/iDoris
open_pr() {
  local base=$1 head=$2 title=$3 body=$4 draft=${5:-}
  if [ -n "$(gh pr list -R $R --head "$head" --state open --json number --jq '.[].number')" ]; then
    echo "skip $head (already open)"; return
  fi
  gh pr create -R $R --base "$base" --head "$head" --title "$title" --body-file "$body" $draft
}
open_pr docs/handoff-2026-10-11-m4-tail docs/handoff-2026-10-11-night "docs(agent): hand off 2026-10-11 night stack" 00-handoff.md
open_pr main integration/m4-landing-tail "chore(m4): integrate open M4 landing tail for acceptance" 01-integration.md --draft
open_pr integration/m4-landing-tail test/m4-acceptance-gaps "test(router): close M4 acceptance evidence gaps" 02-gaps.md
open_pr integration/m4-landing-tail feat/inspect-01-contract-freeze "feat(contracts): freeze M5-01 /v1/inspect contract" 03-inspect-01.md
open_pr feat/inspect-01-contract-freeze feat/inspect-02-l0-inspector "feat(policy): add pure L0 inspector for gate one" 04-inspect-02.md
open_pr feat/inspect-02-l0-inspector feat/inspect-03-cn-identifiers "feat(policy): add CN/HK identifier and email L0 rules" 05-inspect-03.md
open_pr feat/inspect-03-cn-identifiers feat/inspect-04-secret-rules "feat(policy): add credential L0 rules with two independent sets" 06-inspect-04.md
open_pr feat/inspect-04-secret-rules feat/inspect-05-endpoint "feat(router): serve POST /v1/inspect from the L0 inspector" 07-inspect-05.md
open_pr feat/inspect-05-endpoint feat/inspect-06-route-stage "feat(router): add INSPECTED stage to the chat path" 08-inspect-06.md
open_pr feat/inspect-06-route-stage feat/inspect-07-protocol-endpoints "feat(router): inspect embeddings, rerank and messages bodies" 09-inspect-07.md
open_pr feat/inspect-07-protocol-endpoints feat/inspect-08-remote-kill-switch "feat(router): add remote kill switch to gate one" 10-inspect-08.md
open_pr feat/inspect-08-remote-kill-switch feat/inspect-09-egress-counter "feat(router): count off-host proxy egress bytes" 11-inspect-09.md
open_pr feat/inspect-09-egress-counter feat/inspect-10-evasion-normalization "feat(policy): fold L0 evasions before scanning" 12-inspect-10.md
open_pr feat/inspect-10-evasion-normalization feat/inspect-11-encoded-content "feat(policy): decode one layer of encoding before L0" 13-inspect-11.md
open_pr feat/inspect-11-encoded-content test/m5-budget-concurrency "test(router): prove 100 concurrent paid chats cannot overrun budget" 14-budget-concurrency.md
open_pr test/m5-budget-concurrency feat/inspect-12-redact-core "feat(policy): add reversible redaction engine for gate one" 15-inspect-12.md
open_pr feat/inspect-12-redact-core feat/resilience-01-breaker-core "feat(policy): add pure circuit breaker for three-grain resilience" 16-breaker-core.md
open_pr feat/resilience-01-breaker-core feat/resilience-02-proxy-breaker "feat(router): gate buffered proxy sends with provider breakers" 17-proxy-breaker.md
open_pr feat/resilience-02-proxy-breaker feat/resilience-03-stream-breaker "feat(router): put streaming proxy sends behind provider breakers" 18-stream-breaker.md
open_pr feat/resilience-03-stream-breaker feat/resilience-04-steer-selection "feat(router): steer chat selection away from open provider breakers" 19-steer-selection.md
open_pr feat/resilience-04-steer-selection feat/resilience-05-protocol-steering "feat(router): steer protocol endpoint selection away from open breakers" 20-protocol-steering.md
open_pr feat/resilience-05-protocol-steering feat/resilience-06-local-model-lockout "feat(router): lock out failing Supervisor models" 21-local-lockout.md
open_pr feat/resilience-06-local-model-lockout feat/resilience-07-subscription-breaker "feat(router): feed subscription relay faults to the OAuth breaker" 22-subscription-breaker.md
open_pr feat/resilience-07-subscription-breaker fix/inspect-13-review-hardening "fix(inspect): close gate-one fail-open paths from adversarial review" 23-review-hardening.md
open_pr fix/inspect-13-review-hardening feat/inspect-14-off-worker-scan "feat(router): scan large bodies off the async worker" 24-off-worker.md
open_pr feat/inspect-14-off-worker-scan feat/inspect-15-decided-tightening-reason "feat(router): record content tightening in the decided reasons" 25-decided-reason.md
open_pr feat/inspect-15-decided-tightening-reason feat/inspect-16-homoglyph-folding "feat(policy): fold digit scripts and Latin homoglyphs before L0" 26-homoglyphs.md
open_pr feat/inspect-16-homoglyph-folding docs/m5-gate-one-runtime "docs(interfaces): describe gate-one runtime behaviour and M5 switches" 27-docs-runtime.md
open_pr docs/m5-gate-one-runtime fix/inspect-17-window-segment-floor "fix(policy): stop joining short digit runs into one entity" 28-window-floor.md
open_pr fix/inspect-17-window-segment-floor fix/inspect-18-second-review "fix(inspect): address second review of the M5 hardening" 29-second-review.md
open_pr fix/inspect-18-second-review fix/inspect-19-third-review "fix(inspect): judge digit groups whole or fully decomposed" 30-third-review.md
open_pr fix/inspect-19-third-review fix/inspect-20-grouping-recall "fix(inspect): recover grouped entities next to short numbers" 31-grouping-recall.md
open_pr fix/inspect-20-grouping-recall feat/inspect-21-opaque-in-inspector "feat(policy): treat embedded base64 payloads as uninspectable" 32-opaque-inspector.md
open_pr feat/inspect-21-opaque-in-inspector feat/resilience-08-connection-cooldown "feat(router): cool whole connections on Retry-After" 33-connection-cooldown.md
open_pr feat/resilience-08-connection-cooldown feat/event-log-28-protocol-chain "feat(router): record the decision chain on protocol endpoints" 34-protocol-chain.md
open_pr feat/event-log-28-protocol-chain feat/event-log-29-protocol-dispatch "feat(router): record dispatch and completion on protocol endpoints" 35-protocol-dispatch.md
open_pr feat/event-log-29-protocol-dispatch feat/event-log-30-messages-stream "feat(router): complete streamed /v1/messages in the Event Log" 36-messages-stream.md
open_pr feat/event-log-30-messages-stream feat/gate-two-01-egress-decorrelation "feat(router): de-correlate request bodies sent off host" 37-decorrelate.md
open_pr feat/gate-two-01-egress-decorrelation feat/gate-two-02-broker-core "feat(policy): add the gate-two credential broker core" 38-broker-core.md
open_pr feat/gate-two-02-broker-core fix/review-5-hardening "fix: address the fifth review (protocol chain, breakers, broker core)" 39-review5.md
