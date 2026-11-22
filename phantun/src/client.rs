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
use crate::utils::{new_udp_reuseport, udp_recv_pktinfo};

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
        // Always reply from the same address that the peer used to communicate with
        // us. This avoids a frequent problem with IPv6 privacy extensions when we
        // erroneously bind to wrong short-lived temporary address even if the peer
        // explicitly used a persistent address to communicate to us.
        //
        // To do so, first bind to (<incoming packet dst_ip>, <local addr port>), and then
        // connect to (<incoming packet src_ip>, <incoming packet src_port>).
        let bind_addr = match (udp_remote_addr, udp_local_addr) {
            (SocketAddr::V4(_), IpAddr::V4(udp_local_ipv4)) => {
                SocketAddr::V4(SocketAddrV4::new(udp_local_ipv4, self.local_port))
            }
            (SocketAddr::V6(udp_remote_addr), IpAddr::V6(udp_local_ipv6)) => {
                SocketAddr::V6(SocketAddrV6::new(
                    udp_local_ipv6,
                    self.local_port,
                    udp_remote_addr.flowinfo(),
                    udp_remote_addr.scope_id(),
                ))
            }
            (_, _) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "unexpected family combination for udp_remote_addr={udp_remote_addr} and udp_local_addr={udp_local_addr}"
                    ),
                ));
            }
        };

        let udp_sock = new_udp_reuseport(bind_addr);
        udp_sock.connect(udp_remote_addr).await?;

        Ok(udp_sock)
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
    /// A keepalive probe stayed pending longer than [`KEEPALIVE_SEND_TIMEOUT`].
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
                format!("keepalive probe still pending after {KEEPALIVE_SEND_TIMEOUT:?}")
            }
            Self::UdpRecvFailed(error) => format!("unable to receive local datagram: {error}"),
            Self::FakeTcpRecvClosed => "fake-TCP receive closed".to_owned(),
        }
    }
}

/// Finishes `session` with one log record naming the reason, the session
/// generation and the local UDP peer.
async fn end_session<S: FakeTcpSocket>(
    connections: &Connections<Arc<S>>,
    session: &Session<Arc<S>>,
    reason: SessionEnd,
) {
    log::log!(
        reason.level(),
        "Session {} for local UDP peer {}: {}, closing session",
        session.generation(),
        session.udp_remote_addr(),
        reason.describe()
    );
    connections.finish(session).await;
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
            match send_payload(&session, packet).await {
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
        match send_payload(&session, &buf_r[..size]).await {
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    struct FakeSocket {
        send_hangs: bool,
        send_started: Arc<Notify>,
        recv_rx: AsyncMutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    }

    impl FakeTcpSocket for FakeSocket {
        async fn send(&self, _payload: &[u8]) -> Option<()> {
            if self.send_hangs {
                self.send_started.notify_one();
                return std::future::pending().await;
            }
            Some(())
        }

        async fn send_keepalive(&self) -> Option<()> {
            self.send(&[]).await
        }

        async fn recv(&self, buf: &mut [u8]) -> Option<usize> {
            match self.recv_rx.lock().await.recv().await {
                Some(payload) => {
                    let size = payload.len().min(buf.len());
                    buf[..size].copy_from_slice(&payload[..size]);
                    Some(size)
                }
                None => std::future::pending().await,
            }
        }
    }

    struct FakeConnector {
        connects: Arc<AtomicUsize>,
        send_hangs: bool,
        send_started: Arc<Notify>,
    }

    impl FakeTcpConnector for FakeConnector {
        type Socket = FakeSocket;

        async fn connect(&mut self, _remote: SocketAddr) -> Option<FakeSocket> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            let (_tx, rx) = mpsc::unbounded_channel();
            Some(FakeSocket {
                send_hangs: self.send_hangs,
                send_started: self.send_started.clone(),
                recv_rx: AsyncMutex::new(rx),
            })
        }
    }

    struct FakePeer {
        recv_rx: AsyncMutex<mpsc::UnboundedReceiver<io::Result<Vec<u8>>>>,
        send_hangs: bool,
        send_started: Arc<Notify>,
        dropped: Arc<AtomicUsize>,
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

        async fn send(&self, _buf: &[u8]) -> io::Result<usize> {
            if self.send_hangs {
                self.send_started.notify_one();
                return std::future::pending().await;
            }
            Ok(0)
        }
    }

    struct FakePeerFactory {
        dropped: Arc<AtomicUsize>,
    }

    impl PeerFactory for FakePeerFactory {
        type Peer = FakePeer;

        async fn create(&self, _remote: SocketAddr, _local: IpAddr) -> io::Result<FakePeer> {
            let (_tx, rx) = mpsc::unbounded_channel();
            Ok(FakePeer {
                recv_rx: AsyncMutex::new(rx),
                send_hangs: false,
                send_started: Arc::new(Notify::new()),
                dropped: self.dropped.clone(),
            })
        }
    }

    fn fake_socket(
        send_hangs: bool,
        send_started: Arc<Notify>,
    ) -> (FakeSocket, mpsc::UnboundedSender<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            FakeSocket {
                send_hangs,
                send_started,
                recv_rx: AsyncMutex::new(rx),
            },
            tx,
        )
    }

    fn fake_peer(
        send_hangs: bool,
        send_started: Arc<Notify>,
        dropped: Arc<AtomicUsize>,
    ) -> (FakePeer, mpsc::UnboundedSender<io::Result<Vec<u8>>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            FakePeer {
                recv_rx: AsyncMutex::new(rx),
                send_hangs,
                send_started,
                dropped,
            },
            tx,
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
            send_hangs: true,
            send_started: send_started.clone(),
        };
        let peers = FakePeerFactory {
            dropped: Arc::new(AtomicUsize::new(0)),
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

    #[tokio::test]
    async fn worker_interrupts_a_pending_fake_tcp_send() {
        let peer = udp_peer();
        let connections = Arc::new(Connections::<Arc<FakeSocket>>::new());
        let send_started = Arc::new(Notify::new());
        let (socket, _tcp_tx) = fake_socket(true, send_started.clone());
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
        let (socket, tcp_tx) = fake_socket(false, Arc::new(Notify::new()));
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
        let (socket, _tcp_tx) = fake_socket(false, Arc::new(Notify::new()));
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
}
