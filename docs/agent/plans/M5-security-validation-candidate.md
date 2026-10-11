# M5 Security Validation Candidate

## Composition

The candidate combines the tenant-key authority stack (#463/#465/#467/#468),
Event Log publication/terminal fixes (#469–#473), paid pre-send cleanup (#474),
and the subscription fixture-flow regression. Ordinary merges add #464's
accepted wire contract 1.1.0 and #466's process-reaping/port fixture fixes.
No parent feature branch is rewritten.

## Validation

Run the workstation B Rust feature gates serially, cargo deny, all pnpm gates,
and release Rust conformance on this exact combined head. Individual slice
results are evidence for review, not a substitute for this integration run.

## Landing boundary

This is a draft integration vehicle. Dependency approvals, CI and the M4
landing/main acceptance remain required; main is still 792ed904 and v0.3.0
has not been tagged. The approved inspect shape is input:string with UTF-16
spans and wire version 1.1.0 at publication.

REDACT consent, Admin v1 writes, broker storage/wire and real vendor CLI
validation remain owner decisions. Breaker defaults retain their existing
opt-in behavior. This candidate does not declare M5 released or complete.
