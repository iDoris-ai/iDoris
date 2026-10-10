#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

case "$(uname -s)" in
  Linux|Darwin) ;;
  *)
    echo "[subscription] unsupported CI platform: $(uname -s)" >&2
    exit 1
    ;;
esac

run() {
  echo "+ $*" >&2
  "$@"
}

# Unix process/security boundary. Keep these explicit: a removed call must be
# visible in review instead of disappearing behind a broad filter.
for test in \
  subscription_cli_fixture \
  subscription_command \
  subscription_process_group_probe \
  subscription_workspace \
  subscription_spawn \
  subscription_reaper \
  subscription_cancel \
  subscription_relay \
  subscription_shutdown \
  subscription_sandbox
do
  run cargo test --locked -p idoris-upstream --test "$test"
done

run cargo test --locked -p idoris-upstream --lib 'subscription::'
run cargo test --locked -p idoris-router --lib 'connection::tests'
run cargo test --locked -p idoris-router --lib 'subscription::'
for test in \
  subscription_card \
  subscription_source \
  subscription_disconnect \
  subscription_response \
  subscription_discovery \
  subscription_startup \
  subscription_security
do
  run cargo test --locked -p idoris-router --test "$test"
done

# Shared black-box contract against the TS reference. This is reference/PoC
# validation only: no new product behavior is implemented in TypeScript.
run pnpm --filter @idoris/router... build
(
  unset IDORIS_CONFORMANCE_CMD IDORIS_CONFORMANCE_ARGV IDORIS_CONFORMANCE_IMPLEMENTATION
  run pnpm --filter @idoris/conformance test:conformance -- \
    tests/subscription-fixture.test.ts tests/subscription.test.ts
)

# Run the exact same common cases against a release Rust binary. The binary
# resolves its default policy/catalog beside the executable, matching release
# layout rather than repository cwd.
run cargo build --release --locked -p idoris-router
run cargo build --release --locked -p idoris-tenancy --bin conformance_key_seed --features test-bins
bin="${CARGO_TARGET_DIR:-$root/target}/release/idoris"
test -x "$bin"
mkdir -p "$(dirname "$bin")/config"
cp config/routing-policy.yaml "$(dirname "$bin")/config/routing-policy.yaml"
cp config/catalog.yaml "$(dirname "$bin")/config/catalog.yaml"
export IDORIS_CONFORMANCE_ARGV
IDORIS_CONFORMANCE_ARGV="$(node -e 'process.stdout.write(JSON.stringify([process.argv[1], "serve"]))' "$bin")"
unset IDORIS_CONFORMANCE_CMD
export IDORIS_CONFORMANCE_IMPLEMENTATION=rust
export IDORIS_CONFORMANCE_KEY_SEEDER="${CARGO_TARGET_DIR:-$root/target}/release/conformance_key_seed"
export IDORIS_CONFORMANCE_POST_RETRY=0
run pnpm --filter @idoris/conformance test:conformance -- \
  tests/subscription-fixture.test.ts tests/subscription.test.ts
