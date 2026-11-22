//! Liveness probe schedule for fake-TCP sessions.
//!
//! The schedule is anchored to `t0`, the time of the last incoming fake-TCP
//! packet (an empty ACK counts). Probes are due at `t0 + K + n * I` for
//! `n = 0..R` and the session is given up at `t0 + K + R * I`, so outgoing
//! traffic and late sends never postpone a probe or the termination. Every
//! incoming fake-TCP packet restarts the budget from a new `t0`.

use std::time::Duration;

use tokio::time::Instant;

/// Deadline schedule of keepalive probes.
pub struct KeepaliveTimer {
    last_incoming: Instant,
    time: Duration,
    interval: Duration,
    retries: u32,
    probes_sent: u32,
}

impl KeepaliveTimer {
    /// `now` is the initial `t0`, fixed when the session is established.
    pub fn new(now: Instant, time: Duration, interval: Duration, retries: u32) -> Self {
        assert!(retries > 0, "keepalive needs at least one retry");

        Self {
            last_incoming: now,
            time,
            interval,
            retries,
            probes_sent: 0,
        }
    }

    /// Records an incoming fake-TCP packet, restarting the probe budget.
    pub fn on_incoming(&mut self, now: Instant) {
        self.last_incoming = now;
        self.probes_sent = 0;
    }

    /// Absolute deadline of the next action: the next probe while the retry
    /// budget lasts, the session termination once it is exhausted.
    ///
    /// The deadline is derived from `t0` and the number of probes already
    /// sent, never from the time a send actually happened, so a delayed send
    /// does not shift the following deadlines.
    pub fn next_deadline(&self) -> Instant {
        self.last_incoming + self.time + self.interval * self.probes_sent
    }

    /// Whether all retries have been used, making the next deadline the
    /// session termination time.
    pub fn probes_exhausted(&self) -> bool {
        self.probes_sent >= self.retries
    }

    /// Number of probes already sent within the current budget.
    pub fn probes_sent(&self) -> u32 {
        self.probes_sent
    }

    /// Accounts for a probe sent at its deadline.
    pub fn probe_sent(&mut self) {
        self.probes_sent += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn probes_and_termination_follow_the_incoming_anchor() {
        let t0 = Instant::now();
        let mut timer = KeepaliveTimer::new(t0, secs(20), secs(5), 3);

        assert_eq!(timer.next_deadline(), t0 + secs(20));
        assert!(!timer.probes_exhausted());
        timer.probe_sent();

        assert_eq!(timer.next_deadline(), t0 + secs(25));
        assert!(!timer.probes_exhausted());
        timer.probe_sent();

        assert_eq!(timer.next_deadline(), t0 + secs(30));
        assert!(!timer.probes_exhausted());
        timer.probe_sent();

        assert!(timer.probes_exhausted());
        assert_eq!(timer.next_deadline(), t0 + secs(35));
    }

    #[test]
    fn incoming_packet_restarts_the_budget() {
        let t0 = Instant::now();
        let mut timer = KeepaliveTimer::new(t0, secs(20), secs(5), 3);
        timer.probe_sent();
        timer.probe_sent();
        assert_eq!(timer.next_deadline(), t0 + secs(30));

        let t1 = t0 + secs(22);
        timer.on_incoming(t1);

        assert_eq!(timer.probes_sent(), 0);
        assert!(!timer.probes_exhausted());
        assert_eq!(timer.next_deadline(), t1 + secs(20));
    }

    #[test]
    fn late_send_does_not_shift_later_deadlines() {
        let t0 = Instant::now();
        let mut timer = KeepaliveTimer::new(t0, secs(20), secs(5), 3);

        // The probe due at t0 + 20s is actually sent at t0 + 24s; the next
        // probe is still due at t0 + 25s rather than 29s.
        timer.probe_sent();
        assert_eq!(timer.next_deadline(), t0 + secs(25));
    }
}
