# evm-dex-pool

Reusable Rust library for EVM DEX pool implementations (Uniswap V2, Uniswap V3, ERC4626) with traits, math, on-chain fetching, concurrent registry, and blockchain event collection.

## Features

| Feature | Dependencies | Description |
|---------|-------------|-------------|
| *(core)* | alloy, anyhow, serde, chrono, log | Pool types, traits, swap math. Always available. |
| `rpc` | alloy/providers, alloy/contract, tokio | Fetch pool state from RPC. `fetch_v2_pool`, `fetch_v3_pool`, `TokenInfo` trait. |
| `registry` | dashmap, tokio | `PoolRegistry` — thread-safe concurrent pool storage with DashMap. |
| `collector` | registry + rpc + async-trait, futures-util, alloy/transport-ws, alloy/pubsub | Event collection system: block sources, pool state updates, WebSocket listener. |

```toml
# Cargo.toml
[dependencies]
evm-dex-pool = { path = "../evm-dex-pool", features = ["rpc", "registry", "collector"] }
```

---

## Architecture Overview

```
                       evm-dex-pool
 ┌────────────────────────────────────────────────────────────┐
 │  Core (always available)                                   │
 │  ├── PoolInterface trait    (calculate_output/input, etc.) │
 │  ├── UniswapV2Pool          (constant product + stable)    │
 │  ├── UniswapV3Pool          (concentrated liquidity)       │
 │  ├── ERC4626Standard/VerioIP (vault pools)                 │
 │  ├── EventApplicable trait  (apply_log from on-chain)      │
 │  └── TopicList trait        (event signature topics)       │
 │                                                            │
 │  Registry (feature = "registry")                           │
 │  └── PoolRegistry           (DashMap-backed, Arc<RwLock>)  │
 │                                                            │
 │  RPC (feature = "rpc")                                     │
 │  ├── fetch_v2_pool()        (multicall-based fetcher)      │
 │  ├── fetch_v3_pool()        (multicall-based fetcher)      │
 │  └── TokenInfo trait        (resolve token address/decimals│
 │                                                            │
 │  Collector (feature = "collector")                         │
 │  ├── UnifiedPoolUpdater     (main orchestrator)            │
 │  ├── BlockSource trait      (Pending/Latest/Websocket)     │
 │  ├── EventProcessor         (apply logs, send swap events) │
 │  ├── EventQueue/EventSender (dedup + buffering)            │
 │  ├── WebsocketListener      (auto-reconnect, heartbeat)    │
 │  └── CollectorMetrics trait (pluggable metrics)            │
 └────────────────────────────────────────────────────────────┘
```

---

## Core Traits

### PoolInterface

The main abstraction all pool types implement. Defined in `src/pool/base.rs`.

```rust
pub trait PoolInterface: Debug + Send + Sync + PoolTypeTrait + EventApplicable {
    fn calculate_output(&self, token_in: &Address, amount_in: U256) -> Result<U256>;
    fn calculate_input(&self, token_out: &Address, amount_out: U256) -> Result<U256>;
    fn apply_swap(&mut self, token_in: &Address, amount_in: U256, amount_out: U256) -> Result<()>;
    fn address(&self) -> Address;
    fn tokens(&self) -> (Address, Address);
    fn token0(&self) -> Address;
    fn token1(&self) -> Address;
    fn fee(&self) -> f64;          // e.g. 0.003 for 0.3%
    fn fee_raw(&self) -> u64;
    fn id(&self) -> String;
    fn contains_token(&self, token: &Address) -> bool;
    fn clone_box(&self) -> Box<dyn PoolInterface + Send + Sync>;
    fn log_summary(&self) -> String;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
```

**Downcasting** to concrete types:
```rust
let pool: &dyn PoolInterface = ...;
if let Some(v2) = pool.downcast_ref::<UniswapV2Pool>() { /* use v2 fields */ }
if let Some(v3) = pool.downcast_ref::<UniswapV3Pool>() { /* use v3 fields */ }
```

