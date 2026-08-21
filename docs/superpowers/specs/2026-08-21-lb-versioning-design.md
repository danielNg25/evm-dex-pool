# Trader Joe Liquidity Book — Version Abstraction & V2.0 Support

**Date:** 2026-08-21
**Status:** Design — awaiting review
**Base branch:** `feature/trader-joe` (tip `8e25b8f`, v1.4.0)

## 1. Context

A working Liquidity Book implementation already exists on `origin/feature/trader-joe` and is
published as `evm-dex-pool` 1.4.0. It is consumed in production by `evm-dex-arbitrage` and
`arbitrage-bot-dashboard-be`, both pinned to `version = "1.4.0"`. Local `master` is v1.1.0 and
contains none of it.

The existing code covers LB v2.1 and v2.2 and is structurally sound: bins are flattened into
`BTreeMap<u32, (u128, u128)>`, swap math is a faithful port of `getSwapOut`/`getSwapIn`, and
`apply_log` dispatches on `topic0` — the same shape `UniswapV3Pool` uses for its six dialects.

What it lacks is any notion of *version*. Nothing on `LBPool` records which LB generation a pool
is, so nothing can branch on it. The only version-dependent behaviour is two bare constants in
`src/lb/fetcher.rs` — `LB_TREE_BASE_SLOT_V22 = 7` and `LB_TREE_BASE_SLOT_V21 = 8` — used to read
the private `_tree` bitmap out of contract storage, tried in order with a sequential walk as
fallback.

## 2. Goals

- Introduce an explicit `LBVersion` so version-dependent behaviour is named rather than implied.
- Add LB v2.0 support (a total ABI break from v2.1).
- Replace probe-and-guess classification with a positive discriminator.
- Fix the wall-clock time source that makes event-replayed state diverge from fetched state.
- Keep `evm-dex-pool` 1.4.0 consumers compiling without changes.

## 3. Non-goals

- No changes to `src/lb/math.rs`. The swap, fee, and price math is **identical across all three
  versions** — the version axis does not reach it.
- No bin-map windowing. Registry size is dozens to low hundreds of pools; full bin maps are
  affordable and windowing would reintroduce the truncation problem the current design avoids.
- No merge of `feature/trader-joe` into `master`. Work happens directly on the branch.
- No LB hooks *execution* modelling (v2.2). Hooked pools are detected and flagged, not simulated.

## 4. What actually varies by version

This table is the organising principle for the whole design.

| Axis | v2.0 → v2.1 | v2.1 → v2.2 |
|---|---|---|
| Swap / fee / price math | **identical** | **identical** |
| State getters | total break — renames, arg-order flip | additive only |
| Event ABIs | renamed and redesigned | **byte-identical** |
| Bin tree storage slot | different structure | 8 → 7 (ReentrancyGuard `_status` shift) |
| Hooks | — | new (`getLBHooksParameters`) |

Version therefore affects exactly three things: **fetching**, **event decoding**, and **hooks
awareness**. This mirrors `V3PoolType`, whose six variants share one `v3_swap` and differ almost
entirely inside `fetcher.rs`.

### 4.1 Verified interface differences

v2.0 getters (all absent or renamed in v2.1+):

- `getReservesAndId() → (uint256 reserveX, uint256 reserveY, uint256 activeId)` `[0x1b05b83e]`
- `feeParameters() → FeeParameters` — one 12-field struct `[0x98c7adf3]`, field order:
  `binStep, baseFactor, filterPeriod, decayPeriod, reductionFactor, variableFeeControl,
  protocolShare, maxVolatilityAccumulated, volatilityAccumulated, volatilityReference,
  indexRef, time`
- `findFirstNonEmptyBinId(uint24 id, bool sentTokenY) → uint24` `[0x8f919a83]`
- `factory() → address`
- No `getBinStep()`, no `getActiveId()`, no `getSwapIn`/`getSwapOut` on the pair (router only).

v2.1+ getters: `getReserves()` `[0x0902f1ac]`, `getActiveId()` `[0xdbe65edc]`,
`getBinStep()` `[0x17f11ecc]`, `getStaticFeeParameters()` `[0x7ca0de30]`,
`getVariableFeeParameters()` `[0x8d7024e5]`, `getNextNonEmptyBin(bool, uint24)`, `getFactory()`.

v2.2 adds only: `implementation()`, `getLBHooksParameters()`, `setHooksParameters(bytes32,bytes)`,
and the `HooksParametersSet` event.

### 4.2 Event topic0s

