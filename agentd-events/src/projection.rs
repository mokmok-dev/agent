//! Read models derived from the event log.
//!
//! The log is the source of truth; a projection is state derived from it by
//! applying events in [`Seq`] order. A projection records the last position it
//! applied, so startup can [`catch_up`] from where it left off.
//!
//! Rebuilding needs no separate operation: construct a fresh projection (with
//! an [`applied_seq`](Projection::applied_seq) of zero) and catch it up over the
//! whole log.

use crate::LogEntry;
use crate::log::{EventLog, LogError, Seq};
use thiserror::Error;

/// A read model built by applying the event log in order.
///
/// Implementors own their state and the checkpoint of the last applied
/// position. [`apply`](Projection::apply) must be deterministic and idempotent
/// per `seq`, so replaying the same range yields the same state.
pub trait Projection {
    /// The error returned when an event cannot be applied.
    type Error: std::error::Error + 'static;

    /// The position of the last event this projection has applied, or zero.
    fn applied_seq(&self) -> Seq;

    /// Applies `recorded` and advances the checkpoint to its position.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] when the projection cannot apply the event.
    fn apply(
        &mut self,
        recorded: LogEntry,
    ) -> Result<(), Self::Error>;
}

/// Errors returned by [`catch_up`].
#[derive(Debug, Error)]
pub enum ProjectionError<E>
where
    E: std::error::Error + 'static,
{
    /// Reading the log failed.
    #[error(transparent)]
    Log(#[from] LogError),
    /// Applying an event to the projection failed.
    #[error("the projection failed to apply an event: {0}")]
    Apply(#[source] E),
}

/// Applies every log entry the projection has not seen yet, in order.
///
/// Starts after [`Projection::applied_seq`], so a projection loaded from its
/// checkpoint resumes exactly where it stopped.
///
/// # Errors
///
/// Returns [`ProjectionError::Log`] if the log cannot be read and
/// [`ProjectionError::Apply`] if the projection rejects an event.
pub fn catch_up<P>(
    log: &EventLog,
    projection: &mut P,
) -> Result<(), ProjectionError<P::Error>>
where
    P: Projection,
{
    let from = projection.applied_seq().saturating_add(1);
    for entry in log.read_from(from)? {
        projection.apply(entry?).map_err(ProjectionError::Apply)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Projection, catch_up};
    use crate::Event;
    use crate::LogEntry;
    use crate::log::{EventLog, Seq};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::convert::Infallible;

    /// A throwaway projection counting events per type.
    #[derive(Default)]
    struct TypeCounts {
        counts: BTreeMap<String, u64>,
        applied: Seq,
    }

    impl Projection for TypeCounts {
        type Error = Infallible;

        fn applied_seq(&self) -> Seq {
            self.applied
        }

        fn apply(
            &mut self,
            recorded: LogEntry,
        ) -> Result<(), Self::Error> {
            *self.counts.entry(recorded.event.r#type).or_default() += 1;
            self.applied = recorded.seq;
            Ok(())
        }
    }

    #[tokio::test]
    async fn catch_up_applies_every_entry_and_advances_the_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");
        let log = EventLog::open(&path).expect("log should open");
        for index in 0..2 {
            log.publish(Event::new("test.a", json!({ "index": index })))
                .await
                .expect("publish should succeed");
        }
        log.publish(Event::new("test.b", json!({})))
            .await
            .expect("publish should succeed");

        let mut projection = TypeCounts::default();
        catch_up(&log, &mut projection).expect("catch up should succeed");

        assert_eq!(projection.applied, 3);
        assert_eq!(projection.counts.get("test.a"), Some(&2));
        assert_eq!(projection.counts.get("test.b"), Some(&1));
    }

    #[tokio::test]
    async fn catch_up_resumes_from_the_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");
        let log = EventLog::open(&path).expect("log should open");
        log.publish(Event::new("test.a", json!({})))
            .await
            .expect("publish should succeed");

        let mut projection = TypeCounts::default();
        catch_up(&log, &mut projection).expect("first catch up should succeed");

        log.publish(Event::new("test.a", json!({})))
            .await
            .expect("publish should succeed");
        catch_up(&log, &mut projection).expect("second catch up should succeed");

        assert_eq!(projection.applied, 2);
        assert_eq!(projection.counts.get("test.a"), Some(&2));
    }

    #[tokio::test]
    async fn a_fresh_projection_rebuilds_the_whole_log() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");
        let log = EventLog::open(&path).expect("log should open");
        for _ in 0..3 {
            log.publish(Event::new("test.a", json!({})))
                .await
                .expect("publish should succeed");
        }

        let mut projection = TypeCounts::default();
        catch_up(&log, &mut projection).expect("rebuild should succeed");

        assert_eq!(projection.applied, 3);
        assert_eq!(projection.counts.get("test.a"), Some(&3));
    }
}
