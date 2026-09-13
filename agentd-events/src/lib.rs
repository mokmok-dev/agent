//! The event bus at the core of the choreography-style event flow: components
//! publish events and react to the events published by others; there is no
//! central orchestrator.
//!
//! Events conform to [CloudEvents] 1.0: every event carries the required
//! context attributes (`id`, `source`, `specversion`, `type`) serialized with
//! their spec-defined names, plus the event `data`.
//!
//! [CloudEvents]: https://github.com/cloudevents/spec/blob/v1.0.2/cloudevents/spec.md

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::broadcast;

/// Capacity of the channel buffering events per subscriber before it lags.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// The `CloudEvents` specification version all events conform to.
pub const SPEC_VERSION: &str = "1.0";

/// The `source` attribute assigned to events produced by the daemon itself.
pub const DAEMON_SOURCE: &str = "urn:mokmokd";

/// An event exchanged between agentd components and connected clients.
///
/// The serialized form uses the `CloudEvents` 1.0 attribute names; the `type`
/// attribute is exposed as [`Event::kind`] because `type` is a Rust keyword.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// The `CloudEvents` `id` attribute, generated as a UUID version 7 so
    /// that events order by creation time.
    pub id: String,
    /// The `CloudEvents` `source` attribute identifying where the event
    /// originated.
    pub source: String,
    /// The `CloudEvents` `specversion` attribute.
    pub specversion: String,
    /// The `CloudEvents` `type` attribute.
    #[serde(rename = "type")]
    pub kind: String,
    /// The `CloudEvents` `time` attribute as an RFC 3339 timestamp. Optional in
    /// the specification, so absent timestamps are tolerated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    /// The `CloudEvents` `data` payload.
    #[serde(default)]
    pub data: serde_json::Value,
}

impl Event {
    /// Creates a daemon-produced event with `kind` and `data`, assigning a
    /// fresh `id`, the daemon `source`, the current `specversion`, and the
    /// current UTC `time`.
    ///
    /// The `time` attribute is left unset if the current time cannot be
    /// formatted as RFC 3339 (e.g. a system clock far outside the representable
    /// range); `time` is optional in the specification.
    pub fn new(
        kind: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            source: String::from(DAEMON_SOURCE),
            specversion: String::from(SPEC_VERSION),
            kind: kind.into(),
            time: OffsetDateTime::now_utc().format(&Rfc3339).ok(),
            data,
        }
    }
}

/// Broadcasts [`Event`]s to every subscriber.
///
/// Components publish events through [`EventBus::publish`] and react to the
/// events of other components through [`EventBus::subscribe`]; there is no
/// central orchestrator.
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    /// Creates a bus whose channel buffers up to `capacity` events per
    /// subscriber before the subscriber lags.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publishes `event` to all current subscribers.
    ///
    /// Publishing without subscribers is not an error; the event is dropped
    /// and the condition is logged.
    pub fn publish(
        &self,
        event: Event,
    ) {
        if self.sender.send(event).is_err() {
            tracing::debug!("event dropped because there are no subscribers");
        }
    }

    /// Subscribes to all events published after this call.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(EVENT_CHANNEL_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::{DAEMON_SOURCE, Event, EventBus, SPEC_VERSION};
    use serde_json::json;
    use tokio::sync::broadcast::error::RecvError;

    fn test_event(kind: &str) -> Event {
        Event::new(kind, json!({ "value": 1 }))
    }

    #[test]
    fn new_assigns_cloud_events_attributes() {
        let event = test_event("test.event");

        assert_eq!(event.source, DAEMON_SOURCE);
        assert_eq!(event.specversion, SPEC_VERSION);
        assert_eq!(event.kind, "test.event");
        uuid::Uuid::parse_str(&event.id).expect("id should be a UUID");
        let time = event.time.expect("time should be set");
        time::OffsetDateTime::parse(&time, &time::format_description::well_known::Rfc3339)
            .expect("time should be an RFC 3339 timestamp");
    }

    #[test]
    fn deserializes_minimal_cloud_event_with_optional_attributes_absent() {
        let raw = r#"{
            "id": "018f6b2e-7e5c-7000-8000-000000000000",
            "source": "urn:mokmokd",
            "specversion": "1.0",
            "type": "test.event"
        }"#;

        let event: Event = serde_json::from_str(raw).expect("should deserialize");

        assert_eq!(event.time, None);
        assert_eq!(event.data, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn publish_delivers_events_to_all_subscribers() {
        let bus = EventBus::new(8);
        let mut first = bus.subscribe();
        let mut second = bus.subscribe();
        let event = test_event("test.event");

        bus.publish(event.clone());

        assert_eq!(first.recv().await.ok().as_ref(), Some(&event));
        assert_eq!(second.recv().await.ok().as_ref(), Some(&event));
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_not_an_error() {
        let bus = EventBus::new(8);

        bus.publish(test_event("test.event"));
    }

    #[tokio::test]
    async fn lagged_subscribers_observe_lagged_before_events_resume() {
        let bus = EventBus::new(1);
        let mut subscriber = bus.subscribe();
        let second = test_event("test.second");
        bus.publish(test_event("test.first"));
        bus.publish(second.clone());

        let received = subscriber.recv().await;

        assert!(matches!(received, Err(RecvError::Lagged(1))));
        assert_eq!(subscriber.recv().await.ok().as_ref(), Some(&second));
    }
}
