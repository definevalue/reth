//! Transactions management for the p2p network.

use crate::{
    cache::LruCache,
    manager::NetworkEvent,
    message::{PeerRequest, PeerRequestSender},
    network::NetworkHandleMessage,
    peers::ReputationChangeKind,
    NetworkHandle,
};
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use reth_eth_wire::{
    GetPooledTransactions, NewPooledTransactionHashes, PooledTransactions, Transactions,
};
use reth_interfaces::p2p::error::RequestResult;
use reth_primitives::{
    FromRecoveredTransaction, IntoRecoveredTransaction, PeerId, TransactionSigned, TxHash, H256,
};
use reth_transaction_pool::{
    error::PoolResult, PropagateKind, PropagatedTransactions, TransactionPool,
};
use std::{
    collections::{hash_map::Entry, HashMap},
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::{ReceiverStream, UnboundedReceiverStream};
use tracing::trace;

/// Cache limit of transactions to keep track of for a single peer.
const PEER_TRANSACTION_CACHE_LIMIT: usize = 1024 * 10;

/// The future for inserting a function into the pool
pub type PoolImportFuture = Pin<Box<dyn Future<Output = PoolResult<TxHash>> + Send + 'static>>;

/// Api to interact with [`TransactionsManager`] task.
// ANCHOR: struct-TransactionsHandle
pub struct TransactionsHandle {
    /// Command channel to the [`TransactionsManager`]
    manager_tx: mpsc::UnboundedSender<TransactionsCommand>,
}
// ANCHOR_END: struct-TransactionsHandle

// === impl TransactionsHandle ===

impl TransactionsHandle {
    fn send(&self, cmd: TransactionsCommand) {
        let _ = self.manager_tx.send(cmd);
    }

    /// Manually propagate the transaction that belongs to the hash.
    pub fn propagate(&self, hash: TxHash) {
        self.send(TransactionsCommand::PropagateHash(hash))
    }
}

/// Manages transactions on top of the p2p network.
///
/// This can be spawned to another task and is supposed to be run as background service while
/// [`TransactionsHandle`] is used as frontend to send commands to.
///
/// The [`TransactionsManager`] is responsible for:
///    - handling incoming eth messages for transactions.
///    - serving transaction requests.
///    - propagate transactions
///
/// This type communicates with the [`NetworkManager`](crate::NetworkManager) in both directions.
///   - receives incoming network messages.
///   - sends messages to dispatch (responses, propagate tx)
///
/// It is directly connected to the [`TransactionPool`] to retrieve requested transactions and
/// propagate new transactions over the network.
// ANCHOR: struct-TransactionsManager
#[must_use = "Manager does nothing unless polled."]
pub struct TransactionsManager<Pool> {
    /// Access to the transaction pool.
    pool: Pool,
    /// Network access.
    network: NetworkHandle,
    /// Subscriptions to all network related events.
    ///
    /// From which we get all new incoming transaction related messages.
    network_events: UnboundedReceiverStream<NetworkEvent>,
    /// All currently active requests for pooled transactions.
    inflight_requests: Vec<GetPooledTxRequest>,
    /// All currently pending transactions grouped by peers.
    ///
    /// This way we can track incoming transactions and prevent multiple pool imports for the same
    /// transaction
    transactions_by_peers: HashMap<TxHash, Vec<PeerId>>,
    /// Transactions that are currently imported into the `Pool`
    pool_imports: FuturesUnordered<PoolImportFuture>,
    /// All the connected peers.
    peers: HashMap<PeerId, Peer>,
    /// Send half for the command channel.
    command_tx: mpsc::UnboundedSender<TransactionsCommand>,
    /// Incoming commands from [`TransactionsHandle`].
    command_rx: UnboundedReceiverStream<TransactionsCommand>,
    /// Incoming commands from [`TransactionsHandle`].
    pending_transactions: ReceiverStream<TxHash>,
    /// Incoming events from the [`NetworkManager`](crate::NetworkManager).
    transaction_events: UnboundedReceiverStream<NetworkTransactionEvent>,
}
// ANCHOR_END: struct-TransactionsManager

impl<Pool: TransactionPool> TransactionsManager<Pool> {
    /// Sets up a new instance.
    ///
    /// Note: This expects an existing [`NetworkManager`](crate::NetworkManager) instance.
    pub fn new(
        network: NetworkHandle,
        pool: Pool,
        from_network: mpsc::UnboundedReceiver<NetworkTransactionEvent>,
    ) -> Self {
        let network_events = network.event_listener();
        let (command_tx, command_rx) = mpsc::unbounded_channel();

        // install a listener for new transactions
        let pending = pool.pending_transactions_listener();

        Self {
            pool,
            network,
            network_events,
            inflight_requests: Default::default(),
            transactions_by_peers: Default::default(),
            pool_imports: Default::default(),
            peers: Default::default(),
            command_tx,
            command_rx: UnboundedReceiverStream::new(command_rx),
            pending_transactions: ReceiverStream::new(pending),
            transaction_events: UnboundedReceiverStream::new(from_network),
        }
    }
}

// === impl TransactionsManager ===

impl<Pool> TransactionsManager<Pool>
where
    Pool: TransactionPool + 'static,
    <Pool as TransactionPool>::Transaction: IntoRecoveredTransaction,
{
    /// Returns a new handle that can send commands to this type.
    pub fn handle(&self) -> TransactionsHandle {
        TransactionsHandle { manager_tx: self.command_tx.clone() }
    }

    /// Request handler for an incoming request for transactions
    // ANCHOR: fn-on_get_pooled_transactions
    fn on_get_pooled_transactions(
        &mut self,
        peer_id: PeerId,
        request: GetPooledTransactions,
        response: oneshot::Sender<RequestResult<PooledTransactions>>,
    ) {
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            let transactions = self
                .pool
                .get_all(request.0)
                .into_iter()
                .map(|tx| tx.transaction.to_recovered_transaction().into_signed())
                .collect::<Vec<_>>();

            // we sent a response at which point we assume that the peer is aware of the transaction
            peer.transactions.extend(transactions.iter().map(|tx| tx.hash()));

            let resp = PooledTransactions(transactions);
            let _ = response.send(Ok(resp));
        }
    }
    // ANCHOR_END: fn-on_get_pooled_transactions

    /// Invoked when a new transaction is pending.
    ///
    /// When new transactions appear in the pool, we propagate them to the network using the
    /// `Transactions` and `NewPooledTransactionHashes` messages. The Transactions message relays
    /// complete transaction objects and is typically sent to a small, random fraction of connected
    /// peers.
    ///
    /// All other peers receive a notification of the transaction hash and can request the
    /// complete transaction object if it is unknown to them. The dissemination of complete
    /// transactions to a fraction of peers usually ensures that all nodes receive the transaction
    /// and won't need to request it.
    // ANCHOR: fn-on_new_transactions-propagate_transactions
    fn on_new_transactions(&mut self, hashes: impl IntoIterator<Item = TxHash>) {
        trace!(target: "net::tx", "Start propagating transactions");

        let propagated = self.propagate_transactions(
            self.pool
                .get_all(hashes)
                .into_iter()
                .map(|tx| {
                    (*tx.hash(), Arc::new(tx.transaction.to_recovered_transaction().into_signed()))
                })
                .collect(),
        );

        // notify pool so events get fired
        self.pool.on_propagated(propagated);
    }

    fn propagate_transactions(
        &mut self,
        txs: Vec<(TxHash, Arc<TransactionSigned>)>,
    ) -> PropagatedTransactions {
        let mut propagated = PropagatedTransactions::default();

        // send full transactions to a fraction fo the connected peers (square root of the total
        // number of connected peers)
        let max_num_full = (self.peers.len() as f64).sqrt() as usize + 1;

        // Note: Assuming ~random~ order due to random state of the peers map hasher
        for (idx, (peer_id, peer)) in self.peers.iter_mut().enumerate() {
            let (hashes, full): (Vec<_>, Vec<_>) =
                txs.iter().filter(|(hash, _)| peer.transactions.insert(*hash)).cloned().unzip();

            if !full.is_empty() {
                if idx > max_num_full {
                    for hash in &hashes {
                        propagated.0.entry(*hash).or_default().push(PropagateKind::Hash(*peer_id));
                    }
                    // send hashes of transactions
                    self.network.send_transactions_hashes(*peer_id, hashes);
                } else {
                    // send full transactions
                    self.network.send_transactions(*peer_id, full);

                    for hash in hashes {
                        propagated.0.entry(hash).or_default().push(PropagateKind::Full(*peer_id));
                    }
                }
            }
        }

        propagated
    }
    // ANCHOR_END: fn-on_new_transactions-propagate_transactions

    /// Request handler for an incoming `NewPooledTransactionHashes`
    // ANCHOR: fn-on_new_pooled_transactions
    fn on_new_pooled_transactions(&mut self, peer_id: PeerId, msg: NewPooledTransactionHashes) {
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            let mut transactions = msg.0;

            // keep track of the transactions the peer knows
            peer.transactions.extend(transactions.clone());

            self.pool.retain_unknown(&mut transactions);

            if transactions.is_empty() {
                // nothing to request
                return
            }

            // request the missing transactions
            let (response, rx) = oneshot::channel();
            let req = PeerRequest::GetPooledTransactions {
                request: GetPooledTransactions(transactions),
                response,
            };

            if peer.request_tx.try_send(req).is_ok() {
                self.inflight_requests.push(GetPooledTxRequest { peer_id, response: rx })
            }
        }
    }
    // ANCHOR_END: fn-on_new_pooled_transactions

    /// Handles dedicated transaction events related tot the `eth` protocol.
    fn on_network_tx_event(&mut self, event: NetworkTransactionEvent) {
        match event {
            NetworkTransactionEvent::IncomingTransactions { peer_id, msg } => {
                self.import_transactions(peer_id, msg.0);
            }
            NetworkTransactionEvent::IncomingPooledTransactionHashes { peer_id, msg } => {
                self.on_new_pooled_transactions(peer_id, msg)
            }
            NetworkTransactionEvent::GetPooledTransactions { peer_id, request, response } => {
                self.on_get_pooled_transactions(peer_id, request, response)
            }
        }
    }

    /// Handles a command received from a detached [`TransactionsHandle`]
    fn on_command(&mut self, cmd: TransactionsCommand) {
        match cmd {
            TransactionsCommand::PropagateHash(hash) => {
                self.on_new_transactions(std::iter::once(hash))
            }
        }
    }

    /// Handles a received event related to common network events.
    // ANCHOR: fn-on_network_event
    fn on_network_event(&mut self, event: NetworkEvent) {
        match event {
            NetworkEvent::SessionClosed { peer_id, .. } => {
                // remove the peer
                self.peers.remove(&peer_id);
            }
            NetworkEvent::SessionEstablished { peer_id, messages, .. } => {
                // insert a new peer
                self.peers.insert(
                    peer_id,
                    Peer {
                        transactions: LruCache::new(
                            NonZeroUsize::new(PEER_TRANSACTION_CACHE_LIMIT).unwrap(),
                        ),
                        request_tx: messages,
                    },
                );

                // Send a `NewPooledTransactionHashes` to the peer with _all_ transactions in the
                // pool
                let msg = NewPooledTransactionHashes(self.pool.pooled_transactions());
                self.network.send_message(NetworkHandleMessage::SendPooledTransactionHashes {
                    peer_id,
                    msg,
                })
            }
            // TODO Add remaining events
            _ => {}
        }
    }
    // ANCHOR_END: fn-on_network_event

    /// Starts the import process for the given transactions.
    // ANCHOR: fn-import_transactions
    fn import_transactions(&mut self, peer_id: PeerId, transactions: Vec<TransactionSigned>) {
        let mut has_bad_transactions = false;
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            for tx in transactions {
                // recover transaction
                let tx = if let Some(tx) = tx.into_ecrecovered() {
                    tx
                } else {
                    has_bad_transactions = true;
                    continue
                };

                // track that the peer knows this transaction
                peer.transactions.insert(tx.hash);

                match self.transactions_by_peers.entry(tx.hash) {
                    Entry::Occupied(mut entry) => {
                        // transaction was already inserted
                        entry.get_mut().push(peer_id);
                    }
                    Entry::Vacant(entry) => {
                        // this is a new transaction that should be imported into the pool
                        let pool_transaction = <Pool::Transaction as FromRecoveredTransaction>::from_recovered_transaction(tx);

                        let pool = self.pool.clone();
                        let import = Box::pin(async move {
                            pool.add_external_transaction(pool_transaction).await
                        });

                        self.pool_imports.push(import);
                        entry.insert(vec![peer_id]);
                    }
                }
            }
        }

        if has_bad_transactions {
            self.report_bad_message(peer_id);
        }
    }
    // ANCHOR_END: fn-import_transactions

    fn report_bad_message(&self, peer_id: PeerId) {
        self.network.reputation_change(peer_id, ReputationChangeKind::BadTransactions);
    }

    fn on_good_import(&mut self, hash: TxHash) {
        self.transactions_by_peers.remove(&hash);
    }

    fn on_bad_import(&mut self, hash: TxHash) {
        if let Some(peers) = self.transactions_by_peers.remove(&hash) {
            for peer_id in peers {
                self.report_bad_message(peer_id);
            }
        }
    }
}

