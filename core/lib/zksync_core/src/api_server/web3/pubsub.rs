//! (Largely) backend-agnostic logic for dealing with Web3 subscriptions.

use anyhow::Context as _;
use futures::FutureExt;
use tokio::{
    sync::{broadcast, mpsc, watch},
    task::JoinHandle,
    time::{interval, Duration},
};
use zksync_dal::ConnectionPool;
use zksync_types::{MiniblockNumber, H128, H256};
use zksync_web3_decl::{
    jsonrpsee::{
        core::{server::SubscriptionMessage, SubscriptionResult},
        server::IdProvider,
        types::{error::ErrorCode, ErrorObject, SubscriptionId},
        PendingSubscriptionSink, SendTimeoutError, SubscriptionSink,
    },
    namespaces::EthPubSubServer,
    types::{BlockHeader, Log, PubSubFilter, PubSubResult},
};

use super::{
    metrics::{SubscriptionType, PUB_SUB_METRICS},
    namespaces::eth::EVENT_TOPIC_NUMBER_LIMIT,
};

const BROADCAST_CHANNEL_CAPACITY: usize = 1024;
const SUBSCRIPTION_SINK_SEND_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
pub struct EthSubscriptionIdProvider;

impl IdProvider for EthSubscriptionIdProvider {
    fn next_id(&self) -> SubscriptionId<'static> {
        let id = H128::random();
        format!("0x{}", hex::encode(id.0)).into()
    }
}

/// Events emitted by the subscription logic. Only used in WebSocket server tests so far.
#[derive(Debug)]
pub(super) enum PubSubEvent {
    Subscribed(SubscriptionType),
    NotifyIterationFinished(SubscriptionType),
}

/// Manager of notifications for a certain type of subscriptions.
#[derive(Debug)]
struct PubSubNotifier {
    sender: broadcast::Sender<Vec<PubSubResult>>,
    connection_pool: ConnectionPool,
    polling_interval: Duration,
    events_sender: Option<mpsc::UnboundedSender<PubSubEvent>>,
}

impl PubSubNotifier {
    async fn sealed_miniblock_number(&self) -> anyhow::Result<MiniblockNumber> {
        self.connection_pool
            .access_storage_tagged("api")
            .await
            .context("access_storage_tagged")?
            .blocks_web3_dal()
            .get_sealed_miniblock_number()
            .await
            .context("get_sealed_miniblock_number()")
    }

    fn emit_event(&self, event: PubSubEvent) {
        if let Some(sender) = &self.events_sender {
            sender.send(event).ok();
        }
    }
}

impl PubSubNotifier {
    async fn notify_blocks(self, stop_receiver: watch::Receiver<bool>) -> anyhow::Result<()> {
        let mut last_block_number = self.sealed_miniblock_number().await?;
        let mut timer = interval(self.polling_interval);
        loop {
            if *stop_receiver.borrow() {
                tracing::info!("Stop signal received, pubsub_block_notifier is shutting down");
                break;
            }
            timer.tick().await;

            let db_latency = PUB_SUB_METRICS.db_poll_latency[&SubscriptionType::Blocks].start();
            let new_blocks = self.new_blocks(last_block_number).await?;
            db_latency.observe();

            if let Some(last_block) = new_blocks.last() {
                last_block_number = MiniblockNumber(last_block.number.unwrap().as_u32());

                let new_blocks = new_blocks.into_iter().map(PubSubResult::Header).collect();
                // Errors only on 0 receivers, but we want to go on
                // if we have 0 subscribers so ignore the error.
                self.sender.send(new_blocks).ok();
            }
            self.emit_event(PubSubEvent::NotifyIterationFinished(
                SubscriptionType::Blocks,
            ));
        }
        Ok(())
    }

    async fn new_blocks(
        &self,
        last_block_number: MiniblockNumber,
    ) -> anyhow::Result<Vec<BlockHeader>> {
        self.connection_pool
            .access_storage_tagged("api")
            .await
            .context("access_storage_tagged")?
            .blocks_web3_dal()
            .get_block_headers_after(last_block_number)
            .await
            .with_context(|| format!("get_block_headers_after({last_block_number})"))
    }
}

