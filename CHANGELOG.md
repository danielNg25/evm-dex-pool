# Changelog

All notable changes to `evm-dex-pool` will be documented in this file.

## [1.8.0]

### Changed

- **Ramses-family fee changes are applied from `FeeAdjustment` events, not
  polled.** Every Ramses-family CL pool (Pharaoh, Shadow, Nile, Cleo) emits
  `FeeAdjustment(uint24 oldFee, uint24 newFee)` when its fee moves.
  `UniswapV3Pool::apply_log` now handles it, and the event is in
  `UniswapV3Pool::topics()` so the collector fetches it.

  This fixes an ordering bug, not only a cost. The per-batch refetch runs
  after a batch's swaps have been forwarded to the simulator, so block N was
  priced on block N-1's fee. A fork replay of run 6 found this behind 5 of 9
  failing cycles out of 299. The worst was `0x71bd7525`, priced at fee 800
  while the chain had moved to 5500 in that block. `process_confirmed_events`
  applies a whole batch before forwarding any swap, so an event-driven fee is
  current when the block's cycles are priced, and it follows log order within
  the block.

  A `FeeAdjustment` whose `oldFee` disagrees with the fee held logs a warning:
  an earlier change was missed. The new fee is applied either way.

- **`RamsesCL` pools are no longer tracked for fee refetch.** With their fees
  event-driven, polling them only spends calls. `PoolRegistry::add_pool`
  tracks `AlgebraV3` alone, since the Algebra fee plugin still emits nothing
  the collector consumes.

- **Ramses V3 `Mint` is applied.** Ramses-family pools emit Uniswap V3's
  `Mint` with an NFT position `index` added (topic `0xd78218c0…`), which was
  never fetched while their unchanged `Burn` was applied. Run 8 logged 588
  "Burn attempted on uninitialized tick" errors across four pools in 12 hours,
  and three replayed opportunities priced a pool whose live tick the map had
  lost. A `Burn` now checks both ticks before changing either.

### Notes

- `FeeAdjustment` is deliberately absent from `profitable_topics()`. A fee
  change updates state but does not start a cycle search by itself.
- Topics are registered at pool fetch time. A pool snapshot saved by 1.7.x
  restores the old topic list, but 1.7.x snapshots cannot deserialize under
  1.7+ anyway (`UniswapV3Pool` lost a field), so a clean fetch is already
  required.

### Tests

- Four unit tests in `v3::pool`, built from the real `FeeAdjustment` log bytes
  of block 96081609 rather than the binding's own encoding: topic pinned to
  the deployed contract, fee applied, recovery from a missed earlier change,
  and fetched-but-not-a-trigger.
- `registry::tests` now pins that `RamsesCL` is not tracked.

## [1.7.1]

### Fixed

- **A V2 `fee()` reading above 10000 wrapped to near `U256::MAX`.** The
  resolver treated any value over `REVERSE_FEE_MAX` as the complement of a
  1e4-basis fee and computed `GET_FEE_MAX - fee` on unsigned `U256`. A pool
  reporting 15000 underflowed, and every later `fee.to::<u128>()` panicked --
  29,025 times in one 42-hour run, aborting the whole simulation task each
  time and costing roughly 40% of all simulations. Such a value cannot be a
  1e4-basis fee (over 100%) nor a complement (negative), so it is now
  recognised as already being in the 1e6 basis the crate stores.

- **An ambiguous reading was silently multiplied instead of resolved.** Three
  conventions share one number space -- a fee in 1e4 basis (30 = 0.3%), its
  complement (9970 = 0.3%), and a fee already in 1e6 basis (3000 = 0.3%) --
  and magnitude cannot separate the last two. Where the pool exposes
  `getAmountOut`, the fee is now back-solved from a quote it gives itself:

      out = (ain*(1-f)*r1) / (r0 + ain*(1-f))   =>   f = 1 - out*r0/(ain*(r1-out))

  an exact inversion of the curve, slippage included. Two reserve-proportional
  probes go out in one multicall and must agree, so a pool on another curve
  falls back rather than being priced on a guess. Small probes are unusable:
  `getAmountOut` returns an integer, and truncation read 15169 where the fee
  was 15000. Stable pools are excluded -- their curve is x^3y+y^3x -- as are
  pools whose fee comes from a `factory_to_fee` override, which is an explicit
  operator choice.

  This one was silent. `0x903c3ed1` on Avalanche reports 5000 and charges 0.5%;
  it was priced at 50% for a whole run, never panicking and never producing a
  cycle.