| Event | topic0 |
|---|---|
| v2.1+ `Swap(address,address,uint24,bytes32,bytes32,uint24,bytes32,bytes32)` | `0xad7d6f97…` |
| v2.0 `Swap(address,address,uint256,bool,uint256,uint256,uint256,uint256)` | `0xc528cda9…` |
| v2.1+ `DepositedToBins` | `0x87f1f9dc…` |
| v2.1+ `WithdrawnFromBins` | `0xa32e1468…` |
| v2.1+ `StaticFeeParametersSet` | `0xd09e5ddc…` |
| v2.1+ `CompositionFees(address,uint24,bytes32,bytes32)` | `0x3f0b4672…` |
| v2.0 `CompositionFee(address,address,uint256,uint256,uint256)` | `0x56f8e764…` |

Because v2.0 and v2.1+ `Swap` have different topic0s, **event decoding disambiguates itself**. The
`version` field is consulted only for the reserve-accounting difference described in §6.3.

`Swap` is emitted **once per bin crossed**, in both generations. A single `swap()` crossing N bins
produces N logs from the same pair in one transaction.

### 4.3 Packed amount encoding (v2.1+)

`bytes32` amounts pack **X in the low 128 bits, Y in the high 128 bits**. In big-endian bytes:
`[0..16] = amountY`, `[16..32] = amountX`. The existing `decode_amounts` in `math.rs` matches this
and needs no change.

## 5. Design

### 5.1 `LBVersion`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LBVersion {
    V2_0,
    V2_1,
    V2_2,
}

/// Persisted 1.4.0 snapshots predate v2.0 support and are all v2.1/v2.2.
impl Default for LBVersion {
    fn default() -> Self { Self::V2_1 }
}
```

Added as a field on the existing struct:

```rust
pub struct LBPool {
    pub address: Address,
    #[serde(default)]
    pub version: LBVersion,
    // ... all existing fields unchanged ...
    /// v2.2 only. Non-zero means the pair has hooks installed and its
    /// observable behaviour may deviate from pure LB math.
    #[serde(default)]
    pub hooks_parameters: Option<B256>,
}
```

`#[serde(default)]` on `version` keeps persisted 1.4.0 pool snapshots deserialisable.
`persistence.rs` in `evm-dex-arbitrage` round-trips these.

**`PoolType::TraderJoeLB` stays flat.** Nesting it as `TraderJoeLB(LBVersion)` — mirroring
`ERC4626(ERC4626Pool)` — was considered and rejected: `PoolRegistry::get_pools_by_type` and
`get_addresses_by_type` compare `PoolType` by **exact equality** (`registry.rs:135`, `:164`), so
nesting would mean no single call returns "all LB pools", and it would break the five downstream
files already matching on the flat variant.

The cost of staying flat is that `TopicList::topics()` is a static method, so every LB pool
registers the union of v2.0 and v2.1+ topics, widening the global log filter. This is the tradeoff
the repo already accepted for V3 (six topics for six dialects) and is an over-fetch, not a
correctness issue: the filter is also address-scoped, and a given pair only ever emits its own
generation's events.

### 5.2 Version detection

Replaces the current `getBinStep()` probe, which **misclassifies every v2.0 pair**: v2.0 has no
`getBinStep()`, fails `liquidity()`, and falls through to the unconditional `Ok(PoolType::UniswapV2)`
default — then `fetch_v2_pool` calls `token0()`, which reverts on an LB pair, and
`fetch_pools_into_registry` panics after retries. Today a v2.0 address in the fetch list is a
bootstrap-killing landmine.

One `try_aggregate(false)` multicall, evaluated in order:

| Probe succeeds | Conclusion |
|---|---|
| `getLBHooksParameters()` | `TraderJoeLB` / `V2_2` |
| `getFactory()` | `TraderJoeLB` / `V2_1` |
| `factory()` **and** `feeParameters()` | `TraderJoeLB` / `V2_0` |
| `liquidity()` | `UniswapV3` |
| none | `UniswapV2` (unchanged fallback) |

`factory()` is paired with `feeParameters()` because `factory()` alone is too weak a signal — many
non-LB contracts expose it. This is a **positive** discriminator: each version is identified by a
method only that version has, rather than by the absence of something.

Note the selector collision worth guarding: LB v2.1+ `getReserves()` is `0x0902f1ac`, byte-identical
to UniswapV2's `getReserves()`, but returns two words instead of three. Detection must not rely on
it.

`identify_pool_type` returns `PoolType`, which carries no version, so the version must reach the
fetcher. Rather than widen that return type (it is public API), `fetch_lb_pool` re-runs the same
cheap multicall internally. The redundant call costs one multicall per LB pool at fetch time only.

### 5.3 Contract bindings

Add a second binding pair alongside the existing `ILBPair` / `RpcILBPair`, following the repo's
established split and `Rpc` prefix convention:

