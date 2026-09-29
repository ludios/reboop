// Model-output: Claude Opus 5.5

//! Deadlines, and retrying until one passes.

use anyhow::{Error, Result};
use std::fmt;
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

/// An error that trying again won't fix, which makes [`retry`] give up.
#[derive(Debug)]
pub struct Permanent(pub String);

impl fmt::Display for Permanent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Permanent {}

/// Calls `attempt` until it succeeds, starting attempts at most once per
/// `interval`.  Gives up with the last error if it's [`Permanent`] or when
/// the next attempt would start after `deadline`.  Otherwise, each error is
/// passed to `on_retry` before waiting to try again.
///
/// `attempt` is given the overall deadline, which it should not exceed.
pub fn retry<T>(
    deadline: Deadline,
    interval: Duration,
    mut attempt: impl FnMut(Deadline) -> Result<T>,
    mut on_retry: impl FnMut(&Error),
) -> Result<T> {
    loop {
        let started = Instant::now();
        let error = match attempt(deadline) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if error.downcast_ref::<Permanent>().is_some() {
            return Err(error);
        }
        let next = Deadline(started + interval);
        if next > deadline || deadline.has_passed() {
            return Err(error.context("gave up retrying at the deadline"));
        }
        debug!("attempt failed, retrying in {:?}: {error:#}", next.remaining());
        on_retry(&error);
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
        let mut retried = Vec::new();
        let value = retry(Deadline::after(Duration::from_secs(5)), Duration::ZERO, |_| {
            calls += 1;
            if calls < 3 { bail!("not yet") } else { Ok(calls) }
        }, |error| retried.push(error.to_string()))
        .unwrap();
        assert_eq!(value, 3);
        assert_eq!(retried, ["not yet", "not yet"]);
    }

    #[test]
    fn retry_gives_up_on_permanent_errors() {
        let mut calls = 0;
        let error = retry(Deadline::after(Duration::from_secs(5)), Duration::ZERO, |_| -> Result<()> {
            calls += 1;
            Err(Permanent("no".into()).into())
        }, |_| panic!("retried a permanent error"))
        .unwrap_err();
        assert_eq!((calls, error.to_string()), (1, "no".to_string()));
    }

    #[test]
    fn retry_gives_up_with_last_error() {
        let mut calls = 0;
        let error = retry(Deadline::after(Duration::from_millis(50)), Duration::from_millis(20), |_| -> Result<()> {
            calls += 1;
            bail!("failure {calls}")
        }, |_| ())
        .unwrap_err();
        assert!((2..=4).contains(&calls), "{calls} calls");
        assert!(format!("{error:#}").contains(&format!("failure {calls}")));
    }
}
