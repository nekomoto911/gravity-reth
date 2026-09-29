#!/usr/bin/env bash
#
# Long-term regression invariants for the Gravity Alpha system-tx gas-exempt
# design (PR #367). Each invariant fails with a paste-able rg command on
# stderr for fast reproduction. Both invariant logic + thresholds come from
# the design's code-review §6.1 + acceptance-tests §5; the original Rust
# version lived under `crates/ethereum/evm/tests/grep_checklist.rs` until
# CI runners (ubuntu-latest) showed they don't ship ripgrep — making
# `Command::new("rg")` shell-out from a `#[test]` non-portable. A bash
# script that the CI step explicitly `apt-get install`s ripgrep for is the
# correct home: this is a lint, not a unit test, and a self-contained
# shell script keeps the assertion text usable as both a hard check AND a
# paste-able reproduction recipe.
#
# Invariants (numbered as introduced; 3, 5, 6 and 8 were retired once the
# Gravity execution rules moved into `GravityEvmConfig` — the RPC replay
# paths no longer carry rules to pin, and `gravity_pipe` checks every replay
# against the committed chain, including a full re-run once the node's tip
# has passed every fork):
#   1. `fn transact_system_txn` lives in exactly two source files
#      (serial impl + grevm impl).
#   2. SYSTEM_CALLER address literal `625f0000` has a single source of
#      truth (chainspec/src/gravity.rs + helper + tests only).
#   4. `execute_history_block` / `push_history_block` has no non-test
#      callers (R6 / design §3.6 keeps this debug-only).
#   7. (HP-2) `disable_base_fee` / `disable_balance_check` writes are
#      either fork-gated (file co-references `is_system_tx_gas_exempt` /
#      `GravityHardfork::Alpha`) or live in approved
#      simulation-endpoint files / test files.
#   9. CI allowlist parity: every Gravity pipe integration test binary
#      under `crates/pipe-exec-layer-ext-v2/execute/tests/` — a
#      `gravity_*.rs` file or a `gravity_*/main.rs` directory — must EITHER
#      appear in the `--test <name>` allowlist in
#      `.github/workflows/integration.yml` OR be listed in the
#      `KNOWN_UNWIRED_TESTS` skip set inside invariant 9 with a documented
#      reason. Rationale: `-p reth-pipe-exec-layer-ext-v2` alone would run
#      every binary in the dir, including `pipe_test`, `mainnet_replay` and
#      `wipe_recreate_e2e`, which the job deliberately leaves out, so the
#      workflow uses an explicit allowlist. Without invariant 9, a
#      newly added test binary is silently skipped by CI (as happened with
#      the six #367 / #370 files) — assertions may pass locally but never
#      gate a merge.
#
# Invocation:
#   bash scripts/check-gravity-invariants.sh
#
# Exit code: 0 = all invariants pass; 1 = first failure aborts with the
# reproduction rg command on stderr.

set -euo pipefail

# Repo root: this script lives in <repo>/scripts, so `..` is the repo root.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if ! command -v rg >/dev/null 2>&1; then
    echo "FAIL: ripgrep (rg) must be installed to run this lint." >&2
    echo "      On ubuntu/debian: sudo apt-get install -y ripgrep" >&2
    exit 1
fi

fail() {
    # $1 = invariant id, $2 = human description, $3 = reproduction rg command
    echo "FAIL: invariant $1 — $2" >&2
    echo "  reproduce with: $3" >&2
    exit 1
}

ok() {
    echo "OK:   invariant $1 — $2"
}

# ---------------------------------------------------------------------------
# Invariant 1 — `fn transact_system_txn` lives in exactly two files.
# ---------------------------------------------------------------------------
echo "Invariant 1: fn transact_system_txn must live in exactly 2 source files (serial + grevm)"
# `gravity/mod.rs` only forwards `GravityEvmConfig::transact_system_txn` to the serial impl.
impl_files() {
    rg --type rust -l 'fn transact_system_txn' crates/ethereum/evm/src | sort -u | \
        grep -vE 'crates/ethereum/evm/src/gravity/mod\.rs$'
}
files_count=$(impl_files | wc -l)
if [ "$files_count" -ne 2 ]; then
    fail 1 "expected exactly 2 files defining fn transact_system_txn under crates/ethereum/evm/src, got $files_count" \
           "rg --type rust -l 'fn transact_system_txn' crates/ethereum/evm/src"
fi
has_lib=$(impl_files | grep -c 'lib\.rs$' || true)
has_parallel=$(impl_files | grep -c 'parallel_execute\.rs$' || true)
if [ "$has_lib" -lt 1 ] || [ "$has_parallel" -lt 1 ]; then
    fail 1 "expected serial impl in lib.rs and grevm impl in parallel_execute.rs" \
           "rg --type rust -l 'fn transact_system_txn' crates/ethereum/evm/src"