### Added

- A fee resolving above 10% of notional now fails the pool's load, naming it.
  A reading that large is a misparse, and an absent pool costs less than a
  silently mispriced one.

### Tests

- `v2::fetcher::tests::no_reading_wraps` pins the regression: no input may
  produce a fee beyond 100% of notional.
- `tests/v2_fee_calibration.rs` (live, `--ignored`) resolves the three
  Avalanche pools on factory `0x85448bf2` to 1.5%, 1.5% and 0.5%.

## [1.7.0]

### Removed

- **The Ramses `ratio_conversion_factor` calibration is gone.** A `RamsesV2`
  pool used to have its quotes post-scaled by a constant measured once at fetch
  time: `calculate_ratio_conversion_factor` quoted 1e11 units through the Ramses
  quoter in both directions, compared each against this crate's own V3 math, and
  kept the smaller ratio (minus 1 wei). That constant corrected a divergence
  whose cause was unknown at the time. The cause was the mutable `fee()`
  addressed in 1.6.0, so the correction is now redundant -- and it carried the
  same staleness flaw it was compensating for, since it was never recomputed
  after startup. It also applied to `calculate_exact_input` only, leaving
  `calculate_exact_output` uncorrected.

  Removed with it: `UniswapV3Pool::ratio_conversion_factor` (public field),
  `UniswapV3Pool::update_ratio_conversion_factor`, `RAMSES_FACTOR`,
  `calculate_ratio_conversion_factor`, `RAMSES_FACTORIES`, `is_ramses_factory`
  and `get_ramses_quoter`.

### Changed

- **Breaking: `UniswapV3Pool` lost a field.** `bincode`-persisted pools from
  1.6.0 or earlier will not deserialize. Clear the pool snapshot; it rebuilds
  from chain on the next run.
- **Breaking: `V3PoolType::RamsesV2` is removed.** With the factory allowlist
  gone nothing assigned it, and those pools now fall through to the
  `lastPeriod()` check and classify as `RamsesCL` -- which puts them on the
  mutable-fee refetch list, the correct treatment and what the calibration
  constant was standing in for. Downstream `match` arms over `V3PoolType` must
  drop their `RamsesV2` arm; it belongs with `UniswapV3` wherever it appears.
  Removing it also shifts the positional indices `bincode` assigns to the later
  variants, but no persisted pool survives this release anyway (see the field
  removal above), so there is no old data to protect.
- All V3 variants now quote through identical math, with no post-scaling on any
  path. Pinned by `v3::pool::tests::all_v3_variants_quote_identically`.

### Tests

- `tests/ramses_cl_detect.rs` gains `former_ramses_v2_pool_now_classifies_as_ramses_cl`,
  covering `0x0021368B…` -- the one pool deployed by the formerly hardcoded
  factory. It answers `lastPeriod()` like any Ramses-family pool, but the
  allowlist used to claim it first and pin it to `RamsesV2`, excluding it from
  fee refetch.

## [1.6.0]

### Added

- **Ramses-family concentrated-liquidity support** — new `V3PoolType::RamsesCL`
  for Pharaoh (Avalanche), Shadow, Nile, Cleo and Ramses CL. These forks are
  Uniswap V3-shaped and keep `slot0()`, so the Algebra discriminator
  `globalState()` never saw them and they were classified as plain
  `UniswapV3` with a fee frozen at startup. They are detected by the
  `lastPeriod()` view (selector `0xd340ef8a`), which answers on the
  Ramses family and reverts on both Uniswap V3 and Algebra; it is added as
  call 13 of the existing `fetch_v3_pool` multicall, so detection costs no
  extra round trip. The check runs only after every existing branch, so
  Algebra variants and true `RamsesV2` pools keep their classification.
- `RamsesCL` pools are registered for periodic fee refetch alongside Algebra,
  so their mutable fee tracks the chain instead of drifting.

### Changed

- **Breaking: `V3PoolType` gained a variant.** Exhaustive `match` expressions
  over `V3PoolType` in downstream code must add a `RamsesCL` arm. It should be
  grouped with `UniswapV3`/`RamsesV2` wherever routing or swap math is
  selected. The variant is appended last so `bincode`-persisted pools keep
  their positional indices.