- `src/contracts.rs`: `sol! { ILBPairV20, "contracts/ABI/ILBPairV20.json" }` — event decoding.
- `src/contracts_rpc.rs`: `sol! { #[sol(rpc)] RpcILBPairV20, "contracts/ABI/ILBPairV20.json" }`.
- New `contracts/ABI/ILBPairV20.json`, a raw top-level ABI array to match all 23 existing files.

The existing `ILBPair` covers v2.1 **and** v2.2, since their event ABIs are byte-identical and
v2.2's additions are getters. Two entries must be added to `ILBPair.json`, which today holds 12
functions and 4 events:

- `getLBHooksParameters()` — required by the §5.2 discriminator.
- `CompositionFees` — the event is absent from the ABI entirely, which is why §6.3's drift source
  is currently invisible to `apply_log`.

`getFactory()` is already present and needs no change.

### 5.4 Fetcher: per-version paths

`fetch_lb_pool` branches once on the detected version and converges on the same `LBPool`.

**State fetch**
- v2.0: `getReservesAndId()` + `feeParameters()` (unpack the 12-field struct into the flat fields)
  + `tokenX()` / `tokenY()`.
- v2.1/v2.2: existing path — `getReserves()`, `getActiveId()`, `getBinStep()`,
  `getStaticFeeParameters()`, `getVariableFeeParameters()`, `getTokenX()`, `getTokenY()`.
- v2.2 additionally: `getLBHooksParameters()` → `hooks_parameters`.

**Bin discovery**
- v2.1: 3-level storage bitmap at base slot **8** — chosen from the known version, not guessed.
- v2.2: same, base slot **7**.
- v2.0: `findFirstNonEmptyBinId` walk outward from `activeId` in both directions. v2.0's bin tree
  is a different on-chain structure and its layout is not corroborated by any source, so the
  storage-read shortcut is not extended to it. This is O(N) sequential RPC and slower — acceptable
  because v2.0 pools are legacy and few.

**Fallback hardening.** The current chain ends at `vec![active_id]`, producing a pool that "works"
while pricing exactly one bin — a silent, expensive lie. Change it to return an error, matching the
V3 convention where an unfillable swap is `Err`, never a partial answer.

### 5.5 Time source

`apply_log` currently stamps `time_of_last_update` and `last_updated` with `chrono::Utc::now()`,
while `fetch` seeds `time_of_last_update` from the chain's `timeOfLastUpdate`. The variable fee
decays on `now - time_of_last_update`, so mixing wall clock into a chain-time field is both a
quote-accuracy bug and the reason replayed state can never equal fetched state.

Use `Log.block_timestamp` when present, falling back to wall clock only when the log carries none.
`EventApplicable::apply_log` already receives the full `alloy::rpc::types::Log`, so no signature
change is needed.

This is a **prerequisite** for the §7.2 acceptance test, not an optimisation.

### 5.6 Trait addition

Per the earlier decision to evolve `PoolInterface` additively:

```rust
pub struct QuoteContext { pub timestamp: u64 }

pub trait PoolInterface {
    fn calculate_output(&self, token_in: &Address, amount_in: U256) -> Result<U256>;

    fn calculate_output_at(
        &self, token_in: &Address, amount_in: U256, _ctx: &QuoteContext,
    ) -> Result<U256> {
        self.calculate_output(token_in, amount_in)
    }

    // calculate_input_at mirrors this.
}
```

V2, V3, and ERC4626 inherit the default and are untouched, so `evm-dex-arbitrage` compiles without
edits. `LBPool` overrides it, delegating to the `simulate_swap_out_at` / `simulate_swap_in_at`
functions that already exist and already take an explicit timestamp. `LBPool::calculate_output`
keeps its current wall-clock behaviour so generic call sites still work.

## 6. Event handling

### 6.1 Dispatch

`apply_log` keeps matching on `topic0` and gains arms for the v2.0 events. No version check is
needed to *select* a decoder.

### 6.2 v2.1+ (unchanged)

`bin[id] += amountsIn - amountsOut`, where `amountsIn` is `amountsInWithFees` — LP fee included,
protocol fee already subtracted — which is exactly what the contract credits to the bin.

### 6.3 v2.0 (new, and the risk)

v2.0's fee model is materially different: LP fees are **claimable** (`collectFees` / `pendingFees`),
not auto-compounded, and `getReservesAndId()` reports reserves excluding **all** fees. The `Swap`
event's `amountIn` is `_amountInToBin`, excluding all fees, with a separate `fees` field.

Whether `bin[id] += amountIn - amountOut` is correct for v2.0 therefore depends on whether v2.0's
`getBin()` also excludes fees. The sources do not settle this, and **it must not be assumed**. The
§7.2 convergence test is the instrument that decides it: if replayed and refetched bin reserves
diverge on a v2.0 pool, the `fees` field needs folding in.

