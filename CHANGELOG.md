# Changelog

All notable changes to `evm-dex-pool` will be documented in this file.

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
