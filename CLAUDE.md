# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

`evm-dex-pool` is a shared Rust library providing EVM DEX pool implementations (UniswapV2, UniswapV3, ERC4626) with traits, math, and an optional event collection system. It is consumed by `evm-dex-arbitrage` with `features = ["rpc", "registry", "collector"]`.

## Build & Test Commands

```bash
cargo build                                    # Default features only (pool types + math, no RPC)
cargo build --features rpc,registry,collector  # Full build (as used by evm-dex-arbitrage)
cargo test                                     # Default feature tests
cargo test --features collector                # All tests including collector module
cargo test test_name                           # Run a specific test by name
cargo fmt && cargo clippy                      # Format and lint
```

## Feature Flags

Feature selection is critical — large portions of the codebase are gated:

| Feature | Enables |
|---------|---------|
| _(none)_ | Pool types (`UniswapV2Pool`, `UniswapV3Pool`, ERC4626), traits, math |
| `rpc` | `fetch_v2_pool`, `fetch_v3_pool`, alloy provider/contract bindings, `TokenInfo` trait, `create_fallback_provider` |
| `registry` | `PoolRegistry` (DashMap-backed, thread-safe pool storage) |
| `collector` | Full event collection system: `start_collector`, `UnifiedPoolUpdater`, `WebsocketListener`, `EventProcessor`, `fetch_pools_into_registry` (implies `rpc` + `registry`) |

## Architecture

### Core Traits (`src/pool/base.rs`)

All pool types implement `PoolInterface`, which requires:
- `calculate_output` / `calculate_input` — swap math
- `apply_swap` — mutate pool state after a swap
- `apply_log` (via `EventApplicable`) — update state from on-chain log
- `topics` / `profitable_topics` (via `TopicList`) — event topic selectors

`PoolType` enum (`UniswapV2`, `UniswapV3`, `ERC4626(ERC4626Pool)`) maps to the three pool implementations. Downcasting from `Box<dyn PoolInterface>` uses `as_any` / `downcast_ref`.

### Pool Implementations

- **`src/v2/`** — `UniswapV2Pool` with `V2PoolType` (UniswapV2 or Stable). Fee stored in 1_000_000 basis. `src/v2/factories.rs` has chain-specific factory address → fee mappings for 80+ networks.
- **`src/v3/`** — `UniswapV3Pool` with full tick-based concentrated liquidity math (bit_math, full_math, sqrt_price_math, swap_math, tick_math — Rust ports of Uniswap V3 Solidity contracts).
- **`src/erc4626/`** — `ERC4626Standard` and `VerioIP` variants. `ERC4626Pool` enum selects the variant.

### Registry (`src/registry.rs`, feature `registry`)

`PoolRegistry` is the central pool store:
- `DashMap<Address, Arc<tokio::sync::RwLock<Box<dyn PoolInterface>>>>` — shard-locked, lock-free reads
- `AtomicU64` for `last_processed_block`
- Plain `RwLock<Vec<Topic>>` for event topics (written once at startup, then read-only)
- `clone()` shares the underlying `DashMap` (same data, not a deep copy)

### Collector System (`src/collector/`, feature `collector`)

Entry point: `start_collector(provider, config, pool_registry, metrics, swap_event_tx)` — spawns the appropriate updater based on `CollectorConfig`.

**Three updater modes** (all implemented via `UnifiedPoolUpdater` + a `BlockSource` trait):
- `PendingBlock` — polls pending block logs via RPC
- `LatestBlock` — polls latest block logs with configurable `wait_time_ms`
- `Websocket` — subscribes via WebSocket (`WebsocketListener`), drains `EventQueue`

**Data flow:**
```
BlockSource (RPC/WS logs)
  → EventQueue (bounded mpsc, WS mode only)
  → UnifiedPoolUpdater
  → EventProcessor (apply_log to pools, filter profitable topics)
  → swap_event_tx (optional mpsc::Sender<PendingEvent> for caller)
```

**Pool fetching:** `fetch_pools_into_registry` fetches a list of addresses in parallel chunks with exponential backoff retry. It calls `identify_pool_type` (tries `liquidity()` RPC call — success = V3, failure = V2) then the appropriate typed fetcher. Panics after `max_retries` exhausted.

### RPC Contracts (`src/contracts.rs`, `src/contracts_rpc.rs`)

ABI bindings are generated via `alloy::sol!` macro from JSON files in `contracts/ABI/`. `contracts.rs` has bindings used in pure pool math (sync/event parsing). `contracts_rpc.rs` has bindings requiring the `rpc` feature (provider-based calls).

### Key Config Structs

- `CollectorConfig` — `start_block`, `max_blocks_per_batch`, `use_pending_blocks`, `use_websocket`, `websocket_urls`, `wait_time` (ms, polling mode)
- `PoolFetchConfig` — `chain_id`, `factory_to_fee` (HashMap for V2 fees), `aero_factory_addresses`, `chunk_size`, `wait_time_between_chunks`, `max_retries`, `parallel_fetch`, `multicall_address`
