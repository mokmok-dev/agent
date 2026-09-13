//! The event bus at the core of the choreography-style event flow: components
//! publish events and react to the events published by others; there is no
//! central orchestrator.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Capacity of the channel buffering events per subscriber before it lags.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// An event exchanged between agentd components and connected clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// The event kind, e.g. `error.lagged`.
    pub kind: String,
    /// The event payload.
    pub payload: serde_json::Value,
}

impl Event {
    /// Creates an event with `kind` and `payload`.
    pub fn new(
        kind: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            kind: kind.into(),
            payload,
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
    use super::{Event, EventBus};
    use serde_json::json;
    use tokio::sync::broadcast::error::RecvError;

    fn test_event(kind: &str) -> Event {
        Event::new(kind, json!({ "value": 1 }))
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
        bus.publish(test_event("test.first"));
        bus.publish(test_event("test.second"));

        let received = subscriber.recv().await;

        assert!(matches!(received, Err(RecvError::Lagged(1))));
        assert_eq!(
            subscriber.recv().await.ok().as_ref(),
            Some(&test_event("test.second"))
        );
    }
}