fi
ok 1 "fn transact_system_txn in serial + grevm impls only"

# ---------------------------------------------------------------------------
# Invariant 2 — SYSTEM_CALLER literal `625f0000` is a single source of truth.
# Allowed buckets:
#   - canonical const in crates/chainspec/src/gravity.rs
#   - test files (path under tests/ OR filename ends with `_test.rs`)
#   - system_caller_migration.rs (asserts the literal stays canonical)
# Anything else is a re-declaration regression.
# ---------------------------------------------------------------------------
echo "Invariant 2: SYSTEM_CALLER literal 625f0000 single source"
unapproved=$(rg --type rust -l '625f0000' crates/ | sort -u | \
    grep -vE 'crates/chainspec/src/gravity\.rs$' | \
    grep -vE '/tests/' | \
    grep -vE '_test\.rs$' | \
    grep -vE 'system_caller_migration\.rs$' || true)
if [ -n "$unapproved" ]; then
    fail 2 "SYSTEM_CALLER literal 625f0000 found outside chainspec/src/gravity.rs, tests, and system_caller_migration.rs. Unapproved files:
$unapproved" \
        "rg --type rust -n '625f0000' crates/"
fi
ok 2 "SYSTEM_CALLER literal lives in the canonical const + helpers + tests only"

# ---------------------------------------------------------------------------
# Invariant 4 — execute_history_block has no non-test callers.
# Approved hits live in:
#   - pipe-exec-layer-ext-v2/execute/src/lib.rs (def + dispatch)
#   - pipe-exec-layer-ext-v2/execute/tests/* (any test)
#   - scripts/check-gravity-invariants.sh (this script, by naming the symbol)
# ---------------------------------------------------------------------------
echo "Invariant 4: execute_history_block / push_history_block has no non-test callers"
unapproved=$(rg --type rust -l 'push_history_block|execute_history_block' crates/ | sort -u | \
    grep -vE 'crates/pipe-exec-layer-ext-v2/execute/src/lib\.rs$' | \
    grep -vE 'crates/pipe-exec-layer-ext-v2/execute/tests/' || true)
if [ -n "$unapproved" ]; then
    fail 4 "execute_history_block/push_history_block referenced outside pipe-exec-layer-ext-v2 lib.rs + tests. Unapproved files:
$unapproved" \
        "rg --type rust -n 'push_history_block|execute_history_block' crates/"
fi
ok 4 "history_block dispatch confined to pipe-exec-layer src/lib.rs and tests"

# ---------------------------------------------------------------------------
# Invariant 7 (HP-2 #1) — disable_base_fee / disable_balance_check writes
# must either be fork-gated (file co-references is_system_tx_gas_exempt /
# is_gravity_system_caller / GravityHardfork::Alpha / SYSTEM_CALLER) OR be
# in an approved bucket:
#   - test files (path under /tests/ OR filename ends _test.rs)
#   - approved pure-simulation endpoint files that legitimately set
#     disable_base_fee unconditionally (eth_estimateGas etc.); since these
#     don't replay user-signed txs, the gas-exempt feature does not apply.
#     The allowlist is held tight to force a future contributor to audit
#     each addition.
# ---------------------------------------------------------------------------
echo "Invariant 7 (HP-2 #1): disable_base_fee / disable_balance_check writes are fork-gated"
# `call.rs`: `eth_call`, `eth_createAccessList` and `eth_simulateV1` (reth's own settings for
# requests; the SYSTEM_CALLER gas exemption is applied by `GravityEvm` per transaction).
approved_sim_endpoints='crates/rpc/rpc-eth-api/src/helpers/(estimate|call)\.rs$'
hits=$(rg --type rust -l 'disable_base_fee|disable_balance_check' \
    crates/rpc crates/ethereum/evm crates/pipe-exec-layer-ext-v2 | sort -u)
unapproved=""
for f in $hits; do
    # bucket: tests
    if echo "$f" | grep -qE '/tests/|_test\.rs$'; then continue; fi
    # bucket: known pure-simulation endpoint allowlist
    if echo "$f" | grep -qE "$approved_sim_endpoints"; then continue; fi
    # bucket: file co-references a fork gate or sender check
    if rg --type rust -q 'is_system_tx_gas_exempt|is_gravity_system_caller|GravityHardfork::Alpha|SYSTEM_CALLER' "$f"; then
        continue
    fi
    unapproved="${unapproved}${f}
"
done
if [ -n "$unapproved" ]; then
    fail 7 "disable_base_fee/disable_balance_check writes appear in files lacking a fork gate or sender check. Unapproved files:
${unapproved}If a new pure-simulation endpoint legitimately needs these flags unconditionally, add it to the allowlist in this script." \
        "rg --type rust -n 'disable_base_fee|disable_balance_check' crates/rpc crates/ethereum/evm crates/pipe-exec-layer-ext-v2"
