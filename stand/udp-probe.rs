//! Minimal UDP helpers for the session-recovery stand.
//!
//! Two modes, both standard-library only so the stand can build the tool with
//! a plain `rustc` invocation:
//!
//! * `echo ADDR` binds a UDP socket and echoes every datagram back to its
//!   sender. It plays the role of the application behind the Phantun server.
//! * `drive TARGET INTERVAL_MS` sends a datagram every `INTERVAL_MS`
//!   milliseconds to `TARGET` and logs every send and every reply as
//!   `<kind> <unix_microseconds> <counter>` on stdout. The stand parses those
//!   lines to prove that datagrams sent after the new session was established
//!   are answered.
//!
//! Exit codes: 0 never (both modes run until killed), 2 usage error.

use std::env;
use std::io::{self, Write};
use std::net::{SocketAddr, UdpSocket};
use std::process;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_us() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros())
}

fn usage() -> ! {
    eprintln!("usage: udp-probe echo ADDR");
    eprintln!("       udp-probe drive TARGET INTERVAL_MS");
    process::exit(2);
}

fn echo(addr: SocketAddr) -> io::Result<()> {
    let sock = UdpSocket::bind(addr)?;
    let mut buf = [0u8; 2048];

    loop {
        match sock.recv_from(&mut buf) {
            Ok((size, peer)) => {
                if let Err(error) = sock.send_to(&buf[..size], peer) {
                    eprintln!("echo: send to {peer}: {error}");
                }
            }
            Err(error) => eprintln!("echo: receive: {error}"),
        }
    }
}

fn drive(target: SocketAddr, interval: Duration) -> io::Result<()> {
    let sock = UdpSocket::bind("127.0.0.1:0")?;
    sock.connect(target)?;

    let mut buf = [0u8; 2048];
    let mut next = Instant::now();
    let mut counter: u64 = 0;

    loop {
        let now = Instant::now();
        if now >= next {
            counter += 1;
            sock.send(&counter.to_be_bytes())?;
            println!("send {} {}", now_us(), counter);
            io::stdout().flush()?;
            next += interval;
            // Never queue a burst after a stall: the next slot is measured
            // from the current time instead of the missed one.
            if next <= now {
                next = now + interval;
            }
        }

        let wait = next.saturating_duration_since(Instant::now());
        let wait = if wait.is_zero() {
            Duration::from_millis(1)
        } else {
            wait
        };
        sock.set_read_timeout(Some(wait))?;

        match sock.recv(&mut buf) {
            Ok(size) if size >= 8 => {
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&buf[..8]);
                println!("recv {} {}", now_us(), u64::from_be_bytes(bytes));
                io::stdout().flush()?;
            }
            Ok(_) => {}
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error),
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    let result = match args.as_slice() {
        [mode, addr] if mode == "echo" => match addr.parse::<SocketAddr>() {
            Ok(addr) => echo(addr),
            Err(error) => {
                eprintln!("udp-probe: bad address {addr}: {error}");
                process::exit(2);
            }
        },
        [mode, target, interval] if mode == "drive" => {
            let target = match target.parse::<SocketAddr>() {
                Ok(target) => target,
                Err(error) => {
                    eprintln!("udp-probe: bad target {target}: {error}");
                    process::exit(2);
                }
            };
            match interval.parse::<u64>() {
                Ok(interval) if interval > 0 => drive(target, Duration::from_millis(interval)),
                _ => usage(),
            }
        }
        _ => usage(),
    };

    if let Err(error) = result {
        eprintln!("udp-probe: {error}");
        process::exit(1);
    }
}