### EventApplicable

Update pool state from an on-chain log event (Sync, Swap, Mint, Burn, etc.):
```rust
pub trait EventApplicable {
    fn apply_log(&mut self, event: &Log) -> Result<()>;
}
```

### TopicList

Get event signature hashes for filtering:
```rust
pub trait TopicList {
    fn topics() -> Vec<Topic>;              // All events (Sync, Swap, Mint, Burn, etc.)
    fn profitable_topics() -> Vec<Topic>;   // Swap events only
}
```

### PoolType

```rust
pub enum PoolType {
    UniswapV2,
    UniswapV3,
    ERC4626(ERC4626Pool),
}
```

### Type Alias

```rust
pub type Topic = FixedBytes<32>;  // Event signature hash
```

---

## Pool Types

### UniswapV2Pool

Supports constant-product AMM (`x * y = k`) and stable swap curves.

```rust
pub struct UniswapV2Pool {
    pub pool_type: V2PoolType,  // UniswapV2 or Stable
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub decimals0: u128,        // Stored as 10^decimals (e.g. 10^18)
    pub decimals1: u128,
    pub reserve0: U256,
    pub reserve1: U256,
    pub fee: U256,              // Basis points * 100 (e.g. 3000 = 0.3%)
    pub last_updated: u64,
    pub created_at: u64,
}

pub enum V2PoolType { UniswapV2, Stable }
```

**Constructor:**
```rust
UniswapV2Pool::new(
    pool_type: V2PoolType,
    address: Address,
    token0: Address,  token1: Address,
    decimals_0: u8,   decimals_1: u8,
    reserve0: U256,   reserve1: U256,
    fee: U256,
) -> Self
```

**Key methods:** `update_reserves()`, `constant_product()`, `is_valid()`

**Events handled by `apply_log`:** V2 `Sync(reserve0, reserve1)`, V2 `Swap(...)`.

### UniswapV3Pool

Concentrated liquidity with tick-based pricing.

```rust
pub struct UniswapV3Pool {
    pub pool_type: V3PoolType,
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee: U24,               // Fee in ppm (e.g. 3000 = 0.3%)
    pub tick_spacing: i32,
    pub sqrt_price_x96: U160,   // sqrt(price) * 2^96
    pub tick: i32,
    pub liquidity: u128,
    pub ticks: TickMap,          // BTreeMap<i32, Tick>
    pub ratio_conversion_factor: U256,
    pub factory: Address,
    pub last_updated: u64,
    pub created_at: u64,
}

pub enum V3PoolType {
    UniswapV3, PancakeV3, AlgebraV3,
    RamsesV2, AlgebraTwoSideFee, AlgebraPoolFeeInState,
}

pub type TickMap = BTreeMap<i32, Tick>;

pub struct Tick {
    pub index: i32,
    pub liquidity_net: i128,
    pub liquidity_gross: u128,
}
```

**Constructor:**
```rust
UniswapV3Pool::new(
    address: Address,
    token0: Address,  token1: Address,
    fee: U24,         tick_spacing: i32,
    sqrt_price_x96: U160, tick: i32,
    liquidity: u128,  factory: Address,
    pool_type: V3PoolType,
) -> Self
```

**Key methods:** `update_state()`, `update_tick()`, `get_price_from_sqrt_price()`, `has_sufficient_liquidity()`, `get_adjacent_ticks()`

**Events handled by `apply_log`:** V3 `Swap(...)`, `Mint(...)`, `Burn(...)`. Updates sqrt_price, tick, liquidity, and tick map.

### ERC4626Standard

Vault-based pool for deposit/withdraw pricing.

```rust
pub struct ERC4626Standard {
    pub address: Address,
    pub vault_token: Address,   // Shares token
    pub asset_token: Address,   // Underlying asset
    pub vault_reserve: U256,    // Total shares
    pub asset_reserve: U256,    // Total assets
    pub deposit_fee: u32,       // Basis points (10000 = 100%)
    pub withdraw_fee: u32,
}
```

