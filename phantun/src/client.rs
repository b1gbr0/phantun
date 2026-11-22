//! Session handling of the Phantun client.
//!
//! The loops are generic over the I/O they use, so their behaviour can be
//! exercised without a TUN device or real sockets: the binary plugs in
//! [`fake_tcp::Stack`], [`tokio::net::UdpSocket`] and [`udp_recv_pktinfo`],
//! the tests plug in fakes with controlled send and receive results.
//!
//! Every send and receive is anchored to the session generation it serves.
//! A failed operation finishes that generation through
//! [`Connections::finish`], and finishing a generation interrupts the sends
//! that are still pending instead of letting them linger.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use fake_tcp::packet::MAX_PACKET_LEN;
use fake_tcp::{Socket, Stack};
use log::{debug, error, info, trace};
use tokio::net::UdpSocket;
use tokio::time::{self, Instant};

use crate::UDP_TTL;
use crate::keepalive::KeepaliveTimer;
use crate::session::{Connections, Session};
use crate::utils::{try_new_udp_reuseport, udp_recv_pktinfo};

/// Upper bound on how long a keepalive probe may stay in flight. A probe still
/// pending after this is treated as a failed send and the generation is
/// finished, so a stalled TUN write cannot pin a session in place.
pub const KEEPALIVE_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Receives datagrams addressed to the listening UDP socket of the client,
/// together with the address of the local peer and the destination address.
pub trait LocalUdpIngress: Send + Sync + 'static {
    fn recv(
        &self,
        buf: &mut [u8],
    ) -> impl Future<Output = io::Result<(usize, SocketAddr, IpAddr)>> + Send;
}

impl LocalUdpIngress for Arc<UdpSocket> {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr, IpAddr)> {
        udp_recv_pktinfo(self, buf).await
    }
}

/// Connected UDP socket of a fast-path worker, talking to the local peer.
pub trait UdpPeer: Send + Sync + 'static {
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send;
    fn send(&self, buf: &[u8]) -> impl Future<Output = io::Result<usize>> + Send;
}

impl UdpPeer for UdpSocket {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        UdpSocket::recv(self, buf).await
    }

    async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        UdpSocket::send(self, buf).await
    }
}

/// Established fake-TCP connection a session sends through.
pub trait FakeTcpSocket: Send + Sync + 'static {
    fn send(&self, payload: &[u8]) -> impl Future<Output = Option<()>> + Send;
    fn send_keepalive(&self) -> impl Future<Output = Option<()>> + Send;
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = Option<usize>> + Send;
}

impl FakeTcpSocket for Socket {
    async fn send(&self, payload: &[u8]) -> Option<()> {
        Socket::send(self, payload).await
    }

    async fn send_keepalive(&self) -> Option<()> {
        Socket::send_keepalive(self).await
    }

    async fn recv(&self, buf: &mut [u8]) -> Option<usize> {
        Socket::recv(self, buf).await
    }
}

/// Opens the fake-TCP connections of new sessions.
pub trait FakeTcpConnector: Send + 'static {
    type Socket: FakeTcpSocket;

    fn connect(&mut self, remote: SocketAddr) -> impl Future<Output = Option<Self::Socket>> + Send;
}

impl FakeTcpConnector for Stack {
    type Socket = Socket;

    async fn connect(&mut self, remote: SocketAddr) -> Option<Socket> {
        Stack::connect(self, remote).await
    }
}

/// Opens one connected UDP socket per fast-path worker.
pub trait PeerFactory: Send + Sync + 'static {
    type Peer: UdpPeer;

    fn create(
        &self,
        udp_remote_addr: SocketAddr,
        udp_local_addr: IpAddr,
    ) -> impl Future<Output = io::Result<Self::Peer>> + Send;
}

/// Opens the fast-path socket bound to the address the peer used to reach us.
pub struct BindPeerFactory {
    local_port: u16,
}

impl BindPeerFactory {
    pub fn new(local_port: u16) -> Self {
        Self { local_port }
    }
}

impl PeerFactory for BindPeerFactory {
    type Peer = UdpSocket;

    async fn create(
        &self,
        udp_remote_addr: SocketAddr,
        udp_local_addr: IpAddr,
    ) -> io::Result<UdpSocket> {
        let bind_addr = local_bind_addr(udp_remote_addr, udp_local_addr, self.local_port)?;

        let udp_sock = try_new_udp_reuseport(bind_addr)?;
        udp_sock.connect(udp_remote_addr).await?;

        Ok(udp_sock)
    }
}

/// Bind address of a fast-path socket: the address the peer used to reach us.
///
/// Always reply from the same address that the peer used to communicate with
/// us. This avoids a frequent problem with IPv6 privacy extensions when we
/// erroneously bind to wrong short-lived temporary address even if the peer
/// explicitly used a persistent address to communicate to us.
///
/// To do so, first bind to (`incoming packet dst_ip`, `local addr port`), and
/// then connect to (`incoming packet src_ip`, `incoming packet src_port`).
fn local_bind_addr(
    udp_remote_addr: SocketAddr,
    udp_local_addr: IpAddr,
    local_port: u16,
) -> io::Result<SocketAddr> {
    match (udp_remote_addr, udp_local_addr) {
        (SocketAddr::V4(_), IpAddr::V4(udp_local_ipv4)) => Ok(SocketAddr::V4(SocketAddrV4::new(
            udp_local_ipv4,
            local_port,
        ))),
        (SocketAddr::V6(udp_remote_addr), IpAddr::V6(udp_local_ipv6)) => {
            Ok(SocketAddr::V6(SocketAddrV6::new(
                udp_local_ipv6,
                local_port,
                udp_remote_addr.flowinfo(),
                udp_remote_addr.scope_id(),
            )))
        }
        (_, _) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unexpected family combination for udp_remote_addr={udp_remote_addr} and udp_local_addr={udp_local_addr}"
            ),
        )),
    }
}

/// Runtime parameters of the client.
pub struct ClientConfig {
    /// Address of the Phantun server.
    pub remote_addr: SocketAddr,
    /// Optional content sent right after the handshake.
    pub handshake_packet: Option<Vec<u8>>,
    /// Number of fast-path workers per session.
    pub num_workers: usize,
    /// Keepalive settings; `None` disables probes entirely.
    pub keepalive: Option<KeepaliveConfig>,
}

/// Keepalive settings of the client.
#[derive(Clone, Copy)]
pub struct KeepaliveConfig {
    /// Time between the last incoming packet and the first probe.
    pub time: Duration,
    /// Interval between probes.
    pub interval: Duration,
    /// Number of probes before the session is given up.
    pub retries: u32,
}

/// Outcome of a payload send.
enum PayloadSend {
    Sent,
    /// The generation was finished while the send was pending.
    Cancelled,
    /// The TUN write failed.
    Failed,
}

