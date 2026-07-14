use std::time::{Duration, Instant};

const DEFAULT_SUMMARY_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeDecision {
    Started,
    Suppressed,
    Summary {
        total: u64,
        since_last_summary: u64,
        suppressed: u64,
        duration: Duration,
    },
    Recovered {
        total: u64,
        suppressed: u64,
        duration: Duration,
    },
    Healthy,
}

#[derive(Debug, Clone)]
pub struct FailureEpisode {
    summary_interval: Duration,
    started_at: Option<Instant>,
    last_summary_at: Option<Instant>,
    total: u64,
    suppressed_total: u64,
    suppressed_since_summary: u64,
}

impl FailureEpisode {
    #[must_use]
    pub fn new(summary_interval: Duration) -> Self {
        assert!(
            !summary_interval.is_zero(),
            "summary interval must be positive"
        );
        Self {
            summary_interval,
            started_at: None,
            last_summary_at: None,
            total: 0,
            suppressed_total: 0,
            suppressed_since_summary: 0,
        }
    }

    pub fn record_failure(&mut self, now: Instant) -> EpisodeDecision {
        let Some(started_at) = self.started_at else {
            self.started_at = Some(now);
            self.last_summary_at = Some(now);
            self.total = 1;
            return EpisodeDecision::Started;
        };

        self.total = self.total.saturating_add(1);
        let last_summary_at = self.last_summary_at.unwrap_or(started_at);
        if elapsed(now, last_summary_at) >= self.summary_interval {
            let suppressed = self.suppressed_since_summary;
            let since_last_summary = suppressed.saturating_add(1);
            self.suppressed_since_summary = 0;
            self.last_summary_at = Some(now);
            return EpisodeDecision::Summary {
                total: self.total,
                since_last_summary,
                suppressed,
                duration: elapsed(now, started_at),
            };
        }

        self.suppressed_total = self.suppressed_total.saturating_add(1);
        self.suppressed_since_summary = self.suppressed_since_summary.saturating_add(1);
        EpisodeDecision::Suppressed
    }

    pub fn record_success(&mut self, now: Instant) -> EpisodeDecision {
        let Some(started_at) = self.started_at else {
            return EpisodeDecision::Healthy;
        };
        let decision = EpisodeDecision::Recovered {
            total: self.total,
            suppressed: self.suppressed_total,
            duration: elapsed(now, started_at),
        };
        self.reset();
        decision
    }

    pub fn reset(&mut self) {
        self.started_at = None;
        self.last_summary_at = None;
        self.total = 0;
        self.suppressed_total = 0;
        self.suppressed_since_summary = 0;
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.started_at.is_some()
    }
}

impl Default for FailureEpisode {
    fn default() -> Self {
        Self::new(DEFAULT_SUMMARY_INTERVAL)
    }
}

fn elapsed(now: Instant, earlier: Instant) -> Duration {
    now.checked_duration_since(earlier).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_suppresses_summarizes_and_recovers_episode() {
        let start = Instant::now();
        let mut episode = FailureEpisode::new(Duration::from_secs(60));

        assert_eq!(episode.record_failure(start), EpisodeDecision::Started);
        assert_eq!(
            episode.record_failure(start + Duration::from_secs(3)),
            EpisodeDecision::Suppressed
        );
        assert_eq!(
            episode.record_failure(start + Duration::from_secs(59)),
            EpisodeDecision::Suppressed
        );
        assert_eq!(
            episode.record_failure(start + Duration::from_secs(60)),
            EpisodeDecision::Summary {
                total: 4,
                since_last_summary: 3,
                suppressed: 2,
                duration: Duration::from_secs(60),
            }
        );
        assert_eq!(
            episode.record_failure(start + Duration::from_secs(120)),
            EpisodeDecision::Summary {
                total: 5,
                since_last_summary: 1,
                suppressed: 0,
                duration: Duration::from_secs(120),
            }
        );
        assert_eq!(
            episode.record_success(start + Duration::from_secs(123)),
            EpisodeDecision::Recovered {
                total: 5,
                suppressed: 2,
                duration: Duration::from_secs(123),
            }
        );
        assert_eq!(
            episode.record_success(start + Duration::from_secs(124)),
            EpisodeDecision::Healthy
        );
    }

    #[test]
    fn recovery_resets_for_a_new_episode() {
        let start = Instant::now();
        let mut episode = FailureEpisode::default();

        assert_eq!(episode.record_failure(start), EpisodeDecision::Started);
        assert!(episode.is_active());
        assert!(matches!(
            episode.record_success(start + Duration::from_secs(1)),
            EpisodeDecision::Recovered { total: 1, .. }
        ));
        assert!(!episode.is_active());
        assert_eq!(
            episode.record_failure(start + Duration::from_secs(2)),
            EpisodeDecision::Started
        );
    }

    #[test]
    #[should_panic(expected = "summary interval must be positive")]
    fn rejects_zero_summary_interval() {
        let _ = FailureEpisode::new(Duration::ZERO);
    }
}