---

## Pool Registry

Requires feature `"registry"`. Thread-safe concurrent pool storage.

```rust
use evm_dex_pool::PoolRegistry;
use std::sync::Arc;

let registry = Arc::new(PoolRegistry::new(1)); // network_id = 1 (Ethereum)
```

### Pool Management

```rust
registry.add_pool(Box::new(pool));
registry.pool_count() -> usize;
registry.get_pool(&address) -> Option<Arc<RwLock<Box<dyn PoolInterface>>>>;
registry.remove_pool(&address);
registry.get_all_pools() -> Vec<Arc<RwLock<Box<dyn PoolInterface>>>>;
registry.get_v2_pools() -> Vec<Arc<RwLock<Box<dyn PoolInterface>>>>;
registry.get_v3_pools() -> Vec<Arc<RwLock<Box<dyn PoolInterface>>>>;
registry.get_all_addresses() -> Vec<Address>;
registry.get_v2_addresses() -> Vec<Address>;
registry.get_v3_addresses() -> Vec<Address>;
```

### Block & Topic Tracking

```rust
registry.get_last_processed_block() -> u64;
registry.set_last_processed_block(block_number: u64);
registry.add_topics(topics: Vec<Topic>);
registry.add_profitable_topics(topics: Vec<Topic>);
registry.get_topics() -> Vec<Topic>;
registry.get_profitable_topics() -> HashSet<Topic>;
```

### Reading a Pool

```rust
if let Some(pool_lock) = registry.get_pool(&address) {
    let guard = pool_lock.read().await;
    let output = guard.calculate_output(&token_in, amount_in)?;
}
```

### Writing to a Pool

```rust
if let Some(pool_lock) = registry.get_pool(&address) {
    let mut guard = pool_lock.write().await;
    guard.apply_log(&event)?;
}
```

---

## RPC Fetching

Requires feature `"rpc"`.

### TokenInfo Trait

Implement this to provide token address/decimals resolution with caching:

```rust
pub trait TokenInfo: Send + Sync {
    fn get_or_fetch_token<P: Provider + Send + Sync>(
        &self,
        provider: &Arc<P>,
        address: Address,
        multicall_address: Address,
    ) -> impl Future<Output = Result<(Address, u8)>> + Send;
}
```

### Fetch V2 Pool

```rust
use evm_dex_pool::v2::fetcher::fetch_v2_pool;

let pool = fetch_v2_pool(
    &provider,
    pool_address,
    BlockId::latest(),
    &token_info,           // impl TokenInfo
    multicall_address,
    chain_id,
    &factory_to_fee,       // HashMap<String, u64> — factory address -> fee
    &aero_factories,       // &[Address] — Aerodrome-style factories
).await?;
```

### Fetch V3 Pool

```rust
use evm_dex_pool::v3::fetcher::{fetch_v3_pool, fetch_v3_ticks};

let mut pool = fetch_v3_pool(
    &provider,
    pool_address,
    BlockId::latest(),
    &token_info,
    multicall_address,
).await?;

// Fetch tick data (required for accurate V3 swap simulation)
fetch_v3_ticks(&provider, &mut pool, BlockId::latest(), multicall_address).await?;
```

---

## Collector (Event Collection System)

Requires feature `"collector"`. Listens to blockchain events, updates pool state in the registry, and optionally forwards swap events to a downstream consumer.

### Data Flow

```
Blockchain (RPC / WebSocket)
    |
    v
[BlockSource] ──next_batch()──> [EventBatch]
    |                                |
    |                    events + ProcessingMode
    |                                |
    v                                v
[UnifiedPoolUpdater.start()]  ──> [EventProcessor]
    |                                |
    |                   ┌────────────┼──────────────┐
    |                   v            v              v
    |              ApplyOnly   ConfirmedWithSwaps  Pending
    |              (catch-up)  (live blocks)       (speculative)
    |                   |            |              |
    |                   v            v              v
    |              update        update +        clone pools +
    |              registry      send swap       send swap
    |                            events          events
    v
[PoolRegistry] (updated pool state)
    +
[swap_event_tx] ──> consumer receives PendingEvent
```

