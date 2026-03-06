use crate::PoolRegistry;
use alloy::providers::Provider;
use anyhow::Result;
use log::{error, info};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use super::config::CollectorConfig;
use super::event_processor::PendingEvent;
use super::event_queue::EventQueue;
use super::handle::CollectorHandle;
use super::metrics::CollectorMetrics;
use super::unified_pool_updater::{UnifiedPoolUpdater, UpdaterMode};
use super::websocket_listener::WebsocketListener;

/// Bootstrap the collector: create the appropriate updater based on config,
/// spawn WebSocket listeners if needed, and start the update loop.
///
/// Returns a [`CollectorHandle`] that can be used to stop the collector or
/// add new pool addresses while the collector is running.
pub async fn start_collector<P: Provider + Send + Sync + Clone + 'static>(
    provider: Arc<P>,
    config: CollectorConfig,
    pool_registry: Arc<PoolRegistry>,
    metrics: Option<Arc<dyn CollectorMetrics>>,
    swap_event_tx: Option<mpsc::Sender<PendingEvent>>,
) -> Result<CollectorHandle<P>> {
    let chain_id = pool_registry.get_network_id();
    let (cancel_tx, cancel_rx) = oneshot::channel();

    if config.use_websocket {
        info!("[Chain {}] Starting collector with websocket mode", chain_id);
        let event_queue = EventQueue::new(1000, 1000, chain_id);
        let event_sender = event_queue.get_sender();

        let mut ws_listeners: Vec<Arc<WebsocketListener>> =
            Vec::with_capacity(config.websocket_urls.len());

        for url in &config.websocket_urls {
            let listener = Arc::new(WebsocketListener::new(
                url.clone(),
                pool_registry.get_all_addresses(),
                Arc::clone(&event_sender),
                pool_registry.get_topics().clone(),
                chain_id,
            ));
            if let Err(e) = listener.start().await {
                error!("[Chain {}] Websocket listener error: {}", chain_id, e);
            }
            ws_listeners.push(listener);
        }

        let mut updater = UnifiedPoolUpdater::new(
            Arc::clone(&provider),
            Arc::clone(&pool_registry),
            metrics.clone(),
            swap_event_tx.clone(),
            config.start_block,
            config.max_blocks_per_batch,
            UpdaterMode::Websocket { event_queue },
            cancel_rx,
        );

        let updater_handle = tokio::spawn(async move {
            if let Err(e) = updater.start().await {
                error!("[Chain {}] Collector error: {}", chain_id, e);
            }
        });

        let ws_urls = config.websocket_urls.clone();
        Ok(CollectorHandle::new(
            cancel_tx,
            updater_handle,
            ws_listeners,
            ws_urls,
            provider,
            pool_registry,
            metrics,
            swap_event_tx,
            config,
        ))
    } else if config.use_pending_blocks {
        info!("[Chain {}] Starting collector with pending block mode", chain_id);
        let mut updater = UnifiedPoolUpdater::new(
            Arc::clone(&provider),
            Arc::clone(&pool_registry),
            metrics.clone(),
            swap_event_tx.clone(),
            config.start_block,
            config.max_blocks_per_batch,
            UpdaterMode::PendingBlock,
            cancel_rx,
        );

        let updater_handle = tokio::spawn(async move {
            if let Err(e) = updater.start().await {
                error!("[Chain {}] Collector error: {}", chain_id, e);
            }
        });

        Ok(CollectorHandle::new(
            cancel_tx,
            updater_handle,
            vec![],
            vec![],
            provider,
            pool_registry,
            metrics,
            swap_event_tx,
            config,
        ))
    } else {
        info!("[Chain {}] Starting collector with latest block mode", chain_id);
        let mut updater = UnifiedPoolUpdater::new(
            Arc::clone(&provider),
            Arc::clone(&pool_registry),
            metrics.clone(),
            swap_event_tx.clone(),
            config.start_block,
            config.max_blocks_per_batch,
            UpdaterMode::LatestBlock {
                wait_time_ms: config.wait_time,
            },
            cancel_rx,
        );

        let updater_handle = tokio::spawn(async move {
            if let Err(e) = updater.start().await {
                error!("[Chain {}] Collector error: {}", chain_id, e);
            }
        });

        Ok(CollectorHandle::new(
            cancel_tx,
            updater_handle,
            vec![],
            vec![],
            provider,
            pool_registry,
            metrics,
            swap_event_tx,
            config,
        ))
    }
}
