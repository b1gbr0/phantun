//! Session bookkeeping for the Phantun client.
//!
//! Every established fake-TCP session is represented by a [`Session`] record
//! carrying a generation identifier and a cancellation token. The
//! [`Connections`] table is the only place where session records are created
//! and finished, so the manager task, the main loop and every worker share a
//! single idempotent termination path.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

/// A single generation of a client session.
///
/// The record is created by [`Connections::register`] and stays usable until
/// its cancellation token is triggered by [`Connections::finish`]. The payload
/// is whatever the caller attaches to the session (the fake-TCP socket in the
/// client), which keeps the bookkeeping itself independent of the transport.
pub struct Session<T> {
    generation: u64,
    udp_remote_addr: SocketAddr,
    cancel: CancellationToken,
    end_reported: AtomicBool,
    activity: Notify,
    incoming: Notify,
    payload: T,
}

impl<T> Session<T> {
    /// Identifier of this generation, unique within the client process.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Local UDP peer this session belongs to.
    pub fn udp_remote_addr(&self) -> SocketAddr {
        self.udp_remote_addr
    }

    /// Data attached to the session when it was registered.
    pub fn payload(&self) -> &T {
        &self.payload
    }

    /// Handle that cancels this generation when triggered.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Resolves once this generation has been finished.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    /// Whether this generation has already been finished.
    pub fn is_finished(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Claims the right to report the termination of this generation.
    ///
    /// Returns `true` for the first caller only. A termination path that ends
    /// the session for a reason reports it when it wins the claim, so the
    /// reason is logged even when another path removes the table entry first.
    pub fn claim_end_report(&self) -> bool {
        !self.end_reported.swap(true, Ordering::AcqRel)
    }

    /// Signals that a payload datagram of this session moved in either
    /// direction. This keeps the idle timeout from firing but says nothing
    /// about the peer being reachable.
    pub fn notify_activity(&self) {
        self.activity.notify_one();
    }

    /// Resolves on the next activity notification of this session.
    pub async fn activity(&self) {
        self.activity.notified().await;
    }

    /// Signals that a fake-TCP packet arrived for this session.
    pub fn notify_incoming(&self) {
        self.incoming.notify_one();
    }

    /// Resolves on the next incoming fake-TCP packet of this session.
    pub async fn incoming(&self) {
        self.incoming.notified().await;
    }
}

/// Table of live sessions, keyed by the local UDP peer address.
///
/// All session termination goes through [`Connections::finish`], which cancels
/// the generation and removes the record only if it still belongs to that
/// generation. Repeated and late calls are therefore harmless.
pub struct Connections<T> {
    next_generation: AtomicU64,
    table: RwLock<HashMap<SocketAddr, Arc<Session<T>>>>,
}

impl<T> Connections<T> {
    pub fn new() -> Self {
        Self {
            next_generation: AtomicU64::new(0),
            table: RwLock::new(HashMap::new()),
        }
    }

    /// Returns the live session for `udp_remote_addr`, if any.
    ///
    /// Finished sessions are never handed out, which keeps a terminated
    /// generation from being used again.
    pub async fn get(&self, udp_remote_addr: &SocketAddr) -> Option<Arc<Session<T>>> {
        let table = self.table.read().await;
        table
            .get(udp_remote_addr)
            .filter(|session| !session.is_finished())
            .cloned()
    }

    /// Creates a new generation for `udp_remote_addr` and records it.
    pub async fn register(&self, udp_remote_addr: SocketAddr, payload: T) -> Arc<Session<T>> {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let session = Arc::new(Session {
            generation,
            udp_remote_addr,
            cancel: CancellationToken::new(),
            end_reported: AtomicBool::new(false),
            activity: Notify::new(),
            incoming: Notify::new(),
            payload,
        });

        self.table
            .write()
            .await
            .insert(udp_remote_addr, session.clone());

        session
    }

    /// Finishes `session`: cancels the generation, interrupts pending work
    /// waiting on its token and removes the record if it is still current.
    ///
    /// Returns `true` when the record of this generation was removed. The
    /// function is idempotent: finishing an already finished generation, or
    /// finishing an old generation after a new one took its place, leaves the
    /// table untouched.
    pub async fn finish(&self, session: &Session<T>) -> bool {
        session.cancel.cancel();

        let mut table = self.table.write().await;
        let is_current = table
            .get(&session.udp_remote_addr)
            .is_some_and(|current| current.generation == session.generation);
        if is_current {
            table.remove(&session.udp_remote_addr);
        }

        is_current
    }
}

/// Required by `clippy::new_without_default`: the table has no state to
/// initialise beyond what [`Connections::new`] does.
impl<T> Default for Connections<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> SocketAddr {
        "192.0.2.10:1234".parse().unwrap()
    }

    #[tokio::test]
    async fn repeated_finish_is_idempotent() {
        let connections = Connections::<()>::new();
        let session = connections.register(peer(), ()).await;

        assert!(connections.get(&peer()).await.is_some());
        assert!(connections.finish(&session).await);
        assert!(session.is_finished());
        assert!(connections.get(&peer()).await.is_none());

        assert!(!connections.finish(&session).await);
        assert!(connections.get(&peer()).await.is_none());
    }

    #[tokio::test]
    async fn late_finish_does_not_touch_new_generation() {
        let connections = Connections::<()>::new();
        let old = connections.register(peer(), ()).await;
        assert!(connections.finish(&old).await);

        let new = connections.register(peer(), ()).await;
        assert_ne!(old.generation(), new.generation());

        assert!(!connections.finish(&old).await);

        let current = connections.get(&peer()).await.expect("new session is live");
        assert_eq!(current.generation(), new.generation());
        assert!(!current.is_finished());

        assert!(connections.finish(&current).await);
        assert!(connections.get(&peer()).await.is_none());
    }

    #[tokio::test]
    async fn end_report_is_claimed_only_once() {
        let connections = Connections::<()>::new();
        let session = connections.register(peer(), ()).await;

        // The first path that knows a reason claims the report; a later
        // termination attempt must not write a second record.
        assert!(session.claim_end_report());
        assert!(!session.claim_end_report());

        assert!(connections.finish(&session).await);
    }

    #[tokio::test]
    async fn end_report_claim_survives_a_competing_finish() {
        let connections = Connections::<()>::new();
        let session = connections.register(peer(), ()).await;

        // The path that knows the reason claims the report, but a racing path
        // observes the cancellation and removes the record first.
        assert!(session.claim_end_report());
        session.cancel_token().cancel();
        assert!(connections.finish(&session).await);

        // The reason is still nobody else's to report.
        assert!(!session.claim_end_report());
    }

    #[tokio::test]
    async fn get_hides_a_cancelled_session_before_it_is_finished() {
        let connections = Connections::<()>::new();
        let session = connections.register(peer(), ()).await;

        // `finish` cancels before it takes the table lock, so `get` can run
        // against a cancelled record that is still present.
        session.cancel_token().cancel();
        assert!(session.is_finished());
        assert!(connections.get(&peer()).await.is_none());

        assert!(connections.finish(&session).await);
        assert!(connections.get(&peer()).await.is_none());
    }
}
