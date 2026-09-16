use agentd_events::Event;

/// Decides whether a node cares about an event it has received.
///
/// A node subscribes to the whole event log (the daemon broadcasts every event
/// to every subscriber) and uses this to select the events it applies to its
/// projection. An event that is not of interest is skipped: the node still
/// advances its checkpoint past it, so a restart does not re-read it.
pub trait Interest: Send + Sync + 'static {
    /// Returns `true` when the node should apply `event` to its projection.
    fn interested(
        &self,
        event: &Event,
    ) -> bool;
}

impl<F> Interest for F
where
    F: Fn(&Event) -> bool + Send + Sync + 'static,
{
    fn interested(
        &self,
        event: &Event,
    ) -> bool {
        self(event)
    }
}

/// Selects events whose `CloudEvents` `type` starts with one of the given
/// prefixes.
///
/// An empty set of prefixes matches every event, which is the same as
/// [`TypePrefixes::default`].
#[derive(Debug, Clone, Default)]
pub struct TypePrefixes {
    prefixes: Vec<String>,
}

impl TypePrefixes {
    /// Creates a filter matching events whose `type` starts with any of
    /// `prefixes`. An empty iterator matches every event.
    pub fn new<I, S>(prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            prefixes: prefixes.into_iter().map(Into::into).collect(),
        }
    }
}

impl Interest for TypePrefixes {
    fn interested(
        &self,
        event: &Event,
    ) -> bool {
        self.prefixes.is_empty()
            || self
                .prefixes
                .iter()
                .any(|prefix| event.r#type.starts_with(prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::{Interest, TypePrefixes};
    use agentd_events::Event;
    use serde_json::json;

    fn event(r#type: &str) -> Event {
        Event::new(r#type, json!({}))
    }

    #[test]
    fn empty_prefixes_match_every_event() {
        let filter = TypePrefixes::default();

        assert!(filter.interested(&event("anything")));
        assert!(filter.interested(&event("sandbox.permission.granted")));
    }

    #[test]
    fn prefixes_match_by_type_prefix() {
        let filter = TypePrefixes::new(["sandbox."]);

        assert!(filter.interested(&event("sandbox.exec.completed")));
        assert!(!filter.interested(&event("test.event")));
    }

    #[test]
    fn closures_are_interests() {
        let filter = |event: &Event| event.r#type == "test.keep";

        assert!(filter.interested(&event("test.keep")));
        assert!(!filter.interested(&event("test.drop")));
    }
}