### Quick Start

```rust
use evm_dex_pool::collector::{UnifiedPoolUpdater, UpdaterMode};
use evm_dex_pool::PoolRegistry;
use std::sync::Arc;

// 1. Create registry and add pools + topics
let registry = Arc::new(PoolRegistry::new(chain_id));
// ... add pools, topics, profitable_topics ...

// 2. Create updater (no metrics, no swap events — just update pool state)
let mut updater = UnifiedPoolUpdater::new(
    provider.clone(),
    registry.clone(),
    None,   // metrics: Option<Arc<dyn CollectorMetrics>>
    None,   // swap_event_tx: Option<mpsc::Sender<PendingEvent>>
    start_block,
    64,     // max_blocks_per_batch
    UpdaterMode::LatestBlock { wait_time_ms: 1000 },
);

// 3. Run forever (blocks current task)
tokio::spawn(async move {
    updater.start().await.unwrap();
});
```

### With Swap Events and Metrics

```rust
use evm_dex_pool::collector::{
    UnifiedPoolUpdater, UpdaterMode, PendingEvent, CollectorMetrics,
};
use tokio::sync::mpsc;

// Implement CollectorMetrics for your metrics system
impl CollectorMetrics for MyMetrics {
    fn add_opportunity(&self, tx_hash: TxHash, log_index: u64, received_at: u64) {
        // record opportunity
    }
    fn set_processed_at(&self, tx_hash: TxHash, log_index: u64, processed_at: u64) {
        // record processing time
    }
}

// Create channel for swap events
let (swap_tx, mut swap_rx) = mpsc::channel::<PendingEvent>(1000);
let metrics = Arc::new(MyMetrics::new());

let mut updater = UnifiedPoolUpdater::new(
    provider.clone(),
    registry.clone(),
    Some(metrics),      // pluggable metrics
    Some(swap_tx),      // send swap events
    start_block,
    64,
    UpdaterMode::LatestBlock { wait_time_ms: 500 },
);

// Consume swap events
tokio::spawn(async move {
    while let Some(pending_event) = swap_rx.recv().await {
        let log: Log = pending_event.event;
        let modified_pools = pending_event.modified_pools; // for pending mode
        // ... process swap event (find arbitrage, etc.)
    }
});

tokio::spawn(async move {
    updater.start().await.unwrap();
});
```

### UpdaterMode

| Mode | Description |
|------|-------------|
| `PendingBlock` | Polls RPC for confirmed blocks (ApplyOnly), then fetches pending block (Pending mode with speculative clones). |
| `LatestBlock { wait_time_ms }` | Polls RPC every `wait_time_ms`. Catch-up batches use ApplyOnly; only the latest single block triggers swap events. |
| `Websocket { event_queue }` | Bootstraps via RPC catch-up, then streams live events from an EventQueue fed by WebSocket. |

### WebSocket Mode

```rust
use evm_dex_pool::collector::{
    UnifiedPoolUpdater, UpdaterMode, EventQueue, WebsocketListener,
};

// Create event queue for WebSocket events
let event_queue = EventQueue::new(1000, 1000);

// Start WebSocket listener(s)
for ws_url in &websocket_urls {
    let sender = event_queue.get_sender();
    let addresses = registry.get_all_addresses();
    let topics = registry.get_topics();
    let ws = WebsocketListener::new(
        ws_url.clone(),
        addresses,
        sender,
        topics,
    );
    tokio::spawn(async move { ws.start().await.unwrap(); });
}

// Create updater in Websocket mode
let mut updater = UnifiedPoolUpdater::new(
    provider.clone(),
    registry.clone(),
    None, None,
    start_block,
    64,
    UpdaterMode::Websocket { event_queue },
);

tokio::spawn(async move { updater.start().await.unwrap(); });
```