- `RamsesCL` deliberately uses **identical math to `UniswapV3`**. It does not
  get the quoter-calibrated `ratio_conversion_factor` that `RamsesV2` applies:
  Pharaoh's factory is absent from `RAMSES_FACTORIES`, so `get_ramses_quoter`
  returns `None` and there is nothing to calibrate against. Pinned by
  `v3::pool::tests::ramses_cl_quotes_identically_to_uniswap_v3`.
- **Breaking: dynamic-fee tracking renamed on `PoolRegistry`** — the tracking
  set now covers more than Algebra. `add_algebra_v3_address`,
  `remove_algebra_v3_address` and `get_algebra_v3_addresses` become
  `add_dynamic_fee_address`, `remove_dynamic_fee_address` and
  `get_dynamic_fee_addresses`.
- **Breaking: `collector::algebra_fee_refetch` is now
  `collector::dynamic_fee_refetch`**, and `refetch_algebra_v3_fees` is
  `refetch_dynamic_fees`. `CollectorConfig::refetch_algebra_fee` is
  **unchanged** and still the flag that enables it.
- `PoolRegistry::add_pool` now registers a mutable-fee pool for refetch itself,
  and `remove_pool` untracks it. Tracking previously lived in a private helper
  on the fetch path only. The internal helper is gone; no public API changed.

### Fixed

- **The fee refetch never ran when pools were restored from a snapshot.** Pools
  loaded that way go in through `add_pool` directly, and
  `fetch_pools_into_registry` skips addresses already in the registry -- so the
  fetch-path helper that populated the refetch set never saw them and
  `get_dynamic_fee_addresses()` came back empty. Every Algebra pool in that mode
  was quoting on a fee frozen at startup. Tracking now happens inside
  `add_pool`, so every insertion path is covered by construction.
- `remove_pool` left the address in the refetch set. Callers that remove pools
  directly (e.g. a liquidity filter) grew the set without bound and wasted
  multicall slots on pools that were no longer held.
- The periodic fee refetch read `fee()` through the Algebra ABI, which types it
  `uint16`. Ramses-family fees are `uint24`, so a pool on a tier above 65535
  (e.g. 100000 = 10%) would have failed to decode and silently kept its stale
  fee. The refetch now reads `fee()` through the Uniswap V3 ABI (`uint24`);
  Algebra's `uint16` response still decodes correctly, since both ABI-encode to
  a 32-byte word.

### Tests

- `tests/ramses_cl_detect.rs` (live, `--ignored`) classifies three Avalanche
  pools: Pharaoh `0xFf0855A9…` → `RamsesCL`, Uniswap V3 `0x7b602f98…` →
  `UniswapV3`, Algebra `0x23fF0B53…` → `AlgebraV3`.
- `contracts_rpc::tests::last_period_selector_matches_deployed_contracts` pins
  the `lastPeriod()` selector.
- `registry::tests::add_pool_tracks_mutable_fee_pools` and
  `remove_pool_untracks_mutable_fee_pools` cover the snapshot-restore shape:
  insertion through `add_pool` alone, with no fetch path involved.

## [1.5.0]

### Added

- **Trader Joe Liquidity Book v2.0 support** — `LBVersion` (`V2_0`/`V2_1`/`V2_2`)
  now drives state fetching, event decoding and bin discovery.
  `detect_lb_version` identifies a pair by a *positive* discriminator unique to
  each generation (`getLBHooksParameters()` → v2.2, `getFactory()` → v2.1,
  `factory()` + `feeParameters()` → v2.0). v2.0 pairs are read through
  `feeParameters()`/`getReservesAndId()`/`getBin(uint24)` and discovered with
  the `findFirstNonEmptyBinId` walk; v2.1/v2.2 keep the storage-bitmap read.
  Swap, fee and price math is unchanged and shared across all three.
- `LBPool::version` and `LBPool::hooks_parameters` — see the breaking note below.
  `hooks_parameters` is v2.2-only; a non-zero value logs a warning that the
  pair has hooks installed and may deviate from pure LB math.
- **`QuoteContext` and time-aware quotes** — `PoolInterface::calculate_output_at`
  and `calculate_input_at` take a `&QuoteContext { timestamp }`. Both are
  **defaulted** to the existing timeless methods, so no consumer change is
  required; only `LBPool` overrides them, decaying its variable fee against the
  timestamp the swap is expected to execute at instead of its last-seen one.
