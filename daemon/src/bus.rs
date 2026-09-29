//! The daemon's use of the bus client, and the sandbox's `Publisher` bridge.
//!
//! The client itself lives with the bus, in [`agent::client`], because the daemon
//! is not its only caller: the confined agent holds one too. This module keeps the
//! parts that are the daemon's, which are the bridge from the sandbox's
//! [`Publisher`](sandbox::egress::Publisher) seam to a real publish, and the
//! mapping from a sandbox-authored event to the producer half of an envelope.
//! Everything else is re-exported, so a caller names one path.

use agent::cloudevent::Incoming;

pub use agent::client::{
    Client as BusClient, Config, Error, Handler, Receipt, authored, mint_token,
};

/// The daemon's implementation of the sandbox's `Publisher` seam.
///
/// The sandbox authors an event and the bridge publishes it; the sandbox never
/// holds a bus handle. Ids are ULIDs, matching the ids the bus assigns.
#[derive(Debug)]
pub struct BusPublisher {
    client: std::sync::Arc<BusClient>,
}

impl BusPublisher {
    /// A publisher over `client`.
    #[must_use]
    pub const fn new(client: std::sync::Arc<BusClient>) -> Self {
        Self { client }
    }
}

impl sandbox::egress::Publisher for BusPublisher {
    fn publish(
        &self,
        event: sandbox::events::Event,
    ) {
        // The seam returns nothing, so a failure cannot propagate. Log it: a
        // refused publish means the proxy asked a question the log will not show,
        // which an operator must see.
        if let Err(error) = self.client.publish(to_incoming(event)) {
            tracing::error!(%error, "could not publish a sandbox event");
        }
    }

    fn next_request_id(&self) -> sandbox::egress::RequestId {
        sandbox::egress::RequestId::new(mint_token())
    }
}

/// Map a sandbox-authored event to the producer half of an envelope.
#[must_use]
pub fn to_incoming(event: sandbox::events::Event) -> Incoming {
    authored(event.ty, event.subject, event.data, event.traceparent)
}
