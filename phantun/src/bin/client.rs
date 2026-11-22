use clap::{Arg, ArgAction, Command, crate_version};
use fake_tcp::{Socket, Stack};
use log::info;
use phantun::client::{BindPeerFactory, ClientConfig, KeepaliveConfig};
use phantun::session::Connections;
use phantun::utils::{assign_ipv6_address, new_udp_reuseport};
use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio_tun::TunBuilder;

#[tokio::main]
async fn main() -> io::Result<()> {
    pretty_env_logger::init();

    let matches = Command::new("Phantun Client")
        .version(crate_version!())
        .author("Datong Sun (github.com/dndx)")
        .arg(
            Arg::new("local")
                .short('l')
                .long("local")
                .required(true)
                .value_name("IP:PORT")
                .help("Sets the IP and port where Phantun Client listens for incoming UDP datagrams, IPv6 address need to be specified as: \"[IPv6]:PORT\"")
        )
        .arg(
            Arg::new("remote")
                .short('r')
                .long("remote")
                .required(true)
                .value_name("IP or HOST NAME:PORT")
                .help("Sets the address or host name and port where Phantun Client connects to Phantun Server, IPv6 address need to be specified as: \"[IPv6]:PORT\"")
        )
        .arg(
            Arg::new("tun")
                .long("tun")
                .required(false)
                .value_name("tunX")
                .help("Sets the Tun interface name, if absent, pick the next available name")
                .default_value("")
        )
        .arg(
            Arg::new("tun_local")
                .long("tun-local")
                .required(false)
                .value_name("IP")
                .help("Sets the Tun interface IPv4 local address (O/S's end)")
                .default_value("192.168.200.1")
        )
        .arg(
            Arg::new("tun_peer")
                .long("tun-peer")
                .required(false)
                .value_name("IP")
                .help("Sets the Tun interface IPv4 destination (peer) address (Phantun Client's end). \
                       You will need to setup SNAT/MASQUERADE rules on your Internet facing interface \
                       in order for Phantun Client to connect to Phantun Server")
                .default_value("192.168.200.2")
        )
        .arg(
            Arg::new("ipv4_only")
                .long("ipv4-only")
                .short('4')
                .required(false)
                .help("Only use IPv4 address when connecting to remote")
                .action(ArgAction::SetTrue)
                .conflicts_with_all(["tun_local6", "tun_peer6"]),
        )
        .arg(
            Arg::new("tun_local6")
                .long("tun-local6")
                .required(false)
                .value_name("IP")
                .help("Sets the Tun interface IPv6 local address (O/S's end)")
                .default_value("fcc8::1")
        )
        .arg(
            Arg::new("tun_peer6")
                .long("tun-peer6")
                .required(false)
                .value_name("IP")
                .help("Sets the Tun interface IPv6 destination (peer) address (Phantun Client's end). \
                       You will need to setup SNAT/MASQUERADE rules on your Internet facing interface \
                       in order for Phantun Client to connect to Phantun Server")
                .default_value("fcc8::2")
        )
        .arg(
            Arg::new("handshake_packet")
                .long("handshake-packet")
                .required(false)
                .value_name("PATH")
                .help("Specify a file, which, after TCP handshake, its content will be sent as the \
                      first data packet to the server.\n\
                      Note: ensure this file's size does not exceed the MTU of the outgoing interface. \
                      The content is always sent out in a single packet and will not be further segmented")
        )
        .arg(
            Arg::new("keepalive_time")
                .long("keepalive-time")
                .short('k')
                .required(false)
                .value_name("SECONDS")
                .value_parser(clap::value_parser!(u64).range(1..86400))
                .help("Specify the interval between the last packet received and the first keepalive probe.\
                If not specified, no keepalive probe will be sent.")
        )
        .arg(
            Arg::new("keepalive_interval")
                .long("keepalive-interval")
                .required(false)
                .default_value("5")
                .value_name("SECONDS")
                .value_parser(clap::value_parser!(u64).range(1..3600))
                .help("Specify the interval between keepalive probes.")
        )
        .arg(
            Arg::new("keepalive_retries")
                .long("keepalive-retries")
                .required(false)
                .default_value("3")
                .value_name("N")
                .value_parser(clap::value_parser!(i32).range(1..20))
                .help("Specify the number of keepalive probe retries before a connection reset.")
        )
        .get_matches();

    let local_addr: SocketAddr = matches
        .get_one::<String>("local")
        .unwrap()
        .parse()
        .expect("bad local address");

    let ipv4_only = matches.get_flag("ipv4_only");

    let remote_addr = tokio::net::lookup_host(matches.get_one::<String>("remote").unwrap())
        .await
        .expect("bad remote address or host")
        .find(|addr| !ipv4_only || addr.is_ipv4())
        .expect("unable to resolve remote host name");
    info!("Remote address is: {}", remote_addr);

    let tun_local: Ipv4Addr = matches
        .get_one::<String>("tun_local")
        .unwrap()
        .parse()
        .expect("bad local address for Tun interface");
    let tun_peer: Ipv4Addr = matches
        .get_one::<String>("tun_peer")
        .unwrap()
        .parse()
        .expect("bad peer address for Tun interface");

    let (tun_local6, tun_peer6) = if matches.get_flag("ipv4_only") {
        (None, None)
    } else {
        (
            matches
                .get_one::<String>("tun_local6")
                .map(|v| v.parse().expect("bad local address for Tun interface")),
            matches
                .get_one::<String>("tun_peer6")
                .map(|v| v.parse().expect("bad peer address for Tun interface")),
        )
    };

    let tun_name = matches.get_one::<String>("tun").unwrap();
    let handshake_packet: Option<Vec<u8>> = matches
        .get_one::<String>("handshake_packet")
        .map(fs::read)
        .transpose()?;

    let tcp_keepalive_intvl =
        Duration::from_secs(*matches.get_one::<u64>("keepalive_interval").unwrap());
    let tcp_keepalive_time = matches
        .get_one::<u64>("keepalive_time")
        .map(|s| Duration::from_secs(*s));
    let tcp_keepalive_retries = *matches.get_one::<i32>("keepalive_retries").unwrap();

    let num_cpus = num_cpus::get();
    info!("{} cores available", num_cpus);

    let tun = TunBuilder::new()
        .name(tun_name) // if name is empty, then it is set by kernel.
        .up() // or set it up manually using `sudo ip link set <tun-name> up`.
        .address(tun_local)
        .destination(tun_peer)
        .queues(num_cpus)
        .build()
        .unwrap();

    if remote_addr.is_ipv6() {
        assign_ipv6_address(tun[0].name(), tun_local6.unwrap(), tun_peer6.unwrap());
    }

    info!("Created TUN device {}", tun[0].name());

    let udp_sock = Arc::new(new_udp_reuseport(local_addr));
    let connections = Arc::new(Connections::<Arc<Socket>>::new());

    let stack = Stack::new(tun, tun_peer, tun_peer6);

    let config = ClientConfig {
        remote_addr,
        handshake_packet,
        num_workers: num_cpus,
        keepalive: tcp_keepalive_time.map(|time| KeepaliveConfig {
            time,
            interval: tcp_keepalive_intvl,
            retries: tcp_keepalive_retries as u32,
        }),
    };

    phantun::client::run(
        stack,
        udp_sock,
        BindPeerFactory::new(local_addr.port()),
        config,
        connections,
    )
    .await
}