`CompositionFee` (v2.0) / `CompositionFees` (v2.1+) are currently **not handled** and not
registered in `topics()`. Composition fees are credited to the active bin, so ignoring them is a
candidate source of drift. They are added to both the topic list and `apply_log`, and the
convergence test will confirm whether they matter.

## 7. Testing

Two acceptance tests define done. Both hit live RPC and are `#[ignore]`d, matching the existing
integration-test convention.

### 7.1 Offchain vs onchain quote parity

Extends the existing `tests/lb_pool.rs` fuzz harness, which already compares
`simulate_swap_out_at` / `simulate_swap_in_at` against on-chain `getSwapOut` / `getSwapIn` across
13 amount multipliers at a pinned block.

Additions:
- v2.0 pool fixtures. v2.0 pairs have **no `getSwapOut` on the pair**, so the on-chain reference
  must come from the v2.0 **router**'s quoter — a different call path than v2.1+.
- Assert exact equality, not tolerance. The math is integer and deterministic; any divergence is a
  bug.

### 7.2 Collector event-replay convergence

New test, and the primary correctness gate.

1. Fetch pool state at block **A** into a registry.
2. Run the collector from **A** to **B**, applying all logs.
3. Fetch the same pools **fresh** at block **B** into a second registry.
4. Assert the two are **identical**.

Compared field-by-field: `active_id`, `bins` (entire map, bin-for-bin), `bin_step`, all static fee
parameters, `volatility_accumulator`, `volatility_reference`, `id_reference`,
`time_of_last_update`.

Excluded: `last_updated` and `created_at` — local bookkeeping, not chain state.

Fixtures span all three versions so the v2.0 accounting question in §6.3 is actually exercised.

**Expected first-run failures.** This test asserts an invariant the current code does not satisfy,
so it fails before it passes. Each failure is a pre-existing defect in shipped 1.4.0 that is
currently invisible — none is a reason to loosen the assertion.

*Certain — fix before the test can pass:*
- Wall-clock vs block timestamp (§5.5). `apply_log` stamps `time_of_last_update` with
  `chrono::Utc::now()` while `fetch` reads it from chain, and `update_references` decays volatility
  on `dt = timestamp - time_of_last_update` (`pool.rs:139`). The two sources essentially never
  agree, and the error propagates into fee and quote output.

*Likely — hypotheses the test adjudicates:*
- Unhandled `CompositionFee(s)` (§6.3), credited to the active bin.
- v2.0 bin accounting (§6.3), once v2.0 fixtures exist.

*Possible:*
- Flash-loan fees credited to bins with no event this implementation observes.

*Excluded rather than failed:*
- v2.2 pools with non-zero `hooks_parameters`, whose state may move outside LB math. Held out of
  this test and asserted separately to be flagged.

Zero-reserve bins are **not** a source of divergence: `update_bin` removes bins that drain to
`(0, 0)` (`pool.rs:128-132`) and `fetch` only inserts bins with non-zero reserves
(`fetcher.rs:306`). Both sides already normalise symmetrically.

## 8. Backward compatibility

`evm-dex-arbitrage` and `arbitrage-bot-dashboard-be` pin `evm-dex-pool = "1.4.0"` and reference
`LBPool` / `PoolType::TraderJoeLB` across five files.

- `PoolType::TraderJoeLB` keeps its shape — no match arms break.
- `LBPool` keeps its name and all existing public fields; `version` and `hooks_parameters` are
  additive with `#[serde(default)]`, so persisted snapshots still load.
- `PoolInterface` gains only defaulted methods.
- `identify_pool_type` keeps its signature.

The one deliberate behaviour change is that a v2.0 address, which today panics the bootstrap, now
fetches correctly. Release as a **minor** version bump.

## 9. Risks

- **v2.0 bin accounting (§6.3)** — unresolved by research; §7.2 adjudicates. Highest-uncertainty
  item in this design.
- **v2.0 bin discovery is O(N) sequential RPC.** Slow on wide pools. Acceptable given v2.0 is
  legacy; revisit only if a v2.0 pool proves too slow to bootstrap.
- **Storage-slot coupling persists for v2.1/v2.2.** Explicit version selection removes the guessing
  but not the dependence on a private, undocumented layout. A silent LB upgrade would break bin
  discovery quietly; the §7.2 test is the tripwire.
- **`clone_box()` deep-clones the full bin map**, and the pending-block path clones per pool per
  batch. Not introduced by this work, but adding v2.0 pools widens the exposure. Out of scope,
  worth measuring.