fi
ok 7 "disable_* writes are either fork-gated or in approved simulation/test files"

# ---------------------------------------------------------------------------
# Invariant 9 — CI allowlist parity for Gravity pipe integration tests.
# Every test binary under `crates/pipe-exec-layer-ext-v2/execute/tests/` named
# `gravity_*` — a single-file binary `gravity_<name>.rs` or a directory binary
# `gravity_<name>/main.rs` — must EITHER appear as a `--test <name>` argument
# in `.github/workflows/integration.yml` OR be listed in `KNOWN_UNWIRED_TESTS`
# below with a documented reason. The
# workflow uses an explicit allowlist (not `-p reth-pipe-exec-layer-ext-v2`
# alone) to leave out the crate's other integration binaries (`pipe_test`,
# `mainnet_replay`, `wipe_recreate_e2e`); that same allowlist silently skips
# new files unless they are added by hand.
#
# Deferred entries force a future contributor to make an explicit
# claim ("wire" or "defer, with reason"), preventing another silent-skip
# incident like #367 / #370.
# ---------------------------------------------------------------------------
echo "Invariant 9: CI --test allowlist covers all gravity_* pipe integration test binaries (or explicit KNOWN_UNWIRED_TESTS)"

# Known-unwired: test binary name → one-line reason. Any entry here
# should also have a follow-up issue / PR tracked. Removing an entry
# means the corresponding `--test <name>` line must be added to the
# workflow.
declare -A KNOWN_UNWIRED_TESTS=()

workflow="$REPO_ROOT/.github/workflows/integration.yml"
tests_dir="crates/pipe-exec-layer-ext-v2/execute/tests"
if [ ! -f "$workflow" ]; then
    fail 9 "expected workflow file at .github/workflows/integration.yml — invariant needs to know where to look for the allowlist" \
        "ls .github/workflows/integration.yml"
fi
missing=""
double_claimed=""
# Cargo builds `tests/<name>.rs` and `tests/<name>/main.rs` each into a test
# binary called `<name>`.
for f in $(ls "$tests_dir"/gravity_*.rs "$tests_dir"/gravity_*/main.rs 2>/dev/null); do
    case "$f" in
        */main.rs) binary=$(basename "$(dirname "$f")") ;;
        *) binary=$(basename "$f" .rs) ;;
    esac
    in_workflow=false
    if grep -qE "^[[:space:]]*--test[[:space:]]+${binary}([[:space:]]|\\\\|$)" "$workflow"; then
        in_workflow=true
    fi
    in_unwired=false
    if [ "${KNOWN_UNWIRED_TESTS[$binary]+set}" = "set" ]; then
        in_unwired=true
    fi
    if [ "$in_workflow" = "false" ] && [ "$in_unwired" = "false" ]; then
        missing="${missing}${binary}
"
    fi
    if [ "$in_workflow" = "true" ] && [ "$in_unwired" = "true" ]; then
        double_claimed="${double_claimed}${binary}
"
    fi
done
if [ -n "$missing" ]; then
    fail 9 "integration test binary(ies) neither wired to CI nor in KNOWN_UNWIRED_TESTS. Add \`--test <name> \\\` line(s) to .github/workflows/integration.yml (gravity-pipe-test job) OR add an entry to KNOWN_UNWIRED_TESTS in this script with a documented reason. Missing:
${missing}Rationale: silently-skipped tests can't gate merges (see integration.yml comment)." \
        "grep -E '^[[:space:]]*--test' .github/workflows/integration.yml"
fi
if [ -n "$double_claimed" ]; then
    fail 9 "integration test binary(ies) both in CI allowlist AND KNOWN_UNWIRED_TESTS — pick one. Double-claimed:
${double_claimed}Remove the KNOWN_UNWIRED_TESTS entry if now wired, or drop the workflow line if you meant to defer." \
        "grep -E '^[[:space:]]*--test' .github/workflows/integration.yml"
fi
# Also flag KNOWN_UNWIRED_TESTS references to files that no longer exist —
# likely a rename / deletion missed the invariant update.
stale=""
for binary in "${!KNOWN_UNWIRED_TESTS[@]}"; do
    if [ ! -f "$tests_dir/${binary}.rs" ] && [ ! -f "$tests_dir/${binary}/main.rs" ]; then
        stale="${stale}${binary}
"
    fi
done
if [ -n "$stale" ]; then
    fail 9 "KNOWN_UNWIRED_TESTS references test binary(ies) that no longer exist under $tests_dir/. Remove the stale entries or restore the files. Stale:
${stale}" \
        "ls $tests_dir/"
fi
ok 9 "CI --test allowlist + KNOWN_UNWIRED_TESTS jointly cover all gravity_* pipe integration test binaries"

echo
echo "All Gravity invariants passed."
