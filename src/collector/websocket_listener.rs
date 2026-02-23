use super::event_queue::EventSender;
use crate::Topic;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use anyhow::{Context, Result};
use futures_util::stream::StreamExt;
use log::{debug, error, info, warn};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, sleep, Duration, Instant, MissedTickBehavior};

pub struct WebsocketListener {
    ws_url: String,
    pool_addresses: Vec<Address>,
    event_sender: Arc<EventSender>,
    is_running: Arc<RwLock<bool>>,
    last_event_time: Arc<RwLock<Instant>>,
    topics: Arc<RwLock<Vec<Topic>>>,
    chain_id: u64,
}

impl WebsocketListener {
    /// Creates a new WebSocket listener
    pub fn new(
        ws_url: String,
        pool_addresses: Vec<Address>,
        event_sender: Arc<EventSender>,
        topics: Vec<Topic>,
        chain_id: u64,
    ) -> Self {
        Self {
            ws_url,
            pool_addresses,
            event_sender,
            is_running: Arc::new(RwLock::new(false)),
            last_event_time: Arc::new(RwLock::new(Instant::now())),
            topics: Arc::new(RwLock::new(topics)),
            chain_id,
        }
    }

    /// Starts the WebSocket listener in a background task
    pub async fn start(&self) -> Result<()> {
        *self.is_running.write().await = true;
        info!("[Chain {}] Starting WebSocket listener for {}", self.chain_id, self.ws_url);

        let ws_url = self.ws_url.clone();
        let pool_addresses = self.pool_addresses.clone();
        let event_sender = Arc::clone(&self.event_sender);
        let is_running = Arc::clone(&self.is_running);
        let last_event_time = Arc::clone(&self.last_event_time);
        let topics = self.topics.read().await.clone();
        let chain_id = self.chain_id;

        tokio::spawn(async move {
            while *is_running.read().await {
                match Self::connect_and_listen(
                    &ws_url,
                    &pool_addresses,
                    &event_sender,
                    &last_event_time,
                    topics.clone(),
                    chain_id,
                )
                .await
                {
                    Ok(_) => {
                        info!("[Chain {}] WebSocket connection closed for {}", chain_id, ws_url);
                    }
                    Err(e) => {
                        error!("[Chain {}] WebSocket connection error for {}: {}", chain_id, ws_url, e);
                    }
                }

                sleep(Duration::from_secs(2)).await;
                info!("[Chain {}] Attempting to reconnect to WebSocket at {}", chain_id, ws_url);
            }
        });

        Ok(())
    }

    /// Stops the WebSocket listener
    pub async fn stop(&self) -> Result<()> {
        *self.is_running.write().await = false;
        info!("[Chain {}] Stopping WebSocket listener for {}", self.chain_id, self.ws_url);
        Ok(())
    }

    /// Connects to the WebSocket, subscribes, and listens for events
    async fn connect_and_listen(
        ws_url: &str,
        pool_addresses: &[Address],
        event_sender: &Arc<EventSender>,
        last_event_time: &Arc<RwLock<Instant>>,
        topics: Vec<Topic>,
        chain_id: u64,
    ) -> Result<()> {
        // Connect to the WebSocket
        let ws_provider = ProviderBuilder::new()
            .connect(ws_url)
            .await
            .context("Failed to connect to WebSocket")?;

        info!("[Chain {}] Connected to WebSocket at {}", chain_id, ws_url);

        // Subscribe to logs (starts from current block)

        let filter = Filter::new()
            .address(pool_addresses.to_vec())
            .event_signature(topics);

        let subscription = ws_provider
            .subscribe_logs(&filter)
            .await
            .context("Failed to subscribe to logs")?;

        info!(
            "[Chain {}] Subscribed to logs for {} pool addresses at {}",
            chain_id,
            pool_addresses.len(),
            ws_url
        );

        // Start pinging and stall detection task
        let provider_clone = ws_provider.clone();
        let is_running = Arc::new(RwLock::new(true));
        let ping_running = Arc::clone(&is_running);
        let last_event_time_clone = Arc::clone(last_event_time);
        let ws_url_clone = ws_url.to_string();
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(30));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut ping_failures = 0;
            const MAX_PING_FAILURES: u32 = 3;

            while *ping_running.read().await {
                interval.tick().await;

                if last_event_time_clone.read().await.elapsed() > Duration::from_secs(180) {
                    warn!(
                        "[Chain {}] No events received for 180 seconds at {}; forcing reconnect",
                        chain_id, ws_url_clone
                    );
                    break;
                }

                match provider_clone.get_block_number().await {
                    Ok(_) => {
                        ping_failures = 0;
                        debug!("[Chain {}] Sent heartbeat ping for {}", chain_id, ws_url_clone);
                    }
                    Err(e) => {
                        ping_failures += 1;
                        error!("[Chain {}] Ping failed for {}: {}", chain_id, ws_url_clone, e);
                        if ping_failures >= MAX_PING_FAILURES {
                            warn!(
                                "[Chain {}] Max ping failures ({}) reached for {}; forcing reconnect",
                                chain_id, MAX_PING_FAILURES, ws_url_clone
                            );
                            break;
                        }
                    }
                }
            }

            *ping_running.write().await = false;
        });

        // Process WebSocket events
        let mut stream = subscription.into_stream();
        while let Some(event_log) = stream.next().await {
            debug!(
                "[Chain {}] Received log: address={}, topics={:?}",
                chain_id,
                event_log.address(),
                event_log.topics()
            );

            // Update last event time
            *last_event_time.write().await = Instant::now();

            if let Err(e) = event_sender.send(event_log).await {
                error!("[Chain {}] Failed to send event to queue: {}", chain_id, e);
            }
        }

        // Stop pinging task
        *is_running.write().await = false;
        info!("[Chain {}] WebSocket subscription ended for {}", chain_id, ws_url);
        Ok(())
    }
}
