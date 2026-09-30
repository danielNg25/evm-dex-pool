use super::block_source::{log_position, LogPosition};
use super::event_queue::{EventSender, Gap};
use crate::Topic;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::pubsub::{ConnectionHandle, PubSubConnect};
use alloy::rpc::types::{Filter, Log};
use alloy::transports::{TransportErrorKind, TransportResult};
use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::RwLock;
use tokio::time::{interval, sleep, Duration, Instant, MissedTickBehavior};

/// Messages the log subscription may buffer before it drops. alloy's default
/// is 16, and a block carrying more logs for our pools than that arrives as
/// one burst: run 10 (2026-09-28, Avalanche) silently lost 1.3% of its logs
/// that way -- 44 of the 49 blocks with 20+ pool logs lost some, 5 of 8798
/// blocks with under 10 did -- including two Mints whose later Burns then
/// failed on "uninitialized tick".
const LOG_CHANNEL_SIZE: usize = 8192;

/// Buffered headers. A dropped header only costs a header fetch later.
const HEAD_CHANNEL_SIZE: usize = 256;

/// Connects like [`WsConnect`] but never reconnects by itself. alloy's own
/// reconnect resubscribes silently: the subscription carries on as if nothing
/// happened, and the logs emitted while the socket was down are gone without
/// a trace. Refused, the connection ends instead, and the listener reconnects
/// itself and notes the gap.
#[derive(Clone, Debug)]
struct ListenerConnect(WsConnect);

impl PubSubConnect for ListenerConnect {
    fn is_local(&self) -> bool {
        self.0.is_local()
    }

    async fn connect(&self) -> TransportResult<ConnectionHandle> {
        self.0.connect().await
    }

    async fn try_reconnect(&self) -> TransportResult<ConnectionHandle> {
        Err(TransportErrorKind::custom_str(
            "the websocket listener reconnects itself",
        ))
    }
}

/// What a listener's feed has delivered, kept across its connections, and
/// whether logs have gone missing since.
#[derive(Default)]
struct FeedProgress {
    subscribed_before: bool,
    last_forwarded: Option<LogPosition>,
    /// Logs were lost, and the next one forwarded marks where the loss ends.
    gap_open: bool,
}

impl FeedProgress {
    /// A subscription is up. It starts at the current block, so logs before
    /// it are lost: on a reconnect, those emitted while the feed was down; on
    /// the first, any after the source's own catch-up ended (a gap the
    /// source finds empty when that catch-up reached this feed's first log).
    /// Returns whether this was a resubscription.
    fn subscribed(&mut self) -> bool {
        let resubscribed = self.subscribed_before;
        self.subscribed_before = true;
        self.gap_open = true;
        resubscribed
    }

    /// The subscription overflowed and dropped logs.
    fn lagged(&mut self) {
        self.gap_open = true;
    }

