//! Account admission waits pause the execution budget with a bounded liveness lease.
use std::time::Duration;
use tokio::time::Instant;

#[derive(Default)]
pub(crate) struct Wait {
    until: Option<Instant>,
}

impl Wait {
    pub(crate) fn update(&mut self, waiting: bool, now: Instant, deadline: &mut Instant) {
        if waiting {
            let until = now + Duration::from_secs(15);
            let previous = self.until.unwrap_or(now).max(now);
            *deadline += until.saturating_duration_since(previous);
            self.until = Some(until);
        } else if let Some(until) = self.until.take() {
            *deadline -= until.saturating_duration_since(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_observed_wait_time_is_excluded_and_silence_cannot_pause_forever() {
        let now = Instant::now();
        let original = now + Duration::from_secs(100);
        let mut deadline = original;
        let mut wait = Wait::default();
        wait.update(true, now, &mut deadline);
        assert_eq!(deadline, original + Duration::from_secs(15));
        wait.update(true, now + Duration::from_secs(5), &mut deadline);
        wait.update(false, now + Duration::from_secs(7), &mut deadline);
        assert_eq!(deadline, original + Duration::from_secs(7));
        wait.update(false, now + Duration::from_secs(9), &mut deadline);
        assert_eq!(deadline, original + Duration::from_secs(7));
    }
}