impl PubSubNotifier {
    async fn notify_txs(self, stop_receiver: watch::Receiver<bool>) -> anyhow::Result<()> {
        let mut last_time = chrono::Utc::now().naive_utc();
        let mut timer = interval(self.polling_interval);
        loop {
            if *stop_receiver.borrow() {
                tracing::info!("Stop signal received, pubsub_tx_notifier is shutting down");
                break;
            }
            timer.tick().await;

            let db_latency = PUB_SUB_METRICS.db_poll_latency[&SubscriptionType::Txs].start();
            let (new_txs, new_last_time) = self.new_txs(last_time).await?;
            db_latency.observe();

            if let Some(new_last_time) = new_last_time {
                last_time = new_last_time;

                let new_txs = new_txs.into_iter().map(PubSubResult::TxHash).collect();
                // Errors only on 0 receivers, but we want to go on
                // if we have 0 subscribers so ignore the error.
                self.sender.send(new_txs).ok();
            }
            self.emit_event(PubSubEvent::NotifyIterationFinished(SubscriptionType::Txs));
        }
        Ok(())
    }

    async fn new_txs(
        &self,
        last_time: chrono::NaiveDateTime,
    ) -> anyhow::Result<(Vec<H256>, Option<chrono::NaiveDateTime>)> {
        self.connection_pool
            .access_storage_tagged("api")
            .await
            .context("access_storage_tagged")?
            .transactions_web3_dal()
            .get_pending_txs_hashes_after(last_time, None)
            .await
            .context("get_pending_txs_hashes_after()")
    }
}

impl PubSubNotifier {
    async fn notify_logs(self, stop_receiver: watch::Receiver<bool>) -> anyhow::Result<()> {
        let mut last_block_number = self.sealed_miniblock_number().await?;
        let mut timer = interval(self.polling_interval);
        loop {
            if *stop_receiver.borrow() {
                tracing::info!("Stop signal received, pubsub_logs_notifier is shutting down");
                break;
            }
            timer.tick().await;

            let db_latency = PUB_SUB_METRICS.db_poll_latency[&SubscriptionType::Logs].start();
            let new_logs = self.new_logs(last_block_number).await?;
            db_latency.observe();

            if let Some(last_log) = new_logs.last() {
                last_block_number = MiniblockNumber(last_log.block_number.unwrap().as_u32());

                let new_logs = new_logs.into_iter().map(PubSubResult::Log).collect();

                // Errors only on 0 receivers, but we want to go on
                // if we have 0 subscribers so ignore the error.
                self.sender.send(new_logs).ok();
            }
            self.emit_event(PubSubEvent::NotifyIterationFinished(SubscriptionType::Logs));
        }
        Ok(())
    }

    async fn new_logs(&self, last_block_number: MiniblockNumber) -> anyhow::Result<Vec<Log>> {
        self.connection_pool
            .access_storage_tagged("api")
            .await
            .context("access_storage_tagged")?
            .events_web3_dal()
            .get_all_logs(last_block_number)
            .await
            .context("events_web3_dal().get_all_logs()")
    }
}

/// Subscription support for Web3 APIs.
pub(super) struct EthSubscribe {
    blocks: broadcast::Sender<Vec<PubSubResult>>,
    transactions: broadcast::Sender<Vec<PubSubResult>>,
    logs: broadcast::Sender<Vec<PubSubResult>>,
    events_sender: Option<mpsc::UnboundedSender<PubSubEvent>>,
}

impl EthSubscribe {
    pub fn new() -> Self {
        let (blocks, _) = broadcast::channel(BROADCAST_CHANNEL_CAPACITY);
        let (transactions, _) = broadcast::channel(BROADCAST_CHANNEL_CAPACITY);
        let (logs, _) = broadcast::channel(BROADCAST_CHANNEL_CAPACITY);

        Self {
            blocks,
            transactions,
            logs,
            events_sender: None,
        }
    }

    pub fn set_events_sender(&mut self, sender: mpsc::UnboundedSender<PubSubEvent>) {
        self.events_sender = Some(sender);
    }

    async fn reject(sink: PendingSubscriptionSink) {
        sink.reject(ErrorObject::borrowed(
            ErrorCode::InvalidParams.code(),
            "Rejecting subscription - invalid parameters provided.",
            None,
        ))
        .await;
    }

    async fn run_subscriber(
        sink: SubscriptionSink,
        subscription_type: SubscriptionType,
        mut b: broadcast::Receiver<Vec<PubSubResult>>,
        filter: Option<PubSubFilter>,
    ) {
        let _guard = PUB_SUB_METRICS.active_subscribers[&subscription_type].inc_guard(1);
        let closed = sink.closed().fuse();
        tokio::pin!(closed);

        loop {
            tokio::select! {
                new_items = b.recv() => {
                    let Ok(new_items) = new_items else { return };
                    if Self::handle_new_items(
                        &sink,
                        subscription_type,
                        new_items,
                        filter.as_ref()
                    ).await.is_err() { return };
                }
                _ = &mut closed => {
                    break
                }
            }
        }
    }

