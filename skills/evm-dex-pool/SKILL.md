---
name: evm-dex-pool
description: Guide for using the evm-dex-pool Rust library — EVM DEX pool types (UniswapV2, UniswapV3, ERC4626), PoolRegistry, and the collector system (start_collector, CollectorHandle, add_pools, fetch_pools_into_registry). Use this skill whenever the user asks how to use evm-dex-pool, how to start a collector, how to add pools dynamically, how to configure CollectorConfig or PoolFetchConfig, how to implement TokenInfo, or when they encounter bugs or unexpected behavior in evm-dex-pool (collector stopping, channel errors, stale block numbers, WebSocket failures, etc.).
---

# evm-dex-pool Usage Guide

## Overview

`evm-dex-pool` is a Rust library for EVM DEX pool math, state tracking, and live event collection.

**Repository location:** `../evm-dex-pool` (sibling to the bot crate)
**Consumer dependency:** `evm-dex-pool = { features = ["rpc", "registry", "collector"] }`

---

## Feature Flags — Read This First

Feature flags gate large portions of the codebase. Without the right features, code won't compile.

| Feature     | What it enables                                                                                                                    |
| ----------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| _(none)_    | Pool types, math (`UniswapV2Pool`, `UniswapV3Pool`, `ERC4626Pool`), traits                                                         |
| `rpc`       | `fetch_v2_pool`, `fetch_v3_pool`, alloy providers, `TokenInfo` trait, multicall                                                    |
| `registry`  | `PoolRegistry` (DashMap-backed thread-safe store)                                                                                  |
| `collector` | Full collector system: `start_collector`, `CollectorHandle`, `fetch_pools_into_registry`, WS listener (implies `rpc` + `registry`) |

**Build commands:**

```bash
cargo build                                    # pool types only
cargo build --features rpc,registry,collector  # everything
cargo test --features collector                # run all tests
```

---

## Core Traits

Every pool implements `PoolInterface` (from `src/pool/base.rs`):

```rust
pool.calculate_output(&token_in, amount_in) -> Result<U256>
pool.calculate_input(&token_out, amount_out) -> Result<U256>
pool.apply_swap(&token_in, amount_in) -> Result<U256>   // mutates pool state
pool.apply_log(&log) -> Result<()>                       // update from on-chain event
pool.tokens() -> (Address, Address)                      // (token0, token1)
pool.address() -> Address
pool.pool_type() -> PoolType
```

---

## TokenInfo Trait

Required for pool fetching. Implement it to resolve token decimals:

```rust
use evm_dex_pool::TokenInfo;
use alloy::providers::Provider;
use alloy::primitives::Address;

struct MyTokenCache { /* ... */ }

impl TokenInfo for MyTokenCache {
    fn get_or_fetch_token<P: Provider + Send + Sync>(
        &self,
        provider: &Arc<P>,
        address: Address,
        multicall_address: Address,
    ) -> impl Future<Output = anyhow::Result<(Address, u8)>> + Send {
        async move {
            // return (token_address, decimals)
            Ok((address, 18u8))
        }
    }
}
```

---

## PoolRegistry

Thread-safe pool store. Shared via `Arc<PoolRegistry>`.

```rust
use evm_dex_pool::PoolRegistry;

let registry = Arc::new(PoolRegistry::new(chain_id));

// Read a pool (returns Arc<RwLock<Box<dyn PoolInterface>>>)
if let Some(pool_arc) = registry.get_pool(&address) {
    let pool = pool_arc.read().await;
    let (token0, _) = pool.tokens();
    let out = pool.calculate_output(&token0, amount)?;
}

// State management
let block = registry.get_last_processed_block();
registry.set_last_processed_block(block);
registry.pool_count();
registry.get_all_addresses();

// clone() shares the underlying DashMap — NOT a deep copy
let shared = Arc::clone(&registry);
```

---

## Fetching Pools (Bootstrap)

```rust
use evm_dex_pool::collector::{fetch_pools_into_registry, PoolFetchConfig};
use std::collections::HashMap;

let fetch_config = PoolFetchConfig {
    chain_id: 1,
    multicall_address: None,          // None = use chain default
    factory_to_fee: HashMap::new(),   // V2 factory -> fee (1_000_000 basis)
    aero_factory_addresses: vec![],   // Aerodrome stable/volatile detection
    chunk_size: 10,                   // pools per parallel batch
    wait_time_between_chunks: 200,    // ms between batches (rate limiting)
    max_retries: 5,                   // exponential backoff retries
    parallel_fetch: true,             // false for rate-limited RPCs
};

let start_block = provider.get_block_number().await?;
fetch_pools_into_registry(
    &provider,
    &pool_addresses,
    BlockNumberOrTag::Number(start_block),
    &token_info,
    &registry,
    &fetch_config,
).await?;
```

---

## Starting the Collector

```rust
use evm_dex_pool::collector::{start_collector, CollectorConfig};

let mut handle = start_collector(
    Arc::clone(&provider),
    CollectorConfig {
        start_block,
        max_blocks_per_batch: 5,
        use_pending_blocks: false,
        use_websocket: true,                     // false = LatestBlock polling
        websocket_urls: vec!["wss://...".into()],
        wait_time: 2_000,                        // ms poll interval (LatestBlock mode)
    },
    Arc::clone(&registry),
    None,    // Option<Arc<dyn CollectorMetrics>>
    None,    // Option<mpsc::Sender<PendingEvent>>
).await?;
```

