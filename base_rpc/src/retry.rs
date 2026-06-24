use std::time::Duration;

use base::rand::Rng;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub multiplier: f64,
    pub jitter_ratio: f64,
    pub max_attempts: Option<u32>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(30),
            multiplier: 2.0,
            jitter_ratio: 0.2,
            max_attempts: None,
        }
    }
}

impl RetryPolicy {
    pub fn delay(&self, attempt: u32) -> Duration {
        let base = self.delay_without_jitter(attempt);
        if self.jitter_ratio <= 0.0 || base.is_zero() {
            return base;
        }
        let jitter = base.as_secs_f64() * self.jitter_ratio.clamp(0.0, 1.0);
        let delta = base::rand::thread_rng().gen_range(-jitter..=jitter);
        Duration::from_secs_f64((base.as_secs_f64() + delta).max(0.0))
    }

    pub fn delay_without_jitter(&self, attempt: u32) -> Duration {
        let exponent = i32::try_from(attempt.saturating_sub(1)).unwrap_or(i32::MAX);
        let seconds = self.initial_delay.as_secs_f64() * self.multiplier.max(1.0).powi(exponent);
        Duration::from_secs_f64(seconds.min(self.max_delay.as_secs_f64()))
    }

    pub fn permits(&self, attempt: u32) -> bool {
        self.max_attempts.is_none_or(|max| attempt <= max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_exponential_delay() {
        let policy = RetryPolicy {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
            multiplier: 2.0,
            jitter_ratio: 0.0,
            max_attempts: Some(3),
        };
        assert_eq!(policy.delay_without_jitter(1), Duration::from_secs(1));
        assert_eq!(policy.delay_without_jitter(3), Duration::from_secs(4));
        assert_eq!(policy.delay_without_jitter(5), Duration::from_secs(5));
        assert!(policy.permits(3));
        assert!(!policy.permits(4));
    }
}
