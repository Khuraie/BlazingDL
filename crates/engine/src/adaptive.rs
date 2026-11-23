//! Auto-adaptive stream concurrency controller: ramps parallel connections up when adding
//! streams increases aggregate throughput, and holds or scales down when marginal gain plateaus
//! or the disk write cache stalls.

use std::time::{Duration, Instant};

/// Sampling window between concurrency scaling decisions.
const WINDOW: Duration = Duration::from_secs(2);
/// Minimum relative throughput gain (+8%) required to keep scaling up.
const MIN_GAIN: f64 = 1.08;

#[derive(Clone, Debug)]
pub struct AdaptiveScaler {
    current: u32,
    min: u32,
    max: u32,
    last_rate: f64,
    last_eval: Instant,
    stagnant_steps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleAction {
    Keep(u32),
    Increase(u32),
    Decrease(u32),
}

impl AdaptiveScaler {
    pub fn new(initial: u32, min: u32, max: u32) -> Self {
        let min = min.max(1);
        let max = max.max(min);
        let current = initial.clamp(min, max);
        Self {
            current,
            min,
            max,
            last_rate: 0.0,
            last_eval: Instant::now(),
            stagnant_steps: 0,
        }
    }

    pub fn target(&self) -> u32 {
        self.current
    }

    /// Evaluate live aggregate `rate` (bytes/sec) and whether disk writes are stalled.
    pub fn evaluate(&mut self, now: Instant, rate: f64, disk_stalled: bool) -> ScaleAction {
        if now.duration_since(self.last_eval) < WINDOW {
            return ScaleAction::Keep(self.current);
        }
        self.last_eval = now;
        if disk_stalled {
            self.stagnant_steps = 0;
            if self.current > self.min {
                self.current = (self.current / 2).max(self.min);
                self.last_rate = rate;
                return ScaleAction::Decrease(self.current);
            }
            self.last_rate = rate;
            return ScaleAction::Keep(self.current);
        }
        if self.last_rate <= 0.0 {
            self.last_rate = rate;
            if rate > 0.0 && self.current < self.max {
                self.current = (self.current + 2).min(self.max);
                return ScaleAction::Increase(self.current);
            }
            return ScaleAction::Keep(self.current);
        }
        let ratio = rate / self.last_rate.max(1.0);
        self.last_rate = rate;
        if ratio >= MIN_GAIN && self.current < self.max {
            self.stagnant_steps = 0;
            self.current = (self.current + 2).min(self.max);
            ScaleAction::Increase(self.current)
        } else if ratio < 0.85 && self.current > self.min {
            self.stagnant_steps += 1;
            if self.stagnant_steps >= 2 {
                self.stagnant_steps = 0;
                self.current = self.current.saturating_sub(2).max(self.min);
                return ScaleAction::Decrease(self.current);
            }
            ScaleAction::Keep(self.current)
        } else {
            self.stagnant_steps = 0;
            ScaleAction::Keep(self.current)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramps_up_on_throughput_gain_and_backs_off_on_disk_stall() {
        let mut s = AdaptiveScaler::new(4, 2, 16);
        let t0 = s.last_eval;
        assert_eq!(
            s.evaluate(t0 + Duration::from_secs(2), 10_000_000.0, false),
            ScaleAction::Increase(6)
        );
        assert_eq!(
            s.evaluate(t0 + Duration::from_secs(4), 13_000_000.0, false),
            ScaleAction::Increase(8)
        );
        // Plateau holds steady.
        assert_eq!(
            s.evaluate(t0 + Duration::from_secs(6), 13_200_000.0, false),
            ScaleAction::Keep(8)
        );
        // Disk stall immediately halves concurrency.
        assert_eq!(
            s.evaluate(t0 + Duration::from_secs(8), 11_000_000.0, true),
            ScaleAction::Decrease(4)
        );
    }
}