/// Why a session generation is being given up.
///
/// Every termination path names its reason, so the log records of different
/// failures stay distinguishable.
enum SessionEnd {
    /// The configured number of keepalive probes went unanswered.
    KeepaliveExhausted,
    /// No payload traffic moved for [`UDP_TTL`].
    IdleTimeout,
    /// A write to the TUN device failed; the field says what was written.
    TunWriteFailed(&'static str),
    /// A TUN write stayed pending longer than [`KEEPALIVE_SEND_TIMEOUT`].
    SendTimedOut,
    /// Receiving from the local UDP peer failed.
    UdpRecvFailed(io::Error),
    /// The fake-TCP receive side was closed by the peer.
    FakeTcpRecvClosed,
}

impl SessionEnd {
    fn level(&self) -> log::Level {
        match self {
            Self::KeepaliveExhausted | Self::IdleTimeout => log::Level::Info,
            Self::TunWriteFailed(_) | Self::UdpRecvFailed(_) => log::Level::Error,
            Self::SendTimedOut => log::Level::Warn,
            Self::FakeTcpRecvClosed => log::Level::Debug,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::KeepaliveExhausted => "keepalive probes exhausted".to_owned(),
            Self::IdleTimeout => format!("no traffic seen in the last {UDP_TTL:?}"),
            Self::TunWriteFailed(what) => format!("unable to write {what} to the TUN device"),
            Self::SendTimedOut => {
                format!("TUN write still pending after {KEEPALIVE_SEND_TIMEOUT:?}")
            }
            Self::UdpRecvFailed(error) => format!("unable to receive local datagram: {error}"),
            Self::FakeTcpRecvClosed => "fake-TCP receive closed".to_owned(),
        }
    }
}

/// Finishes `session` with one log record naming the reason, the session
/// generation and the local UDP peer.
///
/// The record is written by the first termination path that has a reason, not
/// by the path that removed the table entry: another path can observe the
/// cancellation and remove the record before this one takes the lock, and the
/// reason must survive that. Concurrent termination attempts still produce a
/// single record.
async fn end_session<S: FakeTcpSocket>(
    connections: &Connections<Arc<S>>,
    session: &Session<Arc<S>>,
    reason: SessionEnd,
) {
    let report = session.claim_end_report();
    connections.finish(session).await;
    if report {
        log::log!(
            reason.level(),
            "Session {} for local UDP peer {}: {}, closing session",
            session.generation(),
            session.udp_remote_addr(),
            reason.describe()
        );
    }
}

/// Sends a datagram of `session`, interrupted as soon as the generation is
/// finished.
async fn send_payload<S: FakeTcpSocket>(session: &Session<Arc<S>>, payload: &[u8]) -> PayloadSend {
    tokio::select! {
        _ = session.cancelled() => PayloadSend::Cancelled,
        res = session.payload().send(payload) => match res {
            Some(()) => PayloadSend::Sent,
            None => PayloadSend::Failed,
        },
    }
}

/// Sends one of the first datagrams of a new generation.
///
/// The liveness manager, which is the only other path that could finish the
/// generation, is spawned after these sends, so they are bounded explicitly:
/// a TUN write stuck on the handshake or the first datagram must not pin the
/// main loop.
async fn send_initial<S: FakeTcpSocket>(
    session: &Session<Arc<S>>,
    payload: &[u8],
) -> Option<PayloadSend> {
    time::timeout(KEEPALIVE_SEND_TIMEOUT, send_payload(session, payload))
        .await
        .ok()
}

async fn send_keepalive<S: FakeTcpSocket>(session: &Session<Arc<S>>) -> Option<()> {
    session.payload().send_keepalive().await
}

/// A keepalive probe that is in flight, with its own send timeout.
struct PendingProbe<'a> {
    send: Pin<Box<dyn Future<Output = Option<()>> + Send + 'a>>,
    send_deadline: Instant,
    probe_deadline: Instant,
}