    async fn handle_new_items(
        sink: &SubscriptionSink,
        subscription_type: SubscriptionType,
        new_items: Vec<PubSubResult>,
        filter: Option<&PubSubFilter>,
    ) -> Result<(), SendTimeoutError> {
        for item in new_items {
            if let PubSubResult::Log(log) = &item {
                if let Some(filter) = &filter {
                    if !filter.matches(log) {
                        continue;
                    }
                }
            }

            sink.send_timeout(
                SubscriptionMessage::from_json(&item)
                    .expect("PubSubResult always serializable to json;qed"),
                SUBSCRIPTION_SINK_SEND_TIMEOUT,
            )
            .await?;

            PUB_SUB_METRICS.notify[&subscription_type].inc();
        }

        Ok(())
    }

    #[tracing::instrument(skip(self, pending_sink))]
    pub async fn sub(
        &self,
        pending_sink: PendingSubscriptionSink,
        sub_type: String,
        params: Option<PubSubFilter>,
    ) {
        let sub_type = match sub_type.as_str() {
            "newHeads" => {
                let blocks_rx = self.blocks.subscribe();

                let Ok(sink) = pending_sink.accept().await else {
                    return;
                };
                tokio::spawn(Self::run_subscriber(
                    sink,
                    SubscriptionType::Blocks,
                    blocks_rx,
                    None,
                ));

                Some(SubscriptionType::Blocks)
            }
            "newPendingTransactions" => {
                let transactions_rx = self.transactions.subscribe();

                let Ok(sink) = pending_sink.accept().await else {
                    return;
                };
                tokio::spawn(Self::run_subscriber(
                    sink,
                    SubscriptionType::Txs,
                    transactions_rx,
                    None,
                ));
                Some(SubscriptionType::Txs)
            }
            "logs" => {
                let logs_rx = self.logs.subscribe();
                let filter = params.unwrap_or_default();
                let topic_count = filter.topics.as_ref().map_or(0, Vec::len);

                if topic_count > EVENT_TOPIC_NUMBER_LIMIT {
                    Self::reject(pending_sink).await;
                    None
                } else {
                    let Ok(sink) = pending_sink.accept().await else {
                        return;
                    };
                    tokio::spawn(Self::run_subscriber(
                        sink,
                        SubscriptionType::Logs,
                        logs_rx,
                        Some(filter),
                    ));
                    Some(SubscriptionType::Logs)
                }
            }
            "syncing" => {
                let Ok(sink) = pending_sink.accept().await else {
                    return;
                };

                tokio::spawn(async move {
                    sink.send_timeout(
                        SubscriptionMessage::from_json(&PubSubResult::Syncing(false)).unwrap(),
                        SUBSCRIPTION_SINK_SEND_TIMEOUT,
                    )
                    .await
                });
                None
            }
            _ => {
                Self::reject(pending_sink).await;
                None
            }
        };

        if let Some(sub_type) = sub_type {
            if let Some(sender) = &self.events_sender {
                sender.send(PubSubEvent::Subscribed(sub_type)).ok();
            }
        }
    }

    /// Spawns notifier tasks. This should be called once per instance.
    pub fn spawn_notifiers(
        &self,
        connection_pool: ConnectionPool,
        polling_interval: Duration,
        stop_receiver: watch::Receiver<bool>,
    ) -> Vec<JoinHandle<anyhow::Result<()>>> {
        let mut notifier_tasks = Vec::with_capacity(3);

        let notifier = PubSubNotifier {
            sender: self.blocks.clone(),
            connection_pool: connection_pool.clone(),
            polling_interval,
            events_sender: self.events_sender.clone(),
        };
        let notifier_task = tokio::spawn(notifier.notify_blocks(stop_receiver.clone()));
        notifier_tasks.push(notifier_task);

        let notifier = PubSubNotifier {
            sender: self.transactions.clone(),
            connection_pool: connection_pool.clone(),
            polling_interval,
            events_sender: self.events_sender.clone(),
        };
        let notifier_task = tokio::spawn(notifier.notify_txs(stop_receiver.clone()));
        notifier_tasks.push(notifier_task);

        let notifier = PubSubNotifier {
            sender: self.logs.clone(),
            connection_pool,
            polling_interval,
            events_sender: self.events_sender.clone(),
        };
        let notifier_task = tokio::spawn(notifier.notify_logs(stop_receiver));

        notifier_tasks.push(notifier_task);
        notifier_tasks
    }
}

#[async_trait::async_trait]
impl EthPubSubServer for EthSubscribe {
    async fn subscribe(
        &self,
        pending: PendingSubscriptionSink,
        sub_type: String,
        filter: Option<PubSubFilter>,
    ) -> SubscriptionResult {
        self.sub(pending, sub_type, filter).await;

        Ok(())
    }
}