### CollectorMetrics Trait

Optional trait for recording collector-level metrics. Pass `None` to disable.

```rust
pub trait CollectorMetrics: Send + Sync {
    fn add_opportunity(&self, tx_hash: TxHash, log_index: u64, received_at: u64);
    fn set_processed_at(&self, tx_hash: TxHash, log_index: u64, processed_at: u64);
}
```

### PendingEvent

Sent via `swap_event_tx` channel when a profitable swap event is detected:

```rust
pub struct PendingEvent {
    pub event: Log,
    /// For pending block mode: cloned pools with speculative state.
    /// For confirmed modes: empty HashMap.
    pub modified_pools: Arc<RwLock<HashMap<Address, Box<dyn PoolInterface + Send + Sync>>>>,
}
```

### EventProcessor (Lower-Level API)

For direct control without `UnifiedPoolUpdater`:

```rust
use evm_dex_pool::collector::EventProcessor;

let processor = EventProcessor::new(
    registry.clone(),
    Some(metrics),
    Some(swap_tx),
    Arc::new(profitable_topics),
);

// Bootstrap catch-up (no swap events sent)
processor.apply_events_to_registry(&events).await;

// Live confirmed events (updates registry + sends swap events)
processor.process_confirmed_events(events).await;

// Pending block (clones pools speculatively + sends swap events)
processor.process_pending_events(events).await;
```

### BlockSource (Lower-Level API)

Implement custom block sources:

```rust
#[async_trait]
pub trait BlockSource: Send {
    async fn bootstrap(&mut self) -> Result<()> { Ok(()) }
    async fn next_batch(&mut self) -> Result<EventBatch>;
}

pub struct EventBatch {
    pub events: Vec<Log>,
    pub processing_mode: ProcessingMode,  // ApplyOnly | ConfirmedWithSwaps | Pending
    pub processed_through_block: Option<u64>,
}
```

Built-in implementations:
- `PendingBlockSource<P>` — RPC polling + pending block
- `LatestBlockSource<P>` — RPC polling, latest block only
- `WebsocketBlockSource<P>` — RPC bootstrap + WebSocket streaming

### EventQueue

Deduplicating event buffer for WebSocket mode:

```rust
let event_queue = EventQueue::new(buffer_size, max_tracked_events);
let sender: Arc<EventSender> = event_queue.get_sender(); // clone for multiple WS feeds

// Producer (WebSocket listener calls this)
sender.send(log).await?; // deduplicates by (tx_hash, log_index)

// Consumer
let events: Vec<Log> = event_queue.get_all_available_events().await;
let events: Vec<Log> = event_queue.get_events_batch(max).await;
let events: Vec<Log> = event_queue.get_events_with_batching(timeout).await;
```

### WebsocketListener

Auto-reconnecting WebSocket log subscription:

```rust
let ws = WebsocketListener::new(
    ws_url,             // String (ws:// or wss://)
    pool_addresses,     // Vec<Address>
    event_sender,       // Arc<EventSender> from EventQueue
    topics,             // Vec<Topic> — event signatures to subscribe
);

ws.start().await?;  // spawns background task
ws.stop().await?;   // graceful shutdown
```

Features: auto-reconnect on disconnect, heartbeat ping every 30s, stall detection (180s no events), max 3 ping failures before reconnect.

### Utility Functions

```rust
// Low-level event fetch
pub async fn fetch_events<P: Provider>(
    provider: &Arc<P>,
    addresses: Vec<Address>,
    topics: Vec<FixedBytes<32>>,
    from_block: BlockNumberOrTag,
    to_block: BlockNumberOrTag,
) -> Result<Vec<Log>>;

// With exponential backoff retry (50ms-500ms) and 30s timeout per attempt
pub async fn fetch_events_with_retry<P: Provider>(
    provider: &Arc<P>,
    addresses: Vec<Address>,
    topics: Vec<Topic>,
    from_block: BlockNumberOrTag,
    to_block: BlockNumberOrTag,
) -> Result<Vec<Log>>;
```

