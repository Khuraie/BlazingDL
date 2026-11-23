//! Live 24-Hour Bandwidth Schedule Curve & Game / Focus Auto-Throttle Mode.
//!
//! Automatically caps aggregate download bandwidth when:
//! 1. Focus Mode is enabled and a fullscreen game, D3D app, or presentation is active
//!    (Windows `SHQueryUserNotificationState`), and/or
//! 2. An optional 24-hour bandwidth schedule curve specifies a KB/s cap for the current local hour.
//!
//! Combines cleanly with the user's manual Speed Limiter by always taking the tightest active cap
//! without overwriting persisted manual settings.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Persisted configuration for Focus Auto-Throttle and the 24-hour bandwidth schedule curve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FocusConfig {
    /// Enable automatic throttling while a fullscreen game / presentation is active.
    pub enabled: bool,
    /// Aggregate download speed cap (KB/s) applied during active Focus mode (default 2048 = 2 MB/s).
    pub throttle_kbps: u64,
    /// Detect fullscreen D3D games / busy / presentation state on Windows.
    pub detect_fullscreen: bool,
    /// Optional 24-hour schedule curve (index 0..=23 = local hour; `Some(kbps)` caps that hour).
    /// Empty vector means no hourly curve is active.
    pub hourly_curve_kbps: Vec<Option<u64>>,
}

impl Default for FocusConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            throttle_kbps: 2048,
            detect_fullscreen: true,
            hourly_curve_kbps: Vec::new(),
        }
    }
}

/// Active reason a dynamic cap is currently applied (for Saville UI status badge).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThrottleReason {
    Focus(&'static str),
    Schedule { hour: u8, kbps: u64 },
}

impl ThrottleReason {
    pub fn short_label(&self) -> String {
        match self {
            ThrottleReason::Focus(what) => format!("FOCUS:{what}"),
            ThrottleReason::Schedule { kbps, .. } => {
                if *kbps >= 1024 {
                    format!("SCHED:{}MB/s", kbps / 1024)
                } else {
                    format!("SCHED:{kbps}KB/s")
                }
            }
        }
    }
}

/// Runtime controller that caches OS fullscreen polling (every 2s) and computes effective caps.
#[derive(Default)]
pub struct FocusController {
    last_poll: Option<Instant>,
    fullscreen: bool,
    active_reason: Option<ThrottleReason>,
}

impl FocusController {
    /// Poll OS fullscreen state at most once every 2 seconds and return `(effective_bps, reason)`.
    pub fn tick(
        &mut self,
        cfg: &FocusConfig,
        now_hour: u8,
        manual_bps: Option<u64>,
    ) -> (Option<u64>, Option<ThrottleReason>) {
        if cfg.enabled && cfg.detect_fullscreen {
            let due = self
                .last_poll
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(2));
            if due {
                self.last_poll = Some(Instant::now());
                self.fullscreen = poll_fullscreen();
            }
        } else {
            self.fullscreen = false;
        }
        let (cap, reason) = evaluate_cap(cfg, now_hour, self.fullscreen, manual_bps);
        self.active_reason = reason.clone();
        (cap, reason)
    }

    pub fn active_reason(&self) -> Option<&ThrottleReason> {
        self.active_reason.as_ref()
    }
}

/// Pure function computing the effective global bandwidth cap (bytes/s) and active dynamic reason.
pub fn evaluate_cap(
    cfg: &FocusConfig,
    now_hour: u8,
    fullscreen_active: bool,
    manual_bps: Option<u64>,
) -> (Option<u64>, Option<ThrottleReason>) {
    let mut best_bps = manual_bps;
    let mut reason: Option<ThrottleReason> = None;

    if let Some(kbps) = cfg
        .hourly_curve_kbps
        .get(usize::from(now_hour))
        .copied()
        .flatten()
    {
        let bps = kbps.max(1) * 1024;
        if best_bps.is_none_or(|cur| bps < cur) {
            best_bps = Some(bps);
            reason = Some(ThrottleReason::Schedule {
                hour: now_hour,
                kbps: kbps.max(1),
            });
        }
    }

    if cfg.enabled && cfg.detect_fullscreen && fullscreen_active {
        let bps = cfg.throttle_kbps.max(1) * 1024;
        if best_bps.is_none_or(|cur| bps < cur) {
            best_bps = Some(bps);
            reason = Some(ThrottleReason::Focus("GAME"));
        }
    }

    (best_bps, reason)
}

#[cfg(windows)]
fn poll_fullscreen() -> bool {
    crate::win32::fullscreen_app_active()
}

#[cfg(not(windows))]
fn poll_fullscreen() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_throttles_when_fullscreen_and_restores_when_idle() {
        let cfg = FocusConfig {
            enabled: true,
            throttle_kbps: 2048,
            detect_fullscreen: true,
            hourly_curve_kbps: Vec::new(),
        };
        let (cap_on, reason_on) = evaluate_cap(&cfg, 14, true, None);
        assert_eq!(cap_on, Some(2048 * 1024));
        assert_eq!(reason_on, Some(ThrottleReason::Focus("GAME")));

        let (cap_off, reason_off) = evaluate_cap(&cfg, 14, false, None);
        assert_eq!(cap_off, None);
        assert_eq!(reason_off, None);
    }

    #[test]
    fn tightest_cap_wins_across_manual_schedule_and_focus() {
        let mut curve = vec![None; 24];
        curve[20] = Some(4096); // 4 MB/s evening cap
        curve[21] = Some(512); // 512 KB/s peak cap
        let cfg = FocusConfig {
            enabled: true,
            throttle_kbps: 2048,
            detect_fullscreen: true,
            hourly_curve_kbps: curve,
        };

        // At 20:00, schedule is 4 MB/s, fullscreen focus is 2 MB/s -> focus wins (tighter).
        let (cap, reason) = evaluate_cap(&cfg, 20, true, None);
        assert_eq!(cap, Some(2048 * 1024));
        assert_eq!(reason, Some(ThrottleReason::Focus("GAME")));

        // At 21:00, schedule is 512 KB/s, focus is 2 MB/s -> schedule wins (tighter).
        let (cap2, reason2) = evaluate_cap(&cfg, 21, true, None);
        assert_eq!(cap2, Some(512 * 1024));
        assert_eq!(
            reason2,
            Some(ThrottleReason::Schedule {
                hour: 21,
                kbps: 512
            })
        );

        // Stricter manual cap (256 KB/s) beats both without showing a dynamic override reason.
        let (cap3, reason3) = evaluate_cap(&cfg, 21, true, Some(256 * 1024));
        assert_eq!(cap3, Some(256 * 1024));
        assert_eq!(reason3, None);
    }
}
