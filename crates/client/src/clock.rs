//! Estimates the offset between the local clock and the server clock from
//! ping/pong round trips (NTP-like): the sample with the smallest round trip
//! among the recent ones is the most trustworthy.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

const WINDOW: usize = 8;

pub fn local_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    rtt_ms: u64,
    offset_ms: i64,
}

#[derive(Debug, Default)]
pub struct Clock {
    samples: VecDeque<Sample>,
}

impl Clock {
    /// Records a pong: `t0` sent locally, `server_ms` stamped by the server,
    /// `t1` received locally.
    pub fn record(&mut self, t0: u64, server_ms: u64, t1: u64) {
        let rtt_ms = t1.saturating_sub(t0);
        // The server stamped its time roughly half-way through the round trip.
        let offset_ms = server_ms as i64 + (rtt_ms / 2) as i64 - t1 as i64;
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample { rtt_ms, offset_ms });
    }

    pub fn is_ready(&self) -> bool {
        !self.samples.is_empty()
    }

    fn best(&self) -> Option<Sample> {
        self.samples.iter().min_by_key(|s| s.rtt_ms).copied()
    }

    /// Server clock minus local clock, in ms.
    pub fn offset_ms(&self) -> i64 {
        self.best().map(|s| s.offset_ms).unwrap_or(0)
    }

    pub fn rtt_ms(&self) -> Option<u64> {
        self.best().map(|s| s.rtt_ms)
    }

    pub fn server_now(&self) -> u64 {
        to_server(local_ms(), self.offset_ms())
    }
}

fn to_server(local: u64, offset: i64) -> u64 {
    (local as i64 + offset).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_lowest_rtt_sample() {
        let mut c = Clock::default();
        // Server is 1000 ms ahead. Fast sample: rtt 20.
        c.record(10_000, 11_010, 10_020);
        // Slow, asymmetric sample that would skew the estimate.
        c.record(20_000, 21_400, 20_500);
        assert_eq!(c.rtt_ms(), Some(20));
        assert_eq!(c.offset_ms(), 1_000);
        assert_eq!(to_server(5_000, c.offset_ms()), 6_000);
    }

    #[test]
    fn forgets_old_samples() {
        let mut c = Clock::default();
        c.record(0, 0, 2); // excellent but old
        for i in 0..WINDOW as u64 {
            c.record(i * 100, i * 100 + 500 + 50, i * 100 + 100);
        }
        assert_eq!(c.rtt_ms(), Some(100));
    }
}
