pub mod block_source;
pub mod bootstrap;
pub mod config;
pub mod event_processor;
pub mod event_queue;
pub mod handle;
pub mod metrics;
pub mod multicall;
pub mod pool_fetcher;
pub mod unified_pool_updater;
pub mod utils;
pub mod websocket_listener;

pub use block_source::{
    BlockSource, EventBatch, LatestBlockSource, PendingBlockSource, ProcessingMode,
    WebsocketBlockSource,
};
pub use bootstrap::start_collector;
pub use config::{CollectorConfig, PoolFetchConfig};
pub use event_processor::{fetch_events_with_retry, EventProcessor, PendingEvent};
pub use event_queue::{create_event_queue, EventQueue, EventSender};
pub use handle::CollectorHandle;
pub use metrics::CollectorMetrics;
pub use multicall::resolve_multicall_address;
pub use pool_fetcher::{fetch_pool, fetch_pools_into_registry, identify_pool_type};
pub use unified_pool_updater::{UnifiedPoolUpdater, UpdaterMode};
pub use utils::*;
pub use websocket_listener::WebsocketListener;