- `evm_dex_pool::collector::enrich_log_timestamps` — fills `block_timestamp` on
  logs whose endpoint omitted it (Avalanche's `eth_getLogs` does), one header
  fetch per distinct block. Every block source, and the WebSocket bootstrap,
  calls it on each batch — but only when the registry actually holds LB pools,
  so registries without them pay nothing. Logs whose header cannot be read keep
  `None` rather than getting a wrong timestamp.
- Integration tests: `tests/lb_version_detect.rs` (live version detection per
  generation), plus v2.0 quote-parity and event-replay convergence ranges in
  `tests/lb_pool.rs` and `tests/lb_convergence.rs`.

### Changed

- **Breaking (persisted state): `LBPool` gained two trailing fields** —
  `version: LBVersion` and `hooks_parameters: Option<B256>`. They carry
  `#[serde(default)]` and sit at the end of the struct deliberately, so JSON
  snapshots still load. **bincode is positional and ignores serde defaults**,
  so an LB snapshot written by 1.4.0 hits EOF on these fields and fails to
  deserialize. Downstream persists pools with bincode, which means **every
  stored LB pool is rejected at boot and re-fetched from chain**. Expect a
  one-off burst of LB deserialization errors and a full LB re-fetch on the
  first start after upgrading; it does not repeat. Being trailing is what makes
  the failure clean — an old record fails at EOF instead of silently misparsing
  every field after the insertion point. Non-LB pool snapshots are unaffected.
  `version` defaults to `V2_1`, matching what 1.4.0 could only have written.
- **`LBPool::apply_log` now takes chain time from the log's block timestamp**
  rather than the wall clock. The contract decays `volatilityReference` against
  `timeOfLastUpdate`, so wall-clock stamping made replayed state drift from
  freshly-fetched state permanently. **This changes quote output for existing
  v2.1/v2.2 pools**, not only new v2.0 ones — it is a correctness fix, and the
  convergence tests now assert replayed state matches a fresh fetch exactly.
  `LBPool::last_updated` stays on wall clock: it is local bookkeeping ("when
  this process last touched the pool"), not chain state.
- **`LBPool::topics()` grew from 4 to 7 entries** — the four v2.1+ topics plus
  v2.0's `Swap`, `DepositedToBin` and `WithdrawnFromBin`; `profitable_topics()`
  grew from 1 to 2 with v2.0's `Swap`. Collectors that cache a topic list must
  refresh it or they will silently miss every v2.0 event.
  v2.0's `CompositionFee` is deliberately **not** subscribed: the pair subtracts
  the composition fee from the amount it adds to the bin reserve *and* from the
  amount it emits in `DepositedToBin`, routing the fee to `accTokenXPerShare`
  (the claimable-fee accumulator, which `getBin()` does not read), so
  `DepositedToBin` alone already carries the full reserve delta. Verified on
  chain against pair `0x18332988456C4Bd9ABa6698ec748b331516F5A14`: on tx
  `0xf2487ef13cac53fd66f0b80388a1d266d85f76b72110de3524edfac6f00eb377`
  (block 22_456_848) `getBin(8_388_609)` moves by exactly the `DepositedToBin`
  amounts and not by the `CompositionFee` on top. This mirrors the existing
  v2.1+ policy for `CompositionFees`.
- **`fetch_lb_pool` no longer fails a pool whose bin tree is empty** — a zero
  tree root is the ordinary storage state of a fully-drained or never-funded
  pair (LBPair's `_burn` calls `_tree.remove(id)` as bins empty), so such a pair
  now registers with an empty bin map and quotes `Insufficient liquidity in LB
  pool`. Previously this was a hard error; because it is deterministic it
  survived every retry and made `fetch_pools_into_registry` return `Err`,
  abandoning every remaining pool in every remaining chunk. A *non-zero* root
  with no reachable leaves is still a hard error — that shape really does
  indicate a storage-layout change.
- **`identify_pool_type` probes LB through `detect_lb_version`** instead of
  `getBinStep()`. `getBinStep()` exists only on v2.1+, so v2.0 pairs used to
  fall through to the `UniswapV2` default and panic the bootstrap inside
  `fetch_v2_pool`.
- `fetch_lb_pool` propagates a failed `getLBHooksParameters()` call instead of
  swallowing it, so "no hooks" and "couldn't check" stay distinguishable.

### Fixed

- **LB swap replay did not reproduce `updateReferences()`** — `apply_log` set
  `id_reference` to the swap's *final* bin instead of the pre-swap `activeId`,
  and never wrote `volatility_reference` at all. Both are now recomputed from
  pre-swap state before the event's own state is applied, matching
  `LBPair.swap()`, which runs `updateReferences()` once before its bin loop.
  This affects v2.1/v2.2 quotes as well as v2.0.
- **v2.0 event decoding no longer panics on out-of-range values** — v2.0's
  event ABI widens bin ids, amounts and accumulators to `uint256`. `apply_log`
  narrowed them with `U256::to()`, which panics rather than truncating; a panic
  there kills the collector task while it holds a pool write lock, silently
  stopping all pool updates. Every narrowing now returns `Err`, which callers
  already log and skip. The `try_into().unwrap_or(0)` in `WithdrawnFromBin` is
  gone too: it subtracted nothing on overflow, leaving withdrawn liquidity in
  the bin and over-quoting.

## [1.4.0]

### Added

- **Optional dedicated provider for Algebra V3 fee refetch** —
  `start_collector` now takes a `algebra_refetch_provider: Option<Arc<P>>`
  argument. When `Some`, the per-batch Algebra V3 fee multicall is sent on
  this provider instead of the main one, letting callers point fee refetches
  at a different RPC endpoint than the one driving event ingestion. When
  `None`, the main provider is reused (existing behavior).

### Changed

- **Breaking: `start_collector` signature** — gained a final
  `algebra_refetch_provider: Option<Arc<P>>` parameter. Pass `None` to
  preserve existing behavior.
- **Breaking: `UnifiedPoolUpdater::new`** — gained a final
  `algebra_refetch_provider: Option<Arc<P>>` argument.
- **Breaking: `CollectorHandle::new`** — gained a
  `algebra_refetch_provider: Option<Arc<P>>` argument (after `provider`).

## [1.3.0]

### Added

- **Algebra V3 dynamic fee refetch** — opt-in `CollectorConfig::refetch_algebra_fee` flag.
  When enabled, after each event batch the collector multicalls `fee()` on every tracked
  Algebra V3 pool (`V3PoolType::AlgebraV3`) and writes the fresh fee back into the registry.
  Required for chains where Algebra plugins update the swap fee per-block via an oracle,
  without emitting an on-chain event.
- `PoolRegistry::add_algebra_v3_address`, `remove_algebra_v3_address`, and
  `get_algebra_v3_addresses` — backing store for tracked Algebra V3 pool addresses.
- `UniswapV3Pool::set_fee` — mutate the cached fee.
- `evm_dex_pool::collector::refetch_algebra_v3_fees` — public helper for callers who want
  to trigger the refetch manually outside the collector loop.

### Changed

- **Breaking: `CollectorConfig`** gained a `refetch_algebra_fee: bool` field. Existing
  callers must add `refetch_algebra_fee: false` to their struct literal.
- **Breaking: `UnifiedPoolUpdater`** is now generic over the provider type
  (`UnifiedPoolUpdater<P>`) and `UnifiedPoolUpdater::new` takes an additional
  `refetch_algebra_fee: bool` argument at the end. Direct callers of `new` must update.
  `start_collector` callers are unaffected.

## [1.2.4]

### Improved

- **Rate-limit delay between retry attempts** — added `wait_time_between_chunks` delay between
  retrying different failed pools in both `fetch_pools_into_registry` and `fetch_pools_in_memory`,
  preventing RPC rate limits during retry sequences.

## [1.2.3]

### Improved

- **Info logging for sequential fetch delays** — added log messages showing the wait duration
  and progress (e.g. `Sequential mode: waiting 200ms before next pool (3/10)`) in both
  `fetch_pools_into_registry` and `fetch_pools_in_memory`.

## [1.2.2]

### Fixed

- **Sequential fetch mode ignoring `wait_time_between_chunks`** — in sequential mode
  (`parallel_fetch: false`), pools within a chunk were fetched back-to-back with no delay,
  causing RPC rate limits on slower endpoints. Now `wait_time_between_chunks` is applied
  between each individual pool fetch in sequential mode (in both `fetch_pools_into_registry`
  and `fetch_pools_in_memory`).

## [1.2.1]

### Changed

- **`PoolType` now implements `Display`** — replaced manual `impl ToString` with idiomatic
  `impl Display`, enabling `format!("{}", pool_type)` and consistent string formatting.
  `From<PoolType> for String` now delegates to `Display`.
- **Removed `lb_bin_depth` from `PoolFetchConfig`** — the LB fetcher now always discovers all
  bins (tree bitmap + `getNextNonEmptyBin` walk fallback), making the depth option unnecessary.

### Improved

- **Faster pool type identification** — `identify_pool_type` now accepts a `multicall_address`
  parameter and uses the configured address instead of the default. New `identify_pool_types`
  batch function identifies all pools in a chunk concurrently.
- **LB bin discovery for v2.1 pools** — tree bitmap now tries both v2.2 (slot 7) and v2.1
  (slot 8) storage layouts, with `getNextNonEmptyBin` sequential walk as final fallback.

## [1.2.0]

### Fixed

- **V3/V2 fetcher panic on failed multicall results** — replaced all `.unwrap()` calls on
  multicall results in `fetch_v3_pool` (28 sites) and `fetch_v2_pool` (2 sites) with proper
  error propagation (`map_err` + `?`). Previously, if any individual RPC call in the multicall
  batch failed (e.g. `slot0()` returning empty data on a newly created or non-standard pool),
  the fetcher panicked and killed the tokio worker thread, bypassing the existing retry logic
  in `fetch_pools_in_memory` and `fetch_pools_into_registry`.
- **`fetch_pools_into_registry` panic on max retries** — replaced `panic!()` with
  `return Err(...)` so callers can handle the failure gracefully instead of crashing.
- **Stale `last_processed_block` on low-activity networks** — on quiet networks (e.g. Katana),
  `last_processed_block` never advanced past bootstrap because: (a) WebSocket `next_batch()`
  returned `processed_through_block: None`, (b) `bootstrap()` never set the cursor after
  catch-up. Now both advance the cursor properly. Additionally, `add_pools` accepts a
  `block_number` parameter so callers can specify the block where pools were detected —
  if ahead of the stale cursor, existing pools are caught up before fetching new ones.
- **Collector left dead after failed `add_pools` in WebSocket mode** — if `fetch_pools_in_memory`
  failed, the updater (already stopped) was never restarted, leaving the collector in a zombie
  state. Now the updater is restarted before returning the error.

### Changed

- **Breaking: `add_pools` signature** — added `block_number: u64` parameter (the block at which
  new pools were detected). Callers must update their `add_pools` call sites.

### Added

- Integration tests (`tests/v3_fetch_error.rs`) reproducing the exact bug scenario on Katana
  network (chain 747474): `fetch_v3_pool` returning `Err` instead of panicking, and the
  collector surviving a failed `add_pools` call in WebSocket mode.
- `catchup_registry_to_block` helper for advancing existing registry pools to a target block.

## [1.1.0]

### Added

- **TraderJoe Liquidity Book (LB) pool support** — full implementation of LB pool swap math,
  bin-based concentrated liquidity, fee calculations, and event handling.
- **Versioned LB storage layout** — automatic detection of v2.1 (storage slot 8) and v2.2
  (storage slot 7) tree bitmap layouts for non-empty bin discovery, with sequential
  `getNextNonEmptyBin()` fallback.
- **LB fuzz tests** — property-based tests for LB pool math and bin operations.
- **Multicall-based pool type identification** — `identify_pool_type` uses a single multicall
  with `getBinStep()` (LB) and `liquidity()` (V3) to classify pools as
  `TraderJoeLB`, `UniswapV3`, or `UniswapV2`.
- `PoolType::TraderJoeLB` variant with associated event topics and profitable topics.
- `lb_bin_depth` field on `PoolFetchConfig` for configurable fallback bin discovery depth.

## [1.0.0]

### Added

- Core pool types: `UniswapV2Pool`, `UniswapV3Pool`, `ERC4626Pool`.
- `PoolInterface` trait with swap math, event application, and topic selectors.
- `PoolRegistry` — `DashMap`-backed thread-safe pool storage (feature `registry`).
- Collector system — `start_collector`, `CollectorHandle` with `add_pools`/`remove_pools`,
  `UnifiedPoolUpdater`, `WebsocketListener`, `EventProcessor` (feature `collector`).
- `fetch_pools_into_registry` with parallel chunked fetching and exponential backoff retry.
- `TokenInfo` trait for caller-provided token resolution with caching.
- V3 pool type variants: `UniswapV3`, `PancakeV3`, `AlgebraV3`, `RamsesV2`,
  `AlgebraTwoSideFee`, `AlgebraPoolFeeInState`, plus CLP pool support.
- V2 factory-to-fee mappings for 80+ networks.
- Full Rust ports of Uniswap V3 Solidity math libraries.