    /// `log` is about to be forwarded: the gap it closes, if one is open.
    fn forwarding(&mut self, log: &Log) -> Option<Gap> {
        // A retraction says nothing about where the feed is.
        if log.removed {
            return None;
        }
        let position = log_position(log)?;
        let gap = self.gap_open.then_some(Gap {
            after: self.last_forwarded,
            before: position,
        });
        self.gap_open = false;
        self.last_forwarded = self.last_forwarded.max(Some(position));
        gap
    }
}

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
        info!(
            "[Chain {}] Starting WebSocket listener for {}",
            self.chain_id, self.ws_url
        );

        let ws_url = self.ws_url.clone();
        let pool_addresses = self.pool_addresses.clone();
        let event_sender = Arc::clone(&self.event_sender);
        let is_running = Arc::clone(&self.is_running);
        let last_event_time = Arc::clone(&self.last_event_time);
        let topics = self.topics.read().await.clone();
        let chain_id = self.chain_id;

        tokio::spawn(async move {
            let mut progress = FeedProgress::default();
            while *is_running.read().await {
                match Self::connect_and_listen(
                    &ws_url,
                    &pool_addresses,
                    &event_sender,
                    &last_event_time,
                    topics.clone(),
                    chain_id,
                    &mut progress,
                )
                .await
                {
                    Ok(_) => {
                        info!(
                            "[Chain {}] WebSocket connection closed for {}",
                            chain_id, ws_url
                        );
                    }
                    Err(e) => {
                        error!(
                            "[Chain {}] WebSocket connection error for {}: {:#}",
                            chain_id, ws_url, e
                        );
                    }
                }

                sleep(Duration::from_secs(2)).await;
                info!(
                    "[Chain {}] Attempting to reconnect to WebSocket at {}",
                    chain_id, ws_url
                );
            }
        });

        Ok(())
    }

    /// Stops the WebSocket listener
    pub async fn stop(&self) -> Result<()> {
        *self.is_running.write().await = false;
        info!(
            "[Chain {}] Stopping WebSocket listener for {}",
            self.chain_id, self.ws_url
        );
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
        progress: &mut FeedProgress,
    ) -> Result<()> {
        // Connect to the WebSocket (supports wss:// URLs). One reconnect
        // attempt, which `ListenerConnect` refuses, so a drop ends the
        // service at once and the subscriptions below close.
        let ws_provider = ProviderBuilder::new()
            .connect_pubsub_with(ListenerConnect(WsConnect::new(ws_url).with_max_retries(1)))
            .await
            .context("Failed to connect to WebSocket")?;

        info!("[Chain {}] Connected to WebSocket at {}", chain_id, ws_url);

        // Subscribe to logs (starts from current block)

        let filter = Filter::new()
            .address(pool_addresses.to_vec())
            .event_signature(topics);

        let mut subscription = ws_provider
            .subscribe_logs(&filter)
            .channel_size(LOG_CHANNEL_SIZE)
            .await
            .context("Failed to subscribe to logs")?;

        info!(
            "[Chain {}] Subscribed to logs for {} pool addresses at {}",
            chain_id,
            pool_addresses.len(),
            ws_url
        );

        if progress.subscribed() {
            info!(
                "[Chain {}] Resubscribed at {}; the logs missed after {:?} end at the next to arrive",
                chain_id, ws_url, progress.last_forwarded
            );
        }

        // Block headers on the same connection: a subscribed log carries no
        // block time, and LB pools date their variable-fee decay from it. With
        // each header's time recorded as the block arrives, the timestamp
        // enrichment finds it instead of fetching the header. Without the
        // subscription the enrichment still fetches, so logs keep flowing.
        let mut heads = match ws_provider
            .subscribe_blocks()
            .channel_size(HEAD_CHANNEL_SIZE)
            .await
        {
            Ok(subscription) => {
                info!("[Chain {}] Subscribed to new heads at {}", chain_id, ws_url);
                Some(subscription)
            }
            Err(e) => {
                warn!(
                    "[Chain {}] New-heads subscription failed at {}; LB log times will be fetched: {}",
                    chain_id, ws_url, e
                );
                None
            }
        };
        let have_heads = heads.is_some();

        // `last_event_time` outlives connections; left stale, the new
        // heartbeat's first tick would read the previous connection's stall
        // and tear this one down before anything could arrive.
        *last_event_time.write().await = Instant::now();

        // Start pinging and stall detection task. The stall check needs a
        // steady signal: with new heads a live connection hears something
        // every block, so silence means a dead socket. Without them a quiet
        // pool set is silent too, and a reconnect loses the logs emitted
        // while it happens, so only failed pings reconnect then.
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

                if have_heads
                    && last_event_time_clone.read().await.elapsed() > Duration::from_secs(180)
                {
                    warn!(
                        "[Chain {}] No events received for 180 seconds at {}; forcing reconnect",
                        chain_id, ws_url_clone
                    );
                    break;
                }

                match provider_clone.get_block_number().await {
                    Ok(_) => {
                        ping_failures = 0;
                        debug!(
                            "[Chain {}] Sent heartbeat ping for {}",
                            chain_id, ws_url_clone
                        );
                    }
                    // The server answered, so the connection is alive; it just
                    // does not serve this method over websocket. Avalanche's
                    // public endpoint answers eth_blockNumber with -32601, and
                    // counting that as a failure reconnected every 90 s
                    // (run 10, 2026-09-28), dropping logs across each gap.
                    Err(e) if e.as_error_resp().is_some() => {
                        ping_failures = 0;
                        debug!(
                            "[Chain {}] Heartbeat answered with an error, connection alive at {}: {}",
                            chain_id, ws_url_clone, e
                        );
                    }
                    Err(e) => {
                        ping_failures += 1;
                        error!(
                            "[Chain {}] Ping failed for {}: {}",
                            chain_id, ws_url_clone, e
                        );
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

        // Process WebSocket events. Either subscription ending ends the
        // connection, and the caller reconnects. So does the heartbeat task
        // giving up (stall or failed pings): it clears `is_running`, which the
        // check below sees, since a dead socket need not end its stream.
        // `recv()` rather than a stream: the stream adapters skip a lagged
        // (overflowed) channel silently, and a dropped log must be loud.
        let mut heartbeat_check = interval(Duration::from_secs(5));
        heartbeat_check.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = heartbeat_check.tick() => {
                    if !*is_running.read().await {
                        warn!(
                            "[Chain {}] Heartbeat gave up on {}; reconnecting",
                            chain_id, ws_url
                        );
                        break;
                    }
                }
                event_log = subscription.recv() => {
                    let event_log = match event_log {
                        Ok(event_log) => event_log,
                        Err(RecvError::Lagged(dropped)) => {
                            error!(
                                "[Chain {}] Log subscription at {} overflowed and dropped {} log(s); \
                                 those pools' state may be stale",
                                chain_id, ws_url, dropped
                            );
                            progress.lagged();
                            continue;
                        }
                        Err(RecvError::Closed) => break,
                    };
                    debug!(
                        "[Chain {}] Received log: address={}, topics={:?}",
                        chain_id,
                        event_log.address(),
                        event_log.topics()
                    );

                    // Update last event time
                    *last_event_time.write().await = Instant::now();

                    // Noted before the log goes on, so the source sees the
                    // gap no later than the log that ends it.
                    if let Some(gap) = progress.forwarding(&event_log) {
                        debug!(
                            "[Chain {}] Logs lost at {} after {:?}, before {:?}",
                            chain_id, ws_url, gap.after, gap.before
                        );
                        event_sender.note_gap(gap);
                    }

                    if let Err(e) = event_sender.send(event_log).await {
                        // Channel closed is expected during shutdown (EventQueue receiver
                        // dropped when the updater exits). Log at debug, not error.
                        debug!("[Chain {}] Failed to send event to queue: {}", chain_id, e);
                    }
                }
                header = async {
                    match heads.as_mut() {
                        Some(heads) => heads.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let header = match header {
                        Ok(header) => header,
                        // A dropped header only means a header fetch later.
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    };
                    event_sender.record_block_time(header.number, header.timestamp);
                    // A header proves the connection is live, so a quiet pool
                    // set no longer trips the 180 s stall reconnect. The
                    // trade-off: a provider that silently drops only the log
                    // subscription while heads keep flowing goes unnoticed.
                    *last_event_time.write().await = Instant::now();
                }
            }
        }

        // Stop pinging task
        *is_running.write().await = false;
        info!(
            "[Chain {}] WebSocket subscription ended for {}",
            chain_id, ws_url
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_at(block: u64, tx_index: u64, log_index: u64) -> Log {
        Log {
            block_number: Some(block),
            transaction_index: Some(tx_index),
            log_index: Some(log_index),
            ..Default::default()
        }
    }

    /// Every subscription opens a gap, which the next log forwarded closes:
    /// the gap ends before it. The first has no start.
    #[test]
    fn a_subscription_opens_a_gap_the_next_log_closes() {
        let mut progress = FeedProgress::default();
        assert!(!progress.subscribed());
        assert_eq!(
            progress.forwarding(&log_at(100, 0, 0)),
            Some(Gap {
                after: None,
                before: (100, 0, 0)
            })
        );
        assert_eq!(progress.forwarding(&log_at(100, 2, 1)), None);

        assert!(progress.subscribed());
        assert_eq!(
            progress.forwarding(&log_at(103, 1, 0)),
            Some(Gap {
                after: Some((100, 2, 1)),
                before: (103, 1, 0)
            })
        );
        assert_eq!(progress.forwarding(&log_at(103, 2, 0)), None);
    }

    /// A feed that had forwarded nothing leaves the gap's start open.
    #[test]
    fn a_gap_before_any_log_has_no_start() {
        let mut progress = FeedProgress::default();
        progress.subscribed();
        progress.subscribed();
        assert_eq!(
            progress.forwarding(&log_at(103, 0, 0)),
            Some(Gap {
                after: None,
                before: (103, 0, 0)
            })
        );
    }

    /// An overflowed subscription is a gap like a reconnect.
    #[test]
    fn an_overflow_opens_a_gap() {
        let mut progress = FeedProgress::default();
        progress.subscribed();
        progress.forwarding(&log_at(100, 0, 0));
        progress.lagged();
        assert_eq!(
            progress.forwarding(&log_at(101, 4, 0)),
            Some(Gap {
                after: Some((100, 0, 0)),
                before: (101, 4, 0)
            })
        );
    }

    /// A retraction, or a log without a chain position, neither closes a gap
    /// nor moves the feed's place.
    #[test]
    fn a_removed_or_unplaced_log_leaves_the_gap_open() {
        let mut progress = FeedProgress::default();
        progress.subscribed();
        progress.forwarding(&log_at(100, 0, 0));
        progress.subscribed();

        let removed = Log {
            removed: true,
            ..log_at(102, 0, 0)
        };
        assert_eq!(progress.forwarding(&removed), None);
        let unplaced = Log {
            log_index: None,
            ..log_at(102, 1, 0)
        };
        assert_eq!(progress.forwarding(&unplaced), None);

        assert_eq!(
            progress.forwarding(&log_at(102, 3, 0)),
            Some(Gap {
                after: Some((100, 0, 0)),
                before: (102, 3, 0)
            })
        );
    }
}
