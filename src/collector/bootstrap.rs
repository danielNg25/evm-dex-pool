use crate::PoolRegistry;
use alloy::providers::Provider;
use anyhow::Result;
use log::{error, info};
use std::sync::Arc;
use tokio::sync::mpsc;

use super::config::CollectorConfig;
use super::event_processor::PendingEvent;
use super::event_queue::EventQueue;
use super::metrics::CollectorMetrics;
use super::unified_pool_updater::{UnifiedPoolUpdater, UpdaterMode};
use super::websocket_listener::WebsocketListener;

/// Bootstrap the collector: create the appropriate updater based on config,
/// spawn WebSocket listeners if needed, and start the update loop.
///
/// This is the main entry point for any project that wants to keep pool state
/// up-to-date with on-chain events.
pub async fn start_collector<P: Provider + Send + Sync + Clone + 'static>(
    provider: Arc<P>,
    config: &CollectorConfig,
    pool_registry: Arc<PoolRegistry>,
    metrics: Option<Arc<dyn CollectorMetrics>>,
    swap_event_tx: Option<mpsc::Sender<PendingEvent>>,
) -> Result<()> {
    if config.use_websocket {
        info!("Starting collector with websocket mode");
        let event_queue = EventQueue::new(1000, 1000);

        for url in &config.websocket_urls {
            let event_sender = event_queue.get_sender().clone();
            let pool_registry_clone = pool_registry.clone();
            let url = url.clone();
            tokio::spawn(async move {
                let ws = WebsocketListener::new(
                    url,
                    pool_registry_clone.get_all_addresses(),
                    event_sender,
                    pool_registry_clone.get_topics().clone(),
                );
                if let Err(e) = ws.start().await {
                    error!("Websocket listener error: {}", e);
                }
            });
        }

        let mut updater = UnifiedPoolUpdater::new(
            provider,
            pool_registry,
            metrics,
            swap_event_tx,
            config.start_block,
            config.max_blocks_per_batch,
            UpdaterMode::Websocket { event_queue },
        );

        tokio::spawn(async move {
            if let Err(e) = updater.start().await {
                error!("Collector error: {}", e);
            }
        });
    } else if config.use_pending_blocks {
        info!("Starting collector with pending block mode");
        let mut updater = UnifiedPoolUpdater::new(
            provider,
            pool_registry,
            metrics,
            swap_event_tx,
            config.start_block,
            config.max_blocks_per_batch,
            UpdaterMode::PendingBlock,
        );

        tokio::spawn(async move {
            if let Err(e) = updater.start().await {
                error!("Collector error: {}", e);
            }
        });
    } else {
        info!("Starting collector with latest block mode");
        let mut updater = UnifiedPoolUpdater::new(
            provider,
            pool_registry,
            metrics,
            swap_event_tx,
            config.start_block,
            config.max_blocks_per_batch,
            UpdaterMode::LatestBlock {
                wait_time_ms: config.wait_time,
            },
        );

        tokio::spawn(async move {
            if let Err(e) = updater.start().await {
                error!("Collector error: {}", e);
            }
        });
    }

    Ok(())
}