### CRITICAL: Store the handle

**The `CollectorHandle` MUST be kept alive.** When it drops, `cancel_tx` drops, the oneshot channel closes, and the collector stops immediately:

```rust
// WRONG — collector stops instantly!
let _ = start_collector(...).await?;

// CORRECT — keep handle in scope / struct field
let mut handle = start_collector(...).await?;
// ... do work ...
handle.stop().await;
```

**Diagnostic:** `[WARN] Collector stopping: CollectorHandle was dropped` = handle was dropped prematurely.

---

## CollectorHandle API

These are two independent operations — do NOT call `stop()` before `add_pools()`:

```rust
// permanently stop the collector when you're done
handle.stop().await;

// add new pools while the collector keeps running
// add_pools internally stops → updates → restarts the collector
handle.add_pools(new_addresses, &fetch_config, &token_info).await?;
```

`add_pools` handles everything internally — no need to stop first:

- **HTTP/LatestBlock mode:** fetches new pools in memory (collector still running), then stops, catchup to final block, restarts
- **WS mode:** stops old WS listeners, stops updater, starts new WS listeners (to buffer events during fetch), fetches, registers, restarts from stable block

---

## Three Collector Modes

| Config                     | Mode         | Behavior                                   |
| -------------------------- | ------------ | ------------------------------------------ |
| `use_websocket: true`      | Websocket    | Subscribe via WS, RPC catchup on bootstrap |
| `use_pending_blocks: true` | PendingBlock | Poll pending block for speculation         |
| both false                 | LatestBlock  | Poll latest block every `wait_time` ms     |

---

## WebSocket Setup

- The `provider` passed to `start_collector` should be an **HTTP** provider (for RPC calls)
- `websocket_urls` are purely for the WS event subscription (`wss://...`)
- Multiple URLs = redundant listeners (all buffer into the same EventQueue)
- WS listener reconnects automatically on drop (2s delay)
- Requires `alloy/transport-ws` + `alloy/pubsub` features (included in `collector` feature)

```rust
// HTTP provider for RPC; WS URLs in CollectorConfig for subscriptions
let provider = Arc::new(ProviderBuilder::new().connect_http(http_url.parse()?));
let handle = start_collector(
    provider,
    CollectorConfig {
        use_websocket: true,
        websocket_urls: vec!["wss://mainnet.infura.io/ws/v3/YOUR_KEY".into()],
        ..
    },
    ..
).await?;
```

---

## Common Pitfalls

### `address!()` macro — no `0x` prefix

```rust
use alloy::primitives::address;

// WRONG — panics at compile time
let addr = address!("0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640");

// CORRECT
let addr = address!("88e6a0c2ddd26feeb64f039a2c41296fcb3f5640");
```

### Shutdown ordering in WS mode

Stop WS listeners **before** stopping the updater. The updater dropping its `WebsocketBlockSource` closes the EventQueue channel — listeners still running will spam "channel closed" errors.

Correct order (already implemented in `CollectorHandle::stop` and `add_pools_ws`):

1. Stop WS listeners (`listener.stop().await`)
2. Stop updater (`stop_updater().await`)

### V2 factory fees

For non-standard V2 forks (SushiSwap, PancakeSwap, etc.), populate `factory_to_fee`:

```rust
factory_to_fee: HashMap::from([
    ("0x5c69bee701ef814a2b6a3edd4b1652cb9cc5aa6f".to_string(), 997_000u64), // Uniswap V2 0.3%
    ("0xc0aee478e3658e2610c5f7a4a2e1777ce9e4f2ac".to_string(), 997_000u64), // SushiSwap
])
```

`chain_id` is used to auto-detect known factories via `src/v2/factories.rs` (80+ networks).

### `identify_pool_type` heuristic

Calls `liquidity()` RPC — success = V3, failure = V2. Can misidentify exotic pools. For known types, call `fetch_v2_pool` / `fetch_v3_pool` directly.

### `PoolRegistry::clone()` is shallow

`registry.clone()` (not `Arc::clone`) shares the underlying DashMap — the same data, not a copy. Use `Arc::clone(&registry)` when you want to share ownership, which is almost always what you want.

---

## Key Files

| File                                    | Purpose                                                           |
| --------------------------------------- | ----------------------------------------------------------------- |
| `src/collector/bootstrap.rs`            | `start_collector` entry point                                     |
| `src/collector/handle.rs`               | `CollectorHandle`, `add_pools_http`, `add_pools_ws`               |
| `src/collector/config.rs`               | `CollectorConfig`, `PoolFetchConfig` structs                      |
| `src/collector/block_source.rs`         | `LatestBlockSource`, `PendingBlockSource`, `WebsocketBlockSource` |
| `src/collector/unified_pool_updater.rs` | Main event loop, cancel signal handling                           |
| `src/collector/websocket_listener.rs`   | WS subscription + reconnect logic                                 |
| `src/collector/pool_fetcher.rs`         | `fetch_pools_into_registry`, `identify_pool_type`, `fetch_pool`   |
| `src/registry.rs`                       | `PoolRegistry` implementation                                     |
| `src/v2/factories.rs`                   | Chain-specific V2 factory → fee mappings (80+ networks)           |
| `src/pool/base.rs`                      | `PoolInterface` trait definition                                  |
