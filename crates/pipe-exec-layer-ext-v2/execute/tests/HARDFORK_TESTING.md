# Gravity pipe hardfork-timeline test

## Overview

`gravity_pipe` (the directory `tests/gravity_pipe/`, built by Cargo from `main.rs` into one test
binary) is the integration test of the Gravity pipe execution layer. A single test function starts
one reth node from the Gravity mainnet genesis and walks it, in wall-clock time, through every
hardfork in chain order:

```
genesis → Prague → Alpha → Beta → Gamma
```

A mock consensus layer builds ordered blocks the way gravity-sdk does and pushes them through
`PipeExecLayerApi`. Every block is executed by the pipe (grevm), committed and persisted before
the next one is built. Along the way:

- each hardfork module puts its scenario transactions into the blocks before, at and after its
  activation, and asserts what the committed blocks show;
- the epoch changes several times, in every phase;
- after every committed block, every RPC endpoint that re-executes blocks must reproduce what the
  pipe committed (the **replay check**).

Replay differences and scenario failures do not stop the run. They are collected over the whole
timeline, and the test fails once at the end with every one of them.

## Layout

| Path | Role |
|------|------|
| `gravity_pipe/main.rs` | Entry point and timeline loop: build a block, classify its phase, get the scenario content, commit, replay-check, run the scenario assertions |
| `gravity_pipe/timeline.rs` | `Fork`, `Phase`, `Timeline`: hardfork times, phase classification, deadline, and the in-memory genesis changes |
| `gravity_pipe/node.rs` | Node launch (`run_node`), mock consensus (`Node`), test accounts and transaction signing |
| `gravity_pipe/report.rs` | `MismatchReport`: collects replay and scenario mismatches, fails the test at the end |
| `gravity_pipe/rpc.rs` | Blocking JSON-RPC client for the node's HTTP endpoint |
| `gravity_pipe/replay/` | The replay check, one module per endpoint class |
| `gravity_pipe/hardfork/mod.rs` | `Scenarios`: dispatches every block to the scenario modules |
| `gravity_pipe/hardfork/base.rs` | Scenarios that do not depend on a hardfork |
| `gravity_pipe/hardfork/{prague,alpha,beta,gamma}.rs` | One module per hardfork (`alpha/` holds Alpha's simulation and mid-block scenarios; `gamma.rs` has no scenarios yet) |
| `gravity_pipe/hardfork/chain.rs` | `Chain`: reads of the committed chain for scenarios |
| `gravity_pipe/mainnet_genesis.json` | Gravity mainnet genesis, byte-identical to `gravity-sdk/genesis/mainnet/genesis.json` |

## Genesis

The test starts from the real mainnet genesis (chain id 127001, 7 validators, DKG enabled, the
mainnet system contracts and configuration). The fixture file is never edited;
`Timeline::genesis_json` makes exactly three changes in memory before the node starts:

1. **Hardfork times.** `pragueTime`, `alphaTime`, `betaTime` and `gammaTime` are set to the test's
   wall-clock schedule (mainnet genesis only has `pragueTime`, which is overwritten).
2. **Epoch interval.** Mainnet reconfigures every 2 hours. The test shortens the interval to
   10 seconds by rewriting slot 0 of `EpochConfig` (`0x00000000000000000000000000000001625f1005`):
   the low 8 bytes hold the interval in microseconds, bit 136 is the initialized flag. The test
   first asserts that the slot still holds the mainnet value, so a layout change fails loudly.
3. **Test accounts.** Mainnet genesis funds no ordinary account, so the funded test accounts
   (`TestAccount::FUNDED` in `node.rs`) get 10^6 ether each. One of them, `Delegated`, also gets
   an EIP-7702 delegation designator as code: EIP-7702 is locked down until Beta, so only genesis
   can delegate an account before it. The test asserts that no test account collides with a
   genesis account.

To refresh the fixture, copy `gravity-sdk/genesis/mainnet/genesis.json` over
`mainnet_genesis.json` unchanged.

## Timeline

- **Hardfork times** are the test start plus `FORK_OFFSETS` in `timeline.rs` (currently 40, 80,
  120 and 160 seconds for Prague, Alpha, Beta and Gamma).
- **Block timestamps** are the wall-clock time at which the block is built; the loop sleeps
  `BLOCK_INTERVAL` (1 second) between blocks. Block numbers are therefore only roughly
  predictable.
- **Activation is by timestamp, never by block number.** A block activates a fork when
  `parent_ts < fork_time <= block_ts`. Each block is classified before it is built as
  `Phase::Genesis`, `Phase::Activation(fork)` or `Phase::After(fork)`; after commit the test
  asserts that the chain spec agrees on every activation block. A block that would activate two
  forks at once panics (block production was too slow for the schedule).
- **End.** The loop stops after the first epoch change after Gamma, or at the deadline (the last
  fork time plus 60 seconds). A local run takes about 3 minutes and some 80 blocks.

## Epoch changes

Once the epoch interval has elapsed, the next block's `onBlockStart` opens a DKG session. The
epoch only changes in the block that carries a DKG transcript, and the mock consensus decides
which block that is:

- A pending transcript goes into the next block that is **not an activation block** (no mainnet
  activation block ever changed the epoch). Scenarios get all remaining blocks: an epoch-change
  block drops user transactions, so scenario transactions never go into one.
- Every phase (genesis, and after each of Prague, Alpha, Beta, Gamma) must see at least one epoch
  change, and the run must see at least five in total (`MIN_EPOCH_CHANGES`).
- The transcript is fake: the contracts store it without verifying it. The node asserts that a
  block changes the epoch exactly when it carries a transcript, and that no epoch is skipped.
- On the first epoch change, the test also pushes a block of the old epoch and asserts that the
  pipe discards it.

The phase lengths in `FORK_OFFSETS` leave room for one epoch interval plus a few slow blocks in
every phase. Enlarge them when scenarios need more blocks, and keep one run under about 10 minutes.

## Replay check

After every committed block, `replay::check_block` reads what the pipe committed back from the
node's storage (header, receipts, changesets, historical state) and calls every replay endpoint
over HTTP JSON-RPC, as on a mainnet RPC node (the node runs with `--http --http.api all`):

| Class | Endpoints | Compared with the committed block |
|-------|-----------|-----------------------------------|
| Whole-block traces | `debug_traceBlockByHash`, `debug_traceBlockByNumber`, `debug_traceBlock` (call tracer); `trace_block`, `trace_filter`; `trace_replayBlockTransactions` (`trace` + `stateDiff`); `trace_blockOpcodeGas` | Geth-style traces: each transaction's gas used and success; parity-style traces: success; opcode gas: transaction hashes; state diffs folded in block order reach the committed post-block state |
| Intermediate roots | `debug_intermediateRoots` | One root per transaction; the last is the committed state root |
| Single transaction | `debug_traceTransaction`, `trace_replayTransaction`, `trace_transaction`, `trace_get`, `trace_transactionOpcodeGas`, `ots_traceTransaction`, `ots_getInternalOperations`, `ots_getTransactionError` | Each transaction's receipt (gas, success, revert output), as far as the endpoint exposes it |
| Contract creators | `ots_getContractCreator` | Every created contract names its creating transaction and creator |
| Mid-block | `debug_accountAt`, `debug_accountInfoAt`, `debug_traceCall` (`txIndex`), `eth_callMany`, `debug_traceCallMany` | The committed state at that position inside the block |
| Block execution | `reth_getBlockExecutionOutcome`; `debug_executionWitness`, `debug_executionWitnessByBlockHash` | Receipts and every state change, including writes outside transactions; witnesses must be refused as unsupported |

Once the timeline is done, `replay::check_blocks` also replays spans of many blocks in one call
(`trace_filter`, `reth_getBlockExecutionOutcome` over ranges).

The per-module docs in `replay/` explain exactly which fields each endpoint is compared on and
why.

### How failures are reported

Every difference, including an endpoint error, is recorded in the shared `MismatchReport` and the
timeline continues. Scenario assertions record into the same report. At the end the test panics
with one line per entry:

```
block <number> (<phase>) <endpoint or scenario> [tx <index>] <field>: expected <value>, actual <value>
blocks <first>..=<last> <endpoint> [tx <index>] <field>: expected <value>, actual <value>
```

While running, the test prints one line per block (`[gravity_pipe] block N at <timestamp>
(<phase>)[, epoch changed]; M mismatches so far`).

Only harness invariants panic immediately: a block that cannot be built, executed, committed or
persisted; the chain spec disagreeing with the timeline; epoch accounting (a transcript without an
epoch change, a skipped epoch, an executed old-epoch block, a phase without an epoch change, too
few epoch changes); a scenario whose phase ended before it got its block; and a failed read of the
committed chain through `Chain`.

## Adding a hardfork module

Using a hypothetical `Delta` hardfork activated by a genesis `deltaTime`:

1. **Chain spec.** The fork must be activated by a genesis timestamp key (see
   `crates/chainspec/src/gravity.rs` and the `*Time` parsing in `crates/chainspec/src/spec.rs`).
2. **Timeline** (`timeline.rs`):
   - add `Fork::Delta` in chain order and to `Fork::ALL`;
   - map it in `Fork::genesis_key` (`"deltaTime"`) and `Fork::transitions_at`;
   - add its offset to `FORK_OFFSETS`, at least one epoch interval plus margin after the previous
     fork, more if its scenarios need many blocks.
   The epoch-change requirement for the new phase follows from `Fork::ALL`. If Delta is the last
   fork, change the stop condition in `main.rs` (`Phase::After(Fork::Gamma)`) to Delta; the
   deadline follows the last fork automatically.
3. **Module** (`hardfork/delta.rs`), a `Default` struct with the same three methods as the other
   modules:
   - `next_block(...) -> Option<ScenarioBlock>`: called before each block that may carry
     scenarios (never an epoch-change block), with the block's phase and parent number. Return
     the block's transactions and extra data, or `None` to leave it to the next module. Plan one
     block at a time and remember what it must show.
   - `after_commit(chain, block, report)`: called with every committed block. Assert through
     `report.check_eq` / `report.record`, not `assert!`, so the timeline continues.
   - `assert_all_ran()`: panic if a planned scenario never got its block.
   Sign transactions with `TestAccount` (`node.rs`); add an account to `TestAccount::FUNDED` if
   it needs a genesis balance.
4. **Registration** (`hardfork/mod.rs`): declare the module, add it to `Scenarios`, and call it
   from `next_block`, `after_commit` and `assert_all_ran`. Hardfork modules come before `base` in
   `next_block`, because their phases are short.

The replay check runs on the new blocks without any change. Add a `replay/` module only for a new
class of endpoint.

## Running

```bash
cargo test -p reth-pipe-exec-layer-ext-v2 --test gravity_pipe -- --nocapture
# or, as CI does:
cargo nextest run -p reth-pipe-exec-layer-ext-v2 --test gravity_pipe
```

The test clears its data directory (`crates/pipe-exec-layer-ext-v2/execute/data/gravity_pipe`)
before starting, so no manual cleanup is needed. CI runs it in the `gravity-pipe-test` job of
`.github/workflows/integration.yml`; `.config/nextest.toml` gives it a 10-minute ceiling and no
retries.
