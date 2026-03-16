//! Responsiveness observer for auto-enabling circuit breaker.
//!
//! Tracks LLM response latencies, learns a baseline, and detects sustained
//! degradation. When latency exceeds the baseline by a configurable threshold
//! for multiple consecutive requests, signals that protective measures
//! (circuit breaker, lane changing) should be activated.

use std::collections::VecDeque;
use std::time::Duration;

/// Observes LLM response latencies and detects degradation.
pub struct ResponsivenessObserver {
    /// Rolling window of recent latencies.
    window: VecDeque<Duration>,
    /// Maximum window size.
    window_size: usize,
    /// Learned baseline latency (average of first N requests).
    baseline: Option<Duration>,
    /// Number of samples needed to establish baseline.
    baseline_samples: usize,
    /// Multiplier over baseline that counts as "slow".
    degradation_threshold: f64,
    /// Count of consecutive slow requests.
    consecutive_slow: u32,
    /// Number of consecutive slow requests needed to trigger protection.
    slow_trigger: u32,
    /// Whether auto-protection is currently active.
    active: bool,
}

impl ResponsivenessObserver {
    pub fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(20),
            window_size: 20,
            baseline: None,
            baseline_samples: 5,
            degradation_threshold: 3.0,
            consecutive_slow: 0,
            slow_trigger: 3,
            active: false,
        }
    }

    /// Record a new latency observation.
    pub fn record(&mut self, latency: Duration) {
        self.window.push_back(latency);
        if self.window.len() > self.window_size {
            self.window.pop_front();
        }

        // Learn baseline from first N samples
        if self.baseline.is_none() && self.window.len() >= self.baseline_samples {
            let sum: Duration = self.window.iter().sum();
            self.baseline = Some(sum / self.window.len() as u32);
        }

        // Check if this request was slow relative to baseline
        if let Some(baseline) = self.baseline {
            if latency > baseline.mul_f64(self.degradation_threshold) {
                self.consecutive_slow += 1;
            } else {
                self.consecutive_slow = 0;
            }
        }
    }

    /// Should auto-protection be activated?
    pub fn should_activate(&self) -> bool {
        !self.active && self.consecutive_slow >= self.slow_trigger
    }

    /// Should auto-protection be deactivated (provider recovered)?
    pub fn should_deactivate(&self) -> bool {
        self.active && self.consecutive_slow == 0
    }

    /// Set whether auto-protection is currently active.
    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    /// Whether auto-protection is currently active.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Current baseline latency, if established.
    pub fn baseline(&self) -> Option<Duration> {
        self.baseline
    }

    /// Number of consecutive slow requests.
    pub fn consecutive_slow_count(&self) -> u32 {
        self.consecutive_slow
    }

    /// Number of latency samples recorded so far.
    pub fn sample_count(&self) -> usize {
        self.window.len()
    }
}

impl Default for ResponsivenessObserver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_baseline_learning() {
        let mut obs = ResponsivenessObserver::new();
        for _ in 0..5 {
            obs.record(Duration::from_millis(100));
        }
        assert!(obs.baseline.is_some());
        assert_eq!(obs.baseline.unwrap(), Duration::from_millis(100));
    }

    #[test]
    fn test_degradation_detection() {
        let mut obs = ResponsivenessObserver::new();
        // Establish baseline at 100ms
        for _ in 0..5 {
            obs.record(Duration::from_millis(100));
        }
        assert!(!obs.should_activate());

        // 3 slow requests (400ms > 100ms * 3.0 = 300ms)
        for _ in 0..3 {
            obs.record(Duration::from_millis(400));
        }
        assert!(obs.should_activate());
    }

    #[test]
    fn test_recovery_detection() {
        let mut obs = ResponsivenessObserver::new();
        for _ in 0..5 {
            obs.record(Duration::from_millis(100));
        }
        for _ in 0..3 {
            obs.record(Duration::from_millis(400));
        }
        obs.set_active(true);

        // One normal request resets consecutive_slow
        obs.record(Duration::from_millis(100));
        assert!(obs.should_deactivate());
    }

    #[test]
    fn test_no_false_trigger_before_baseline() {
        let mut obs = ResponsivenessObserver::new();
        // Only 2 samples, baseline not established
        obs.record(Duration::from_millis(100));
        obs.record(Duration::from_millis(10000));
        assert!(!obs.should_activate());
    }

    /// Window caps at max size (20).
    #[test]
    fn test_window_caps_at_max_size() {
        let mut obs = ResponsivenessObserver::new();
        for i in 0..30 {
            obs.record(Duration::from_millis(100 + i));
        }
        assert_eq!(obs.sample_count(), 20);
    }

    /// Multiple activation/deactivation cycles work correctly.
    #[test]
    fn test_multiple_activation_cycles() {
        let mut obs = ResponsivenessObserver::new();
        // Establish baseline at 100ms
        for _ in 0..5 {
            obs.record(Duration::from_millis(100));
        }

        // Cycle 1: degrade → activate
        for _ in 0..3 {
            obs.record(Duration::from_millis(400));
        }
        assert!(obs.should_activate());
        obs.set_active(true);

        // Recover → deactivate
        obs.record(Duration::from_millis(100));
        assert!(obs.should_deactivate());
        obs.set_active(false);

        // Cycle 2: degrade again → activate again
        for _ in 0..3 {
            obs.record(Duration::from_millis(400));
        }
        assert!(obs.should_activate());
        obs.set_active(true);

        // Recover again
        obs.record(Duration::from_millis(50));
        assert!(obs.should_deactivate());
    }

    /// Latency exactly at threshold (3×baseline) does NOT count as slow.
    #[test]
    fn test_at_threshold_boundary_not_triggered() {
        let mut obs = ResponsivenessObserver::new();
        // Baseline = 100ms, threshold = 3.0 → slow if > 300ms
        for _ in 0..5 {
            obs.record(Duration::from_millis(100));
        }
        // Record exactly 300ms three times (not > 300ms)
        for _ in 0..3 {
            obs.record(Duration::from_millis(300));
        }
        // Should NOT activate (300ms is not > 300ms)
        assert!(!obs.should_activate());
    }

    /// sample_count tracks correctly.
    #[test]
    fn test_sample_count_tracking() {
        let mut obs = ResponsivenessObserver::new();
        assert_eq!(obs.sample_count(), 0);
        obs.record(Duration::from_millis(100));
        assert_eq!(obs.sample_count(), 1);
        for _ in 0..4 {
            obs.record(Duration::from_millis(100));
        }
        assert_eq!(obs.sample_count(), 5);
        assert!(obs.baseline().is_some());
    }
}