---

## Complete Example: Pool State Tracker

A minimal application that tracks pool state from the blockchain:

```rust
use evm_dex_pool::{
    PoolRegistry, PoolType, UniswapV2Pool, UniswapV3Pool, TopicList,
    collector::{UnifiedPoolUpdater, UpdaterMode},
};
use alloy::providers::ProviderBuilder;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let provider = Arc::new(
        ProviderBuilder::new().connect("https://rpc.example.com").await?
    );

    // Setup registry
    let registry = Arc::new(PoolRegistry::new(1));

    // Add event topics for V2 and V3 pools
    registry.add_topics(UniswapV2Pool::topics());
    registry.add_topics(UniswapV3Pool::topics());
    registry.add_profitable_topics(UniswapV2Pool::profitable_topics());
    registry.add_profitable_topics(UniswapV3Pool::profitable_topics());

    // Add pools (fetched or constructed)
    // registry.add_pool(Box::new(v2_pool));
    // registry.add_pool(Box::new(v3_pool));

    // Start updater — just keeps pools in sync, no swap events
    let mut updater = UnifiedPoolUpdater::new(
        provider,
        registry.clone(),
        None,   // no metrics
        None,   // no swap events
        0,      // start from last processed block
        64,
        UpdaterMode::LatestBlock { wait_time_ms: 1000 },
    );

    updater.start().await?;
    Ok(())
}
```

---

## Module Structure

```
src/
├── lib.rs                  # Public API, feature gates, re-exports
├── pool/
│   ├── mod.rs              # Re-exports
│   ├── base.rs             # PoolInterface, EventApplicable, TopicList, PoolType, Topic
│   └── mock.rs             # MockPool for testing
├── v2/
│   ├── mod.rs
│   ├── pool.rs             # UniswapV2Pool (constant product + stable swap math)
│   ├── factories.rs        # Chain-specific factory addresses and fees
│   └── fetcher.rs          # fetch_v2_pool() [rpc feature]
├── v3/
│   ├── mod.rs
│   ├── pool.rs             # UniswapV3Pool (concentrated liquidity)
│   ├── fetcher.rs          # fetch_v3_pool(), fetch_v3_ticks() [rpc feature]
│   ├── tick.rs             # Tick struct
│   ├── swap_math.rs        # V3 swap math (v3_swap function)
│   ├── sqrt_price_math.rs  # Price calculations
│   ├── tick_math.rs        # Tick-to-price conversions
│   ├── liquidity_math.rs   # Liquidity calculations
│   ├── full_math.rs        # Full-precision arithmetic
│   ├── bit_math.rs         # Bit operations
│   └── unsafe_math.rs      # Unchecked math
├── erc4626/
│   ├── mod.rs              # ERC4626Pool enum, re-exports
│   ├── standard.rs         # ERC4626Standard implementation
│   ├── verio_ip.rs         # VerioIP variant
│   └── fetcher.rs          # [rpc feature]
├── registry.rs             # PoolRegistry [registry feature]
├── contracts.rs            # ABI bindings (event-only, no RPC)
├── contracts_rpc.rs        # ABI bindings with RPC [rpc feature]
├── token_info.rs           # TokenInfo trait [rpc feature]
└── collector/              # [collector feature]
    ├── mod.rs              # Re-exports
    ├── metrics.rs          # CollectorMetrics trait
    ├── utils.rs            # fetch_events()
    ├── event_queue.rs      # EventQueue, EventSender
    ├── event_processor.rs  # EventProcessor, PendingEvent, fetch_events_with_retry()
    ├── block_source.rs     # BlockSource trait, PendingBlockSource, LatestBlockSource, WebsocketBlockSource
    ├── unified_pool_updater.rs  # UnifiedPoolUpdater, UpdaterMode
    └── websocket_listener.rs    # WebsocketListener
```