/// An endless future.
///
/// This should be spawned or used as part of `tokio::select!`.
impl<Pool> Future for TransactionsManager<Pool>
where
    Pool: TransactionPool + Unpin + 'static,
    <Pool as TransactionPool>::Transaction: IntoRecoveredTransaction,
{
    type Output = ();

    // ANCHOR: fn-poll
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // drain network/peer related events
        while let Poll::Ready(Some(event)) = this.network_events.poll_next_unpin(cx) {
            this.on_network_event(event);
        }

        // drain commands
        while let Poll::Ready(Some(cmd)) = this.command_rx.poll_next_unpin(cx) {
            this.on_command(cmd);
        }

        // drain incoming transaction events
        while let Poll::Ready(Some(event)) = this.transaction_events.poll_next_unpin(cx) {
            this.on_network_tx_event(event);
        }

        // Advance all requests.
        // We remove each request one by one and add them back.
        for idx in (0..this.inflight_requests.len()).rev() {
            let mut req = this.inflight_requests.swap_remove(idx);
            match req.response.poll_unpin(cx) {
                Poll::Pending => {
                    this.inflight_requests.push(req);
                }
                Poll::Ready(Ok(Ok(txs))) => {
                    this.import_transactions(req.peer_id, txs.0);
                }
                Poll::Ready(Ok(Err(_))) => {
                    this.report_bad_message(req.peer_id);
                }
                Poll::Ready(Err(_)) => {
                    this.report_bad_message(req.peer_id);
                }
            }
        }

        // Advance all imports
        while let Poll::Ready(Some(import_res)) = this.pool_imports.poll_next_unpin(cx) {
            match import_res {
                Ok(hash) => {
                    this.on_good_import(hash);
                }
                Err(err) => {
                    this.on_bad_import(*err.hash());
                }
            }
        }

        // handle and propagate new transactions
        let mut new_txs = Vec::new();
        while let Poll::Ready(Some(hash)) = this.pending_transactions.poll_next_unpin(cx) {
            new_txs.push(hash);
        }
        if !new_txs.is_empty() {
            this.on_new_transactions(new_txs);
        }

        // all channels are fully drained and import futures pending

        Poll::Pending
    }
    // ANCHOR_END: fn-poll
}

