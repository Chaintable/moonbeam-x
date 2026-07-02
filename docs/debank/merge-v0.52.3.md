# Merge upstream v0.52.3 — verification report

Branch `merge-v0.52.3`, merge commit `0d0c86afb` (fork main + upstream `v0.52.3`),
plus follow-ups: upstream-CI removal, `libclang-dev` builder fix.

## 1. Change summary

- Fork base moves from the v0.49.3 release line to **v0.52.3** (latest stable
  client). Dependency lines move from `moonbeam-polkadot-stable2506` to
  `moonbeam-polkadot-stable2512` (polkadot-sdk, frontier, moonkit, evm, and the
  new `ethereum` fork pin).
- Notable upstream content in the increment: one-time Frontier DB migration
  (v0.50.0), eth RPC correctness fixes (`eth_getBlockByNumber` null, receipt
  race, `eth_blockNumber` 0x00, pruned-node "latest", stale block visibility,
  `eth_getLogs` filtering, EIP-7702 tx format, reorg-aware log re-emission,
  LogsJournal), litep2p as default network provider, slot-based authoring +
  dynamic relay-parent offset (client-side prep for runtime 4400+).
- Breaking CLI in the increment: `--ethapi-trace-cache-duration` →
  `--ethapi-trace-cache-size` (already present on our v0.49.3 base via
  backport); `trace_filter` bounded by `--max-block-range` (default 1024).

## 2. Conflicts and resolution

25 conflicts: 17 CI files (kept fork deletions / fork `build.yml`; also removed
four upstream workflows newly added in this increment, same policy), 4
`Cargo.toml` version bumps + one dependency-line union, `Cargo.lock`
(regenerated from upstream side; git pins verified byte-identical to v0.52.3;
fork workspace crates re-added; `alloy-primitives`/`alloy-rlp` pinned back to
pre-merge 1.4.0/0.3.12), `client/rpc/trace/src/lib.rs` (all hunks were fork
debank additions — kept), `node/service/src/rpc.rs` (kept the
`create_pending_cidp` factory feeding both `Eth` API instances; upgraded its
body to upstream's `relay_parent_offset` semantics; adapted both `Eth::new`
call sites to the new fc_rpc signature; wired `LogsJournal`), and the alphanet
embedded spec (took upstream).

## 3. Impact analysis (both directions)

- **Upstream → fork collection surfaces:** the increment does not touch
  `primitives/rpc/evm-tracing-events` nor the evm-tracing listener/formatter
  crates; the trace event surface consumed by the debank formatter is
  unchanged. Frontier RPC fixes benefit the `EthDataProvider` aux paths
  (receipts / block-by-number / estimate-gas). The EIP-7702 RPC format fix is
  inert at current runtimes (no 7702 transactions).
- **Fork → chain correctness:** fork patches remain confined to read-only RPC
  paths (trace replay, simulation, one extra non-registered `Eth` instance);
  no consensus / import / authoring / p2p surface is modified.

## 4. Build and deployment notes

- Local: `cargo check` on the six debank crates and on the `moonbeam` bin with
  default features (three native runtimes + lazy-loading) — pass.
- CI: multi-arch images built and pushed on the PR after adding `libclang-dev`
  to the builder image (stable2512 pulls clang-sys/bindgen build scripts;
  upstream applied the same fix to their production Dockerfile).
- **Operational (any rollout of this image):**
  - One-time **Frontier DB purge** required before first start of the new
    binary (v0.50.0 migration); the mapping DB is rebuilt automatically. On a
    pruned node the rebuild is bounded and fast.
  - This repo's image ships the binary only; trace support needs the tracing
    runtime overrides companion image (or a mounted overrides directory).
    Companion wasm set already covers runtimes up to 4401.
  - Network provider defaults to litep2p from v0.51.0; add
    `--network-provider libp2p` if the old stack is required.

## 5. Verification (test deployment, x86_64)

Two nodes were deployed side by side from the same recent pruned snapshot:
the merged build (`04f5c5eb`) with a purged Frontier DB, and the pre-merge
fork head (`75c58162`) as behavioral reference with its Frontier DB intact.
Both used identical production-shaped flags (pruned 7200, tracing enabled,
runtime overrides mounted, relay via RPC).

| Check | Result |
|---|---|
| Startup / CLI flag compatibility | clean; all production-shape flags accepted |
| Frontier DB purge + rebuild | mapping rebuilt in background; eth RPC live throughout |
| Catch-up to chain tip (~46k blocks) | ~35 min, 0 restarts, no errors |
| Head lag vs public RPC | 0–5 blocks (lockstep) |
| eth block hashes vs public RPC | 20/20 formal sample + spread probes within pruning window: MATCH |
| `trace_debankBlock` vs pre-merge build | 8 blocks incl. the 5 busiest recent (6–15 txs): field-by-field MATCH |
| `debank_contractMultiCall` / `debank_estimateGas` / `debank_simulateTransactions` | outputs identical across builds (pinned block context) |
| Sampler checks (startup clean / samples / head growth / synced / lag) | PASS (7/7) |

Known pre-existing non-determinism (present identically on the pre-merge
build, both nodes unstable across repeated calls on the same block;
`validation_hash` always agrees):

- `block_file.block.process_start_timestamp` — node-local wall clock.
- `block_file.storage_contracts` ordering — set semantics, map iteration
  order; compared as sets above.

Also pre-existing (baseline, cosmetic): prometheus registration of
`frontier_eth_trace-filter-blocks-cache_hits` fails (dashes in metric name);
identical on the pre-merge build and upstream v0.52.3.

**Verdict: PASS.**
