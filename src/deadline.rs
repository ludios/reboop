// Model-output: Claude Opus 5.5

//! Deadlines, and retrying until one passes.

use anyhow::Result;
use std::thread::sleep;
use std::time::{Duration, Instant};
use tracing::debug;

/// A point in time by which something has to be done.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Deadline(Instant);

impl Deadline {
    pub fn after(duration: Duration) -> Deadline {
        Deadline(Instant::now() + duration)
    }

    /// The time left, which is zero once the deadline has passed.
    pub fn remaining(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    pub fn has_passed(self) -> bool {
        self.remaining().is_zero()
    }

    /// `self`, or `duration` from now if that's sooner.
    pub fn at_most(self, duration: Duration) -> Deadline {
        self.min(Deadline::after(duration))
    }
}

/// Calls `attempt` until it succeeds, starting attempts at most once per
/// `interval`, and gives up with the last error when the next attempt would
/// start after `deadline`.
///
/// `attempt` is given the overall deadline, which it should not exceed.
pub fn retry<T>(
    deadline: Deadline,
    interval: Duration,
    mut attempt: impl FnMut(Deadline) -> Result<T>,
) -> Result<T> {
    loop {
        let started = Instant::now();
        let error = match attempt(deadline) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let next = Deadline(started + interval);
        if next > deadline {
            return Err(error.context("gave up retrying at the deadline"));
        }
        debug!("attempt failed, retrying in {:?}: {error:#}", next.remaining());
        sleep(next.remaining());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    #[test]
    fn retry_returns_first_success() {
        let mut calls = 0;
        let value = retry(Deadline::after(Duration::from_secs(5)), Duration::ZERO, |_| {
            calls += 1;
            if calls < 3 { bail!("not yet") } else { Ok(calls) }
        })
        .unwrap();
        assert_eq!(value, 3);
    }

    #[test]
    fn retry_gives_up_with_last_error() {
        let mut calls = 0;
        let error = retry(Deadline::after(Duration::from_millis(50)), Duration::from_millis(20), |_| -> Result<()> {
            calls += 1;
            bail!("failure {calls}")
        })
        .unwrap_err();
        assert!((2..=4).contains(&calls), "{calls} calls");
        assert!(format!("{error:#}").contains(&format!("failure {calls}")));
    }
}
