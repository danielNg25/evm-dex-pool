# evm-dex-pool

Reusable Rust library for EVM DEX pool state management — fetch pools from chain, keep them in sync with events, and estimate swap outputs.

## Installation

```toml
[dependencies]
evm-dex-pool = { path = "../evm-dex-pool", features = ["rpc", "registry", "collector"] }
```

| Feature | What it enables |
|---------|----------------|
| `rpc` | Fetch pool state from RPC (`fetch_v2_pool`, `fetch_v3_pool`) |
| `registry` | `PoolRegistry` — thread-safe concurrent pool storage |
| `collector` | Event collection system — keeps pools in sync with chain via RPC polling or WebSocket |

---

## Usage

### 1. Create a Registry and Add Pools

```rust
use evm_dex_pool::{PoolRegistry, UniswapV2Pool, UniswapV3Pool, TopicList};
use std::sync::Arc;

let registry = Arc::new(PoolRegistry::new(chain_id));

// Register event topics so the collector knows what to listen for
registry.add_topics(UniswapV2Pool::topics());
registry.add_topics(UniswapV3Pool::topics());
registry.add_profitable_topics(UniswapV2Pool::profitable_topics());
registry.add_profitable_topics(UniswapV3Pool::profitable_topics());

// Add pools (see "Fetching Pools from RPC" below)
registry.add_pool(Box::new(v2_pool));
registry.add_pool(Box::new(v3_pool));
```

### 2. Start the Collector (Bootstrap)

The collector keeps pool state in sync with the chain. Use `start_collector` for the simplest setup:

```rust
use evm_dex_pool::collector::{start_collector, CollectorConfig, PendingEvent};
use tokio::sync::mpsc;

// Optional: channel to receive swap events for downstream processing
let (swap_tx, mut swap_rx) = mpsc::channel::<PendingEvent>(1000);

let config = CollectorConfig {
    start_block: 0,             // 0 = resume from last processed block
    max_blocks_per_batch: 64,
    use_pending_blocks: false,  // true = speculative pending block mode
    use_websocket: false,       // true = WebSocket mode (needs websocket_urls)
    websocket_urls: vec![],     // WebSocket RPC endpoints
    wait_time: 500,             // ms polling interval for LatestBlock mode
};

start_collector(
    provider.clone(),
    &config,
    registry.clone(),
    None,              // metrics: Option<Arc<dyn CollectorMetrics>>
    Some(swap_tx),     // swap events: Option<mpsc::Sender<PendingEvent>>
).await?;

// Consume swap events (optional)
tokio::spawn(async move {
    while let Some(event) = swap_rx.recv().await {
        // event.event: Log — the raw swap log
        // event.modified_pools — speculative pool clones (pending mode only)
    }
});
```

**Collector modes:**

| Config | Mode | Behavior |
|--------|------|----------|
| `use_websocket: true` | WebSocket | Bootstraps via RPC, then streams live events. Fastest. |
| `use_pending_blocks: true` | PendingBlock | Polls confirmed blocks + pending block for speculative state. |
| Both false | LatestBlock | Polls RPC every `wait_time` ms. Simplest. |

### 3. Estimate Swap Outputs

Read a pool from the registry and calculate swap amounts:

```rust
if let Some(pool_lock) = registry.get_pool(&pool_address) {
    let pool = pool_lock.read().await;

    // How much token1 do I get for 1 ETH in?
    let output = pool.calculate_output(&token_in, amount_in)?;

    // How much token0 do I need to get exactly 100 USDC out?
    let input = pool.calculate_input(&token_out, amount_out)?;
}
```

### 4. Batch Fetch Pools into Registry

Use `fetch_pools_into_registry` to auto-detect pool types, fetch state from chain, and add them to the registry in parallel with retry:

```rust
use evm_dex_pool::collector::{fetch_pools_into_registry, PoolFetchConfig};

let config = PoolFetchConfig {
    multicall_address,
    chain_id,
    factory_to_fee: factory_to_fee.clone(), // HashMap<String, u64> — factory address -> fee
    aero_factory_addresses: vec![],         // Aerodrome-style factory addresses
    chunk_size: 10,                         // pools fetched in parallel per chunk
    wait_time_between_chunks: 500,          // ms between chunks (rate limiting)
    max_retries: 5,                         // retries per pool with exponential backoff
};

let pool_addresses: Vec<Address> = vec![/* ... */];

// Returns addresses of newly fetched pools (skips pools already in registry)
let fetched = fetch_pools_into_registry(
    &provider,
    &pool_addresses,
    block_number,          // BlockNumberOrTag
    &token_info,           // impl TokenInfo
    &registry,
    &config,
).await?;

// Do app-specific work with newly fetched pools
for addr in fetched {
    if let Some(pool) = registry.get_pool(&addr) {
        let guard = pool.read().await;
        println!("Fetched {} ({})", guard.address(), guard.pool_type());
    }
}
```

Pool type detection is automatic — it calls `liquidity()` (V3-specific) to distinguish V2 from V3. You can also call the lower-level functions directly:

```rust
use evm_dex_pool::collector::{identify_pool_type, fetch_pool};

let pool_type = identify_pool_type(&provider, pool_address).await?;
let pool = fetch_pool(&provider, pool_address, block_id, pool_type, &token_info, &config).await?;
registry.add_pool(pool);
```

### 5. Fetching Individual Pools from RPC

```rust
use evm_dex_pool::v2::fetcher::fetch_v2_pool;
use evm_dex_pool::v3::fetcher::{fetch_v3_pool, fetch_v3_ticks};

// Fetch a V2 pool
let v2_pool = fetch_v2_pool(
    &provider, pool_address, BlockId::latest(),
    &token_info,         // impl TokenInfo (resolves token address/decimals)
    multicall_address, chain_id,
    &factory_to_fee,     // HashMap<String, u64> — factory address -> fee
    &aero_factories,     // &[Address] — Aerodrome-style factories
).await?;

// Fetch a V3 pool + tick data
let mut v3_pool = fetch_v3_pool(
    &provider, pool_address, BlockId::latest(),
    &token_info, multicall_address, chain_id,
).await?;
fetch_v3_ticks(&provider, &mut v3_pool, BlockId::latest(), multicall_address).await?;
```

---

## Optional: Custom Metrics

Implement `CollectorMetrics` to plug in your own metrics system:

```rust
use evm_dex_pool::collector::CollectorMetrics;

impl CollectorMetrics for MyMetrics {
    fn add_opportunity(&self, tx_hash: TxHash, log_index: u64, received_at: u64) { /* ... */ }
    fn set_processed_at(&self, tx_hash: TxHash, log_index: u64, processed_at: u64) { /* ... */ }
}

// Pass to start_collector
start_collector(provider, &config, registry, Some(Arc::new(my_metrics)), swap_tx).await?;
```

Pass `None` to disable metrics.

---

## Pool Types Supported

- **UniswapV2Pool** — constant product (`x * y = k`) and stable swap curves
- **UniswapV3Pool** — concentrated liquidity with tick-based pricing (supports UniswapV3, PancakeV3, AlgebraV3, RamsesV2 variants)
- **ERC4626** — vault-based pools (deposit/withdraw pricing)

All pool types implement `PoolInterface` which provides `calculate_output`, `calculate_input`, `apply_swap`, `apply_log`, `tokens`, `fee`, `address`, etc.