/// Runs the client until the listening socket fails.
///
/// New local datagrams create a session; every session gets one manager task
/// and `config.num_workers` fast-path workers, all of which end through
/// [`Connections::finish`].
pub async fn run<C, I, F>(
    mut connector: C,
    ingress: I,
    peers: F,
    config: ClientConfig,
    connections: Arc<Connections<Arc<C::Socket>>>,
) -> io::Result<()>
where
    C: FakeTcpConnector,
    I: LocalUdpIngress,
    F: PeerFactory,
{
    let mut buf_r = [0u8; MAX_PACKET_LEN];

    loop {
        let (size, udp_remote_addr, udp_local_addr) = ingress.recv(&mut buf_r).await?;

        // seen UDP packet to listening socket, this means:
        // 1. It is a new UDP connection, or
        // 2. It is some extra packets not filtered by more specific
        //    connected UDP socket yet
        if let Some(session) = connections.get(&udp_remote_addr).await {
            match send_payload(&session, &buf_r[..size]).await {
                PayloadSend::Sent => session.notify_activity(),
                PayloadSend::Cancelled => {
                    // A concurrent path finished this generation; make sure the
                    // table no longer points at it and keep serving the socket.
                    connections.finish(&session).await;
                }
                PayloadSend::Failed => {
                    end_session(
                        &connections,
                        &session,
                        SessionEnd::TunWriteFailed("datagram"),
                    )
                    .await;
                }
            }
            // The generation reference is dropped here, before the next receive
            // awaits, so a finished session never delays a new one.
            continue;
        }

        info!("New UDP client from {}", udp_remote_addr);
        let Some(sock) = connector.connect(config.remote_addr).await else {
            error!("Unable to connect to remote {}", config.remote_addr);
            continue;
        };

        // Initial anchor of the liveness schedule: the handshake just
        // completed, so the last incoming fake-TCP packet is the SYN+ACK.
        let established_at = Instant::now();
        let session = connections.register(udp_remote_addr, Arc::new(sock)).await;

        if let Some(ref packet) = config.handshake_packet {
            let Some(outcome) = send_initial(&session, packet).await else {
                end_session(&connections, &session, SessionEnd::SendTimedOut).await;
                continue;
            };
            match outcome {
                PayloadSend::Sent => {
                    debug!("Sent handshake packet for session {}", session.generation())
                }
                PayloadSend::Cancelled => {
                    connections.finish(&session).await;
                    continue;
                }
                PayloadSend::Failed => {
                    end_session(
                        &connections,
                        &session,
                        SessionEnd::TunWriteFailed("handshake packet"),
                    )
                    .await;
                    continue;
                }
            }
        }

        // send first packet
        let Some(outcome) = send_initial(&session, &buf_r[..size]).await else {
            end_session(&connections, &session, SessionEnd::SendTimedOut).await;
            continue;
        };
        match outcome {
            PayloadSend::Sent => {
                session.notify_activity();
                debug!("inserted fake TCP socket into connection table");
            }
            PayloadSend::Cancelled => {
                connections.finish(&session).await;
                continue;
            }
            PayloadSend::Failed => {
                end_session(
                    &connections,
                    &session,
                    SessionEnd::TunWriteFailed("first datagram"),
                )
                .await;
                continue;
            }
        }

        // Spawn "fastpath" UDP sockets and tasks, this offloads the main task
        // from forwarding UDP packets.
        let mut worker_peers = Vec::with_capacity(config.num_workers);
        let mut failed = false;
        for _ in 0..config.num_workers {
            match peers.create(udp_remote_addr, udp_local_addr).await {
                Ok(peer) => worker_peers.push(peer),
                Err(e) => {
                    error!(
                        "Session {} for local UDP peer {}: unable to create fast-path socket: {}",
                        session.generation(),
                        session.udp_remote_addr(),
                        e
                    );
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            connections.finish(&session).await;
            continue;
        }

        for (index, peer) in worker_peers.into_iter().enumerate() {
            tokio::spawn(run_worker(
                index,
                session.clone(),
                connections.clone(),
                peer,
            ));
        }

        tokio::spawn(run_manager(
            session.clone(),
            connections.clone(),
            config.keepalive,
            established_at,
        ));
    }
}

/// Fast-path worker: forwards datagrams in both directions for one session.
async fn run_worker<S, P>(
    index: usize,
    session: Arc<Session<Arc<S>>>,
    connections: Arc<Connections<Arc<S>>>,
    peer: P,
) where
    S: FakeTcpSocket,
    P: UdpPeer,
{
    let mut buf_udp = [0u8; MAX_PACKET_LEN];
    let mut buf_tcp = [0u8; MAX_PACKET_LEN];

    loop {
        tokio::select! {
            res = peer.recv(&mut buf_udp) => {
                match res {
                    Ok(size) => {
                        match send_payload(&session, &buf_udp[..size]).await {
                            PayloadSend::Sent => session.notify_activity(),
                            PayloadSend::Cancelled => {
                                debug!("worker {} of session {} terminated", index, session.generation());
                                connections.finish(&session).await;
                                return;
                            }
                            PayloadSend::Failed => {
                                end_session(
                                    &connections,
                                    &session,
                                    SessionEnd::TunWriteFailed("datagram"),
                                )
                                .await;
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        end_session(&connections, &session, SessionEnd::UdpRecvFailed(e)).await;
                        return;
                    }
                }
            }
            res = session.payload().recv(&mut buf_tcp) => {
                match res {
                    Some(size) if size > 0 => {
                        let sent = peer.send(&buf_tcp[..size]);
                        tokio::select! {
                            _ = session.cancelled() => {
                                connections.finish(&session).await;
                                return;
                            }
                            res = sent => match res {
                                Ok(_) => session.notify_activity(),
                                Err(e) => {
                                    error!(
                                        "Session {} for local UDP peer {}: unable to send datagram to the local peer: {}",
                                        session.generation(),
                                        session.udp_remote_addr(),
                                        e
                                    );
                                    connections.finish(&session).await;
                                    return;
                                }
                            }
                        }
                        session.notify_incoming();
                    }
                    Some(_) => {
                        trace!("Empty TCP packet received");
                        session.notify_incoming();
                    }
                    None => {
                        end_session(&connections, &session, SessionEnd::FakeTcpRecvClosed).await;
                        return;
                    }
                }
            }
            _ = session.cancelled() => {
                debug!("worker {} terminated", index);
                return;
            }
        }
    }
}

/// Liveness manager of one session: keepalive probes and the idle timeout.
///
/// A probe is awaited together with everything else the manager watches, so a
/// pending keepalive send never blocks incoming activity, cancellation, the
/// idle deadline or the probe exhaustion deadline. A probe that does not
/// complete within [`KEEPALIVE_SEND_TIMEOUT`] finishes the generation.
async fn run_manager<S: FakeTcpSocket>(
    session: Arc<Session<Arc<S>>>,
    connections: Arc<Connections<Arc<S>>>,
    keepalive_config: Option<KeepaliveConfig>,
    established_at: Instant,
) {
    let mut last_data_recv = Instant::now();
    let mut keepalive = keepalive_config.map(|config| {
        KeepaliveTimer::new(established_at, config.time, config.interval, config.retries)
    });
    let mut pending: Option<PendingProbe<'_>> = None;

    let reason = loop {
        if let Some(probe) = pending.take() {
            let PendingProbe {
                mut send,
                send_deadline,
                probe_deadline,
            } = probe;
            let idle_deadline = last_data_recv + UDP_TTL;
            // A probe in flight does not postpone the exhaustion deadline: if
            // the retry budget runs out while the send is still pending, the
            // session is given up then, not when the send times out.
            let exhaustion_deadline = keepalive
                .as_ref()
                .map(KeepaliveTimer::exhaustion_deadline);
            let mut requeue = false;

            tokio::select! {
                res = send.as_mut() => {
                    match res {
                        Some(()) => {
                            // The probe belongs to the budget it was started
                            // for; an incoming packet meanwhile restarted it.
                            if let Some(timer) = keepalive.as_mut()
                                && timer.next_deadline() == probe_deadline
                            {
                                timer.probe_sent();
                            }
                        }
                        None => break Some(SessionEnd::TunWriteFailed("keepalive probe")),
                    }
                }
                _ = time::sleep_until(send_deadline) => {
                    break Some(SessionEnd::SendTimedOut);
                }
                _ = time::sleep_until(exhaustion_deadline.unwrap_or(idle_deadline)),
                    if exhaustion_deadline.is_some() =>
                {
                    break Some(SessionEnd::KeepaliveExhausted);
                }
                _ = time::sleep_until(idle_deadline) => {
                    break Some(SessionEnd::IdleTimeout);
                }
                _ = session.cancelled() => break None,
                _ = session.activity() => {
                    last_data_recv = Instant::now();
                    requeue = true;
                }
                _ = session.incoming() => {
                    if let Some(timer) = keepalive.as_mut() {
                        timer.on_incoming(Instant::now());
                    }
                    requeue = true;
                }
            }

            if requeue {
                pending = Some(PendingProbe {
                    send,
                    send_deadline,
                    probe_deadline,
                });
            }
            continue;
        }

        let idle_deadline = last_data_recv + UDP_TTL;
        let probe_deadline = keepalive.as_ref().map(KeepaliveTimer::next_deadline);
        let deadline = probe_deadline.map_or(idle_deadline, |p| p.min(idle_deadline));

        tokio::select! {
            _ = time::sleep_until(deadline) => {
                let now = Instant::now();

                if let Some(timer) = keepalive.as_mut()
                    && now >= timer.next_deadline()
                {
                    if timer.probes_exhausted() {
                        break Some(SessionEnd::KeepaliveExhausted);
                    }

                    let retries = keepalive_config.map_or(0, |config| config.retries);
                    trace!(
                        "Session {} sending keepalive {} / {}",
                        session.generation(),
                        timer.probes_sent() + 1,
                        retries
                    );

                    let probe_deadline = timer.next_deadline();
                    pending = Some(PendingProbe {
                        send: Box::pin(send_keepalive(&session)),
                        send_deadline: now + KEEPALIVE_SEND_TIMEOUT,
                        probe_deadline,
                    });
                }

                if now >= idle_deadline {
                    break Some(SessionEnd::IdleTimeout);
                }
            }
            _ = session.cancelled() => break None,
            _ = session.activity() => {
                last_data_recv = Instant::now();
            }
            _ = session.incoming() => {
                if let Some(timer) = keepalive.as_mut() {
                    timer.on_incoming(Instant::now());
                }
            }
        }
    };

    debug!(
        "Session {} for local UDP peer {}: manager stopped",
        session.generation(),
        session.udp_remote_addr()
    );
    if let Some(reason) = reason {
        end_session(&connections, &session, reason).await;
    } else {
        connections.finish(&session).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};

    fn udp_peer() -> SocketAddr {
        "192.0.2.10:1234".parse().unwrap()
    }

    fn udp_local() -> IpAddr {
        "192.0.2.1".parse().unwrap()
    }

    fn remote() -> SocketAddr {
        "198.51.100.1:8080".parse().unwrap()
    }

    struct FakeIngress {
        rx: AsyncMutex<mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr, IpAddr)>>,
    }

    impl LocalUdpIngress for FakeIngress {
        async fn recv(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr, IpAddr)> {
            match self.rx.lock().await.recv().await {
                Some((payload, peer, local)) => {
                    let size = payload.len().min(buf.len());
                    buf[..size].copy_from_slice(&payload[..size]);
                    Ok((size, peer, local))
                }
                None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed")),
            }
        }
    }

    /// How the fake fake-TCP socket answers a send.
    #[derive(Clone, Copy)]
    enum SendBehavior {
        Ok,
        /// The send never completes.
        Hang,
        /// The send stays pending until the test opens the `released` gate,
        /// then stays released for every later send.
        Hold,
        /// The TUN write fails.
        Fail,
    }

    struct FakeSocket {
        send_behavior: SendBehavior,
        send_started: Arc<Notify>,
        /// Virtual times at which keepalive probes were sent.
        probes: Arc<StdMutex<Vec<Instant>>>,
        /// Payloads the session wrote to the fake-TCP connection.
        sent: Arc<StdMutex<Vec<Vec<u8>>>>,
        /// Whether a closed test channel means "peer closed the connection"
        /// instead of "stay pending".
        close_recv_on_empty: bool,
        /// Gate that releases a [`SendBehavior::Hold`] send.
        release: Arc<Notify>,
        released: Arc<AtomicBool>,
        recv_rx: AsyncMutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    }

    impl FakeTcpSocket for FakeSocket {
        async fn send(&self, payload: &[u8]) -> Option<()> {
            match self.send_behavior {
                SendBehavior::Ok => {}
                SendBehavior::Hang => {
                    self.send_started.notify_one();
                    std::future::pending().await
                }
                SendBehavior::Hold => {
                    while !self.released.load(Ordering::SeqCst) {
                        let notified = self.release.notified();
                        if self.released.load(Ordering::SeqCst) {
                            break;
                        }
                        notified.await;
                    }
                }
                SendBehavior::Fail => return None,
            }
            self.sent.lock().unwrap().push(payload.to_vec());
            Some(())
        }

        async fn send_keepalive(&self) -> Option<()> {
            self.probes.lock().unwrap().push(Instant::now());
            self.send(&[]).await
        }

        async fn recv(&self, buf: &mut [u8]) -> Option<usize> {
            match self.recv_rx.lock().await.recv().await {
                Some(payload) => {
                    let size = payload.len().min(buf.len());
                    buf[..size].copy_from_slice(&payload[..size]);
                    Some(size)
                }
                None => {
                    if self.close_recv_on_empty {
                        None
                    } else {
                        std::future::pending().await
                    }
                }
            }
        }
    }

    struct FakeConnector {
        connects: Arc<AtomicUsize>,
        send_behavior: SendBehavior,
        /// Makes the socket of the first connection fail its sends.
        first_socket_fails: bool,
        send_started: Arc<Notify>,
        /// Payloads written by every socket this connector produced.
        sent: Arc<StdMutex<Vec<Vec<u8>>>>,
    }

    impl FakeTcpConnector for FakeConnector {
        type Socket = FakeSocket;

        async fn connect(&mut self, _remote: SocketAddr) -> Option<FakeSocket> {
            let index = self.connects.fetch_add(1, Ordering::SeqCst);
            let send_behavior = if self.first_socket_fails && index == 0 {
                SendBehavior::Fail
            } else {
                self.send_behavior
            };
            let (_tx, rx) = mpsc::unbounded_channel();
            Some(FakeSocket {
                send_behavior,
                send_started: self.send_started.clone(),
                probes: Arc::new(StdMutex::new(Vec::new())),
                sent: self.sent.clone(),
                close_recv_on_empty: false,
                release: Arc::new(Notify::new()),
                released: Arc::new(AtomicBool::new(false)),
                recv_rx: AsyncMutex::new(rx),
            })
        }
    }

    struct FakePeer {
        recv_rx: AsyncMutex<mpsc::UnboundedReceiver<io::Result<Vec<u8>>>>,
        send_hangs: bool,
        send_fails: bool,
        send_started: Arc<Notify>,
        dropped: Arc<AtomicUsize>,
        /// Datagrams written to the local UDP peer.
        sent: Arc<StdMutex<Vec<Vec<u8>>>>,
    }

    impl Drop for FakePeer {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl UdpPeer for FakePeer {
        async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
            match self.recv_rx.lock().await.recv().await {
                Some(Ok(payload)) => {
                    let size = payload.len().min(buf.len());
                    buf[..size].copy_from_slice(&payload[..size]);
                    Ok(size)
                }
                Some(Err(e)) => Err(e),
                None => std::future::pending().await,
            }
        }

        async fn send(&self, buf: &[u8]) -> io::Result<usize> {
            if self.send_hangs {
                self.send_started.notify_one();
                return std::future::pending().await;
            }
            if self.send_fails {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "boom"));
            }
            self.sent.lock().unwrap().push(buf.to_vec());
            Ok(buf.len())
        }
    }

    struct FakePeerFactory {
        dropped: Arc<AtomicUsize>,
        created: Arc<AtomicUsize>,
        create_fails: bool,
    }

    impl PeerFactory for FakePeerFactory {
        type Peer = FakePeer;

        async fn create(&self, _remote: SocketAddr, _local: IpAddr) -> io::Result<FakePeer> {
            self.created.fetch_add(1, Ordering::SeqCst);
            if self.create_fails {
                return Err(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "cannot bind",
                ));
            }
            let (_tx, rx) = mpsc::unbounded_channel();
            Ok(FakePeer {
                recv_rx: AsyncMutex::new(rx),
                send_hangs: false,
                send_fails: false,
                send_started: Arc::new(Notify::new()),
                dropped: self.dropped.clone(),
                sent: Arc::new(StdMutex::new(Vec::new())),
            })
        }
    }

    fn fake_socket(
        send_behavior: SendBehavior,
        send_started: Arc<Notify>,
    ) -> (FakeSocket, mpsc::UnboundedSender<Vec<u8>>) {
        fake_socket_with(send_behavior, send_started, false)
    }

    fn fake_socket_with(
        send_behavior: SendBehavior,
        send_started: Arc<Notify>,
        close_recv_on_empty: bool,
    ) -> (FakeSocket, mpsc::UnboundedSender<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            FakeSocket {
                send_behavior,
                send_started,
                probes: Arc::new(StdMutex::new(Vec::new())),
                sent: Arc::new(StdMutex::new(Vec::new())),
                close_recv_on_empty,
                release: Arc::new(Notify::new()),
                released: Arc::new(AtomicBool::new(false)),
                recv_rx: AsyncMutex::new(rx),
            },
            tx,
        )
    }

    fn keepalive_20_5_3() -> KeepaliveConfig {
        KeepaliveConfig {
            time: Duration::from_secs(20),
            interval: Duration::from_secs(5),
            retries: 3,
        }
    }

    /// Virtual seconds at which keepalive probes were sent, relative to `t0`.
    fn probe_offsets(probes: &StdMutex<Vec<Instant>>, t0: Instant) -> Vec<u64> {
        probes
            .lock()
            .unwrap()
            .iter()
            .map(|probe| (*probe - t0).as_secs())
            .collect()
    }

    fn fake_peer(
        send_hangs: bool,
        send_started: Arc<Notify>,
        dropped: Arc<AtomicUsize>,
    ) -> (FakePeer, mpsc::UnboundedSender<io::Result<Vec<u8>>>) {
        let (peer, tx, _sent) = fake_peer_full(send_hangs, false, send_started, dropped);
        (peer, tx)
    }

    #[allow(clippy::type_complexity)]
    fn fake_peer_full(
        send_hangs: bool,
        send_fails: bool,
        send_started: Arc<Notify>,
        dropped: Arc<AtomicUsize>,
    ) -> (
        FakePeer,
        mpsc::UnboundedSender<io::Result<Vec<u8>>>,
        Arc<StdMutex<Vec<Vec<u8>>>>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let sent = Arc::new(StdMutex::new(Vec::new()));
        (
            FakePeer {
                recv_rx: AsyncMutex::new(rx),
                send_hangs,
                send_fails,
                send_started,
                dropped,
                sent: sent.clone(),
            },
            tx,
            sent,
        )
    }

    async fn wait_for_session<S>(
        connections: &Connections<Arc<S>>,
        peer: &SocketAddr,
    ) -> Arc<Session<Arc<S>>> {
        for _ in 0..10_000 {
            if let Some(session) = connections.get(peer).await {
                return session;
            }
            tokio::task::yield_now().await;
        }
        panic!("session for {peer} was not registered");
    }

    #[tokio::test]
    async fn main_loop_interrupts_a_pending_send_and_starts_the_next_generation() {
        let peer = udp_peer();
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel();
        let ingress = FakeIngress {
            rx: AsyncMutex::new(ingress_rx),
        };
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let connects = Arc::new(AtomicUsize::new(0));
        let send_started = Arc::new(Notify::new());
        let connector = FakeConnector {
            connects: connects.clone(),
            send_behavior: SendBehavior::Hang,
            first_socket_fails: false,
            send_started: send_started.clone(),
            sent: Arc::new(StdMutex::new(Vec::new())),
        };
        let peers = FakePeerFactory {
            dropped: Arc::new(AtomicUsize::new(0)),
            created: Arc::new(AtomicUsize::new(0)),
            create_fails: false,
        };
        let config = ClientConfig {
            remote_addr: remote(),
            handshake_packet: None,
            num_workers: 1,
            keepalive: None,
        };

        let main_loop = tokio::spawn(run(connector, ingress, peers, config, connections.clone()));

        ingress_tx.send((vec![1u8; 8], peer, udp_local())).unwrap();
        let first = wait_for_session(&connections, &peer).await;
        send_started.notified().await;
        assert!(connections.finish(&first).await);

        // The main loop drops the old generation and keeps serving the socket.
        ingress_tx.send((vec![2u8; 8], peer, udp_local())).unwrap();
        let second = wait_for_session(&connections, &peer).await;
        assert_ne!(first.generation(), second.generation());
        assert_eq!(connects.load(Ordering::SeqCst), 2);

        // A late finish of the old generation does not touch the new one.
        assert!(!connections.finish(&first).await);
        assert_eq!(
            connections
                .get(&peer)
                .await
                .expect("new generation is live")
                .generation(),
            second.generation()
        );

        main_loop.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn main_loop_finishes_the_session_when_the_first_datagram_send_stalls() {
        let peer = udp_peer();
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel();
        let ingress = FakeIngress {
            rx: AsyncMutex::new(ingress_rx),
        };
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let send_started = Arc::new(Notify::new());
        let connector = FakeConnector {
            connects: Arc::new(AtomicUsize::new(0)),
            send_behavior: SendBehavior::Hang,
            first_socket_fails: false,
            send_started: send_started.clone(),
            sent: Arc::new(StdMutex::new(Vec::new())),
        };
        let peers = FakePeerFactory {
            dropped: Arc::new(AtomicUsize::new(0)),
            created: Arc::new(AtomicUsize::new(0)),
            create_fails: false,
        };
        let config = ClientConfig {
            remote_addr: remote(),
            handshake_packet: None,
            num_workers: 1,
            keepalive: None,
        };

        let main_loop = tokio::spawn(run(connector, ingress, peers, config, connections.clone()));

        ingress_tx.send((vec![1u8; 8], peer, udp_local())).unwrap();
        let session = wait_for_session(&connections, &peer).await;
        send_started.notified().await;

        // The TUN write never completes and no manager exists yet to cancel
        // it; the bounded initial send gives up on its own.
        time::sleep(KEEPALIVE_SEND_TIMEOUT + Duration::from_secs(1)).await;
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());

        main_loop.abort();
    }

    #[tokio::test]
    async fn worker_interrupts_a_pending_fake_tcp_send() {
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let send_started = Arc::new(Notify::new());
        let (socket, _tcp_tx) = fake_socket(SendBehavior::Hang, send_started.clone());
        let session = connections.register(peer, Arc::new(socket)).await;

        let dropped = Arc::new(AtomicUsize::new(0));
        let (worker_peer, udp_tx) = fake_peer(false, Arc::new(Notify::new()), dropped.clone());
        let worker = tokio::spawn(run_worker(
            0,
            session.clone(),
            connections.clone(),
            worker_peer,
        ));

        udp_tx.send(Ok(vec![7u8; 4])).unwrap();
        send_started.notified().await;
        assert!(connections.finish(&session).await);

        worker.await.unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(connections.get(&peer).await.is_none());
    }

    #[tokio::test]
    async fn worker_interrupts_a_pending_udp_send() {
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let send_started = Arc::new(Notify::new());
        let (socket, tcp_tx) = fake_socket(SendBehavior::Ok, Arc::new(Notify::new()));
        let session = connections.register(peer, Arc::new(socket)).await;

        let dropped = Arc::new(AtomicUsize::new(0));
        let (worker_peer, _udp_tx) = fake_peer(true, send_started.clone(), dropped.clone());
        let worker = tokio::spawn(run_worker(
            0,
            session.clone(),
            connections.clone(),
            worker_peer,
        ));

        tcp_tx.send(vec![9u8; 16]).unwrap();
        send_started.notified().await;
        assert!(connections.finish(&session).await);

        worker.await.unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn worker_finishes_the_session_on_udp_recv_error() {
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let (socket, _tcp_tx) = fake_socket(SendBehavior::Ok, Arc::new(Notify::new()));
        let session = connections.register(peer, Arc::new(socket)).await;

        let (worker_peer, udp_tx) = fake_peer(
            false,
            Arc::new(Notify::new()),
            Arc::new(AtomicUsize::new(0)),
        );
        let worker = tokio::spawn(run_worker(
            0,
            session.clone(),
            connections.clone(),
            worker_peer,
        ));

        udp_tx
            .send(Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "boom",
            )))
            .unwrap();

        worker.await.unwrap();
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());
    }

    async fn wait_for_generation<S>(
        connections: &Connections<Arc<S>>,
        peer: &SocketAddr,
        generation: u64,
    ) -> Arc<Session<Arc<S>>> {
        for _ in 0..10_000 {
            if let Some(session) = connections.get(peer).await
                && session.generation() == generation
            {
                return session;
            }
            tokio::task::yield_now().await;
        }
        panic!("generation {generation} for {peer} was not registered");
    }

    #[tokio::test]
    async fn main_loop_finishes_the_session_on_a_tun_write_failure() {
        let peer = udp_peer();
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel();
        let ingress = FakeIngress {
            rx: AsyncMutex::new(ingress_rx),
        };
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let connects = Arc::new(AtomicUsize::new(0));
        let connector = FakeConnector {
            connects: connects.clone(),
            send_behavior: SendBehavior::Ok,
            first_socket_fails: true,
            send_started: Arc::new(Notify::new()),
            sent: Arc::new(StdMutex::new(Vec::new())),
        };
        let peers = FakePeerFactory {
            dropped: Arc::new(AtomicUsize::new(0)),
            created: Arc::new(AtomicUsize::new(0)),
            create_fails: false,
        };
        let config = ClientConfig {
            remote_addr: remote(),
            handshake_packet: None,
            num_workers: 1,
            keepalive: None,
        };

        let main_loop = tokio::spawn(run(connector, ingress, peers, config, connections.clone()));

        // The first datagram cannot be written to the TUN device, so its
        // generation is given up instead of staying in the table.
        ingress_tx.send((vec![1u8; 8], peer, udp_local())).unwrap();
        for _ in 0..10_000 {
            if connects.load(Ordering::SeqCst) == 1 && connections.get(&peer).await.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(connects.load(Ordering::SeqCst), 1);
        assert!(connections.get(&peer).await.is_none());

        // The next datagram establishes a new generation without a restart.
        ingress_tx.send((vec![2u8; 8], peer, udp_local())).unwrap();
        let second = wait_for_generation(&connections, &peer, 2).await;
        assert!(!second.is_finished());

        main_loop.abort();
    }

    #[tokio::test]
    async fn worker_finishes_the_session_on_a_tun_write_failure() {
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let (socket, _tcp_tx) = fake_socket(SendBehavior::Fail, Arc::new(Notify::new()));
        let session = connections.register(peer, Arc::new(socket)).await;

        let (worker_peer, udp_tx) = fake_peer(
            false,
            Arc::new(Notify::new()),
            Arc::new(AtomicUsize::new(0)),
        );
        let worker = tokio::spawn(run_worker(
            0,
            session.clone(),
            connections.clone(),
            worker_peer,
        ));

        udp_tx.send(Ok(vec![7u8; 4])).unwrap();

        worker.await.unwrap();
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn outgoing_activity_does_not_shift_probes_or_termination() {
        let t0 = Instant::now();
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let (socket, _tcp_tx) = fake_socket(SendBehavior::Ok, Arc::new(Notify::new()));
        let probes = socket.probes.clone();
        let session = connections.register(peer, Arc::new(socket)).await;

        // Steady outgoing payload traffic, far more frequent than the probe
        // interval.
        let notifier = tokio::spawn({
            let session = session.clone();
            async move {
                loop {
                    time::sleep(Duration::from_secs(1)).await;
                    session.notify_activity();
                }
            }
        });

        run_manager(
            session.clone(),
            connections.clone(),
            Some(keepalive_20_5_3()),
            t0,
        )
        .await;
        notifier.abort();

        assert_eq!(probe_offsets(&probes, t0), vec![20, 25, 30]);
        assert_eq!((Instant::now() - t0).as_secs(), 35);
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn incoming_empty_ack_restarts_the_probe_budget() {
        let t0 = Instant::now();
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let (socket, tcp_tx) = fake_socket(SendBehavior::Ok, Arc::new(Notify::new()));
        let probes = socket.probes.clone();
        let session = connections.register(peer, Arc::new(socket)).await;

        let dropped = Arc::new(AtomicUsize::new(0));
        let (worker_peer, _udp_tx) = fake_peer(false, Arc::new(Notify::new()), dropped);
        let worker = tokio::spawn(run_worker(
            0,
            session.clone(),
            connections.clone(),
            worker_peer,
        ));
        let manager = tokio::spawn(run_manager(
            session.clone(),
            connections.clone(),
            Some(keepalive_20_5_3()),
            t0,
        ));

        // An empty fake-TCP ACK arrives before the second probe is due and
        // restarts the whole budget from that moment.
        time::sleep_until(t0 + Duration::from_secs(22)).await;
        tcp_tx.send(Vec::new()).unwrap();

        manager.await.unwrap();
        worker.await.unwrap();

        assert_eq!(probe_offsets(&probes, t0), vec![20, 42, 47, 52]);
        assert_eq!((Instant::now() - t0).as_secs(), 57);
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_works_without_keepalive() {
        let t0 = Instant::now();
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let (socket, _tcp_tx) = fake_socket(SendBehavior::Ok, Arc::new(Notify::new()));
        let probes = socket.probes.clone();
        let session = connections.register(peer, Arc::new(socket)).await;

        run_manager(session.clone(), connections.clone(), None, t0).await;

        assert!(probe_offsets(&probes, t0).is_empty());
        assert_eq!(Instant::now() - t0, UDP_TTL);
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_fires_while_the_keepalive_budget_stays_fresh() {
        let t0 = Instant::now();
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let (socket, _tcp_tx) = fake_socket(SendBehavior::Ok, Arc::new(Notify::new()));
        let probes = socket.probes.clone();
        let session = connections.register(peer, Arc::new(socket)).await;

        // Incoming ACKs restart the probe budget often enough for no probe to
        // ever be due.
        let notifier = tokio::spawn({
            let session = session.clone();
            async move {
                loop {
                    time::sleep(Duration::from_secs(10)).await;
                    session.notify_incoming();
                }
            }
        });

        run_manager(
            session.clone(),
            connections.clone(),
            Some(keepalive_20_5_3()),
            t0,
        )
        .await;
        notifier.abort();

        // Empty ACKs are not payload traffic, so the idle timeout still fires.
        assert!(probe_offsets(&probes, t0).is_empty());
        assert_eq!(Instant::now() - t0, UDP_TTL);
        assert!(session.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn failed_keepalive_probe_finishes_the_session() {
        let t0 = Instant::now();
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let (socket, _tcp_tx) = fake_socket(SendBehavior::Fail, Arc::new(Notify::new()));
        let probes = socket.probes.clone();
        let session = connections.register(peer, Arc::new(socket)).await;

        run_manager(
            session.clone(),
            connections.clone(),
            Some(keepalive_20_5_3()),
            t0,
        )
        .await;

        assert_eq!(probe_offsets(&probes, t0), vec![20]);
        assert_eq!((Instant::now() - t0).as_secs(), 20);
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_keepalive_probe_finishes_the_session() {
        let t0 = Instant::now();
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let send_started = Arc::new(Notify::new());
        let (socket, _tcp_tx) = fake_socket(SendBehavior::Hang, send_started.clone());
        let session = connections.register(peer, Arc::new(socket)).await;

        let manager = tokio::spawn(run_manager(
            session.clone(),
            connections.clone(),
            Some(keepalive_20_5_3()),
            t0,
        ));
        send_started.notified().await;
        manager.await.unwrap();

        assert_eq!(
            Instant::now() - t0,
            Duration::from_secs(20) + KEEPALIVE_SEND_TIMEOUT
        );
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn keepalive_exhaustion_is_not_blocked_by_a_pending_probe() {
        let t0 = Instant::now();
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let send_started = Arc::new(Notify::new());
        let (socket, _tcp_tx) = fake_socket(SendBehavior::Hang, send_started.clone());
        let session = connections.register(peer, Arc::new(socket)).await;

        let manager = tokio::spawn(run_manager(
            session.clone(),
            connections.clone(),
            Some(KeepaliveConfig {
                time: Duration::from_secs(20),
                interval: Duration::from_secs(1),
                retries: 3,
            }),
            t0,
        ));
        send_started.notified().await;
        manager.await.unwrap();

        // The probe due at t0 + 20s never completes, but the budget still runs
        // out at t0 + 23s instead of waiting for the send timeout at t0 + 25s.
        assert_eq!(Instant::now() - t0, Duration::from_secs(23));
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_probe_send_does_not_shift_the_next_probe() {
        let t0 = Instant::now();
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let (socket, _tcp_tx) = fake_socket(SendBehavior::Hold, Arc::new(Notify::new()));
        let probes = socket.probes.clone();
        let release = socket.release.clone();
        let released = socket.released.clone();
        let session = connections.register(peer, Arc::new(socket)).await;

        let manager = tokio::spawn(run_manager(
            session.clone(),
            connections.clone(),
            Some(keepalive_20_5_3()),
            t0,
        ));

        // The probe due at t0 + 20s only completes at t0 + 24s; the next
        // probe must still be due at t0 + 25s.
        time::sleep_until(t0 + Duration::from_secs(24)).await;
        released.store(true, Ordering::SeqCst);
        release.notify_one();
        manager.await.unwrap();

        assert_eq!(probe_offsets(&probes, t0), vec![20, 25, 30]);
        assert_eq!((Instant::now() - t0).as_secs(), 35);
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn an_incoming_packet_during_a_probe_send_keeps_the_restarted_budget() {
        let t0 = Instant::now();
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let (socket, _tcp_tx) = fake_socket(SendBehavior::Hold, Arc::new(Notify::new()));
        let probes = socket.probes.clone();
        let release = socket.release.clone();
        let released = socket.released.clone();
        let session = connections.register(peer, Arc::new(socket)).await;

        let manager = tokio::spawn(run_manager(
            session.clone(),
            connections.clone(),
            Some(keepalive_20_5_3()),
            t0,
        ));

        // The probe due at t0 + 20s is still in flight when an incoming packet
        // restarts the budget at t0 + 21s.
        time::sleep_until(t0 + Duration::from_secs(21)).await;
        assert_eq!(probe_offsets(&probes, t0), vec![20]);
        session.notify_incoming();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        released.store(true, Ordering::SeqCst);
        release.notify_one();
        manager.await.unwrap();

        // The probe of the old budget is not counted: probes are due 20s after
        // the restart and the session ends 15s later.
        assert_eq!(probe_offsets(&probes, t0), vec![20, 41, 46, 51]);
        assert_eq!((Instant::now() - t0).as_secs(), 56);
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());
    }

    #[test]
    fn local_bind_addr_mirrors_the_incoming_destination() {
        let remote: SocketAddr = "192.0.2.10:1234".parse().unwrap();
        let local: IpAddr = "192.0.2.1".parse().unwrap();
        assert_eq!(
            local_bind_addr(remote, local, 4567).unwrap(),
            "192.0.2.1:4567".parse::<SocketAddr>().unwrap()
        );

        let remote6: SocketAddr = "[fe80::2%7]:1234".parse().unwrap();
        let local6: IpAddr = "2001:db8::1".parse().unwrap();
        match local_bind_addr(remote6, local6, 4567).unwrap() {
            SocketAddr::V6(bound) => {
                assert_eq!(bound.ip(), &local6);
                assert_eq!(bound.port(), 4567);
                assert_eq!(bound.scope_id(), 7);
            }
            other => panic!("expected an IPv6 bind address, got {other}"),
        }
    }

    #[test]
    fn local_bind_addr_rejects_mixed_families() {
        let remote: SocketAddr = "192.0.2.10:1234".parse().unwrap();
        let local6: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(
            local_bind_addr(remote, local6, 4567).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[tokio::test]
    async fn worker_forwards_a_fake_tcp_payload_to_the_local_peer() {
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let (socket, tcp_tx) = fake_socket(SendBehavior::Ok, Arc::new(Notify::new()));
        let session = connections.register(peer, Arc::new(socket)).await;

        let (worker_peer, _udp_tx, peer_sent) = fake_peer_full(
            false,
            false,
            Arc::new(Notify::new()),
            Arc::new(AtomicUsize::new(0)),
        );
        let worker = tokio::spawn(run_worker(
            0,
            session.clone(),
            connections.clone(),
            worker_peer,
        ));

        tcp_tx.send(vec![9u8, 8, 7]).unwrap();
        for _ in 0..10_000 {
            if !peer_sent.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert_eq!(*peer_sent.lock().unwrap(), vec![vec![9u8, 8, 7]]);
        assert!(!session.is_finished());

        assert!(connections.finish(&session).await);
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn worker_finishes_the_session_when_the_fake_tcp_receive_closes() {
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let (socket, tcp_tx) = fake_socket_with(SendBehavior::Ok, Arc::new(Notify::new()), true);
        drop(tcp_tx);
        let session = connections.register(peer, Arc::new(socket)).await;

        let (worker_peer, _udp_tx) = fake_peer(
            false,
            Arc::new(Notify::new()),
            Arc::new(AtomicUsize::new(0)),
        );
        let worker = tokio::spawn(run_worker(
            0,
            session.clone(),
            connections.clone(),
            worker_peer,
        ));

        worker.await.unwrap();
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());
    }

    #[tokio::test]
    async fn worker_finishes_the_session_on_a_local_udp_send_error() {
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let (socket, tcp_tx) = fake_socket(SendBehavior::Ok, Arc::new(Notify::new()));
        let session = connections.register(peer, Arc::new(socket)).await;

        let (worker_peer, _udp_tx, _sent) = fake_peer_full(
            false,
            true,
            Arc::new(Notify::new()),
            Arc::new(AtomicUsize::new(0)),
        );
        let worker = tokio::spawn(run_worker(
            0,
            session.clone(),
            connections.clone(),
            worker_peer,
        ));

        tcp_tx.send(vec![7u8; 4]).unwrap();
        worker.await.unwrap();
        assert!(session.is_finished());
        assert!(connections.get(&peer).await.is_none());
    }

    #[tokio::test]
    async fn main_loop_sends_the_handshake_and_first_datagram_and_starts_workers() {
        let peer = udp_peer();
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel();
        let ingress = FakeIngress {
            rx: AsyncMutex::new(ingress_rx),
        };
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let connects = Arc::new(AtomicUsize::new(0));
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let connector = FakeConnector {
            connects: connects.clone(),
            send_behavior: SendBehavior::Ok,
            first_socket_fails: false,
            send_started: Arc::new(Notify::new()),
            sent: sent.clone(),
        };
        let created = Arc::new(AtomicUsize::new(0));
        let peers = FakePeerFactory {
            dropped: Arc::new(AtomicUsize::new(0)),
            created: created.clone(),
            create_fails: false,
        };
        let config = ClientConfig {
            remote_addr: remote(),
            handshake_packet: Some(vec![0xAA]),
            num_workers: 2,
            keepalive: None,
        };

        let main_loop = tokio::spawn(run(connector, ingress, peers, config, connections.clone()));

        ingress_tx
            .send((vec![1u8, 2, 3], peer, udp_local()))
            .unwrap();
        for _ in 0..10_000 {
            let ready = connects.load(Ordering::SeqCst) == 1
                && created.load(Ordering::SeqCst) == 2
                && sent.lock().unwrap().len() >= 2;
            if ready {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert_eq!(
            sent.lock().unwrap().clone(),
            vec![vec![0xAAu8], vec![1u8, 2, 3]]
        );
        assert_eq!(created.load(Ordering::SeqCst), 2);
        assert!(connections.get(&peer).await.is_some());

        main_loop.abort();
    }

    #[tokio::test]
    async fn main_loop_drops_the_session_when_the_handshake_send_fails() {
        let peer = udp_peer();
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel();
        let ingress = FakeIngress {
            rx: AsyncMutex::new(ingress_rx),
        };
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let connects = Arc::new(AtomicUsize::new(0));
        let connector = FakeConnector {
            connects: connects.clone(),
            send_behavior: SendBehavior::Fail,
            first_socket_fails: false,
            send_started: Arc::new(Notify::new()),
            sent: Arc::new(StdMutex::new(Vec::new())),
        };
        let peers = FakePeerFactory {
            dropped: Arc::new(AtomicUsize::new(0)),
            created: Arc::new(AtomicUsize::new(0)),
            create_fails: false,
        };
        let config = ClientConfig {
            remote_addr: remote(),
            handshake_packet: Some(vec![0xAA]),
            num_workers: 1,
            keepalive: None,
        };

        let main_loop = tokio::spawn(run(connector, ingress, peers, config, connections.clone()));

        ingress_tx.send((vec![1u8; 8], peer, udp_local())).unwrap();
        for _ in 0..10_000 {
            if connects.load(Ordering::SeqCst) == 1 && connections.get(&peer).await.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert_eq!(connects.load(Ordering::SeqCst), 1);
        assert!(connections.get(&peer).await.is_none());

        main_loop.abort();
    }

    #[tokio::test]
    async fn main_loop_finishes_the_session_when_a_fastpath_socket_cannot_be_created() {
        let peer = udp_peer();
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel();
        let ingress = FakeIngress {
            rx: AsyncMutex::new(ingress_rx),
        };
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let connects = Arc::new(AtomicUsize::new(0));
        let connector = FakeConnector {
            connects: connects.clone(),
            send_behavior: SendBehavior::Ok,
            first_socket_fails: false,
            send_started: Arc::new(Notify::new()),
            sent: Arc::new(StdMutex::new(Vec::new())),
        };
        let created = Arc::new(AtomicUsize::new(0));
        let peers = FakePeerFactory {
            dropped: Arc::new(AtomicUsize::new(0)),
            created: created.clone(),
            create_fails: true,
        };
        let config = ClientConfig {
            remote_addr: remote(),
            handshake_packet: None,
            num_workers: 1,
            keepalive: None,
        };

        let main_loop = tokio::spawn(run(connector, ingress, peers, config, connections.clone()));

        ingress_tx.send((vec![1u8; 8], peer, udp_local())).unwrap();
        for _ in 0..10_000 {
            if connects.load(Ordering::SeqCst) == 1
                && created.load(Ordering::SeqCst) >= 1
                && connections.get(&peer).await.is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert_eq!(created.load(Ordering::SeqCst), 1);
        assert!(connections.get(&peer).await.is_none());

        main_loop.abort();
    }

    #[test]
    fn session_end_reasons_are_distinguishable() {
        assert_eq!(SessionEnd::KeepaliveExhausted.level(), log::Level::Info);
        assert_eq!(
            SessionEnd::KeepaliveExhausted.describe(),
            "keepalive probes exhausted"
        );

        assert_eq!(SessionEnd::IdleTimeout.level(), log::Level::Info);
        assert!(
            SessionEnd::IdleTimeout
                .describe()
                .contains("no traffic seen")
        );

        assert_eq!(
            SessionEnd::TunWriteFailed("datagram").level(),
            log::Level::Error
        );
        assert_eq!(
            SessionEnd::TunWriteFailed("datagram").describe(),
            "unable to write datagram to the TUN device"
        );

        assert_eq!(SessionEnd::SendTimedOut.level(), log::Level::Warn);
        assert!(
            SessionEnd::SendTimedOut
                .describe()
                .contains("still pending after")
        );

        let error = io::Error::new(io::ErrorKind::ConnectionReset, "reset");
        assert_eq!(SessionEnd::UdpRecvFailed(error).level(), log::Level::Error);
        assert!(
            SessionEnd::UdpRecvFailed(io::Error::new(io::ErrorKind::ConnectionReset, "reset"))
                .describe()
                .contains("reset")
        );

        assert_eq!(SessionEnd::FakeTcpRecvClosed.level(), log::Level::Debug);
        assert_eq!(
            SessionEnd::FakeTcpRecvClosed.describe(),
            "fake-TCP receive closed"
        );
    }
}