/// An inflight request for `PooledTransactions` from a peer
#[allow(missing_docs)]
// ANCHOR: struct-GetPooledTxRequest
struct GetPooledTxRequest {
    peer_id: PeerId,
    response: oneshot::Receiver<RequestResult<PooledTransactions>>,
}
// ANCHOR_END: struct-GetPooledTxRequest

/// Tracks a single peer
// ANCHOR: struct-Peer
struct Peer {
    /// Keeps track of transactions that we know the peer has seen.
    transactions: LruCache<H256>,
    /// A communication channel directly to the session task.
    request_tx: PeerRequestSender,
}
// ANCHOR_END: struct-Peer

/// Commands to send to the [`TransactionManager`]
// ANCHOR: enum-TransactionsCommand
enum TransactionsCommand {
    PropagateHash(H256),
}
// ANCHOR_END: enum-TransactionsCommand

/// All events related to transactions emitted by the network.
#[derive(Debug)]
#[allow(missing_docs)]
// ANCHOR: enum-NetworkTransactionEvent
pub enum NetworkTransactionEvent {
    /// Received list of transactions from the given peer.
    IncomingTransactions { peer_id: PeerId, msg: Transactions },
    /// Received list of transactions hashes to the given peer.
    IncomingPooledTransactionHashes { peer_id: PeerId, msg: NewPooledTransactionHashes },
    /// Incoming `GetPooledTransactions` request from a peer.
    GetPooledTransactions {
        peer_id: PeerId,
        request: GetPooledTransactions,
        response: oneshot::Sender<RequestResult<PooledTransactions>>,
    },
}
// ANCHOR_END: enum-NetworkTransactionEvent
