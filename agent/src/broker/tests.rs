//! Tests for the broker: fan-out, delivery order, and slow-subscriber eviction.

use super::*;

/// An event with a distinctive payload for a given sequence.
fn event(seq: u64) -> Event {
    Event {
        seq,
        timestamp_ns: seq * 1000,
        payload: format!("event-{seq}").into_bytes(),
    }
}

#[test]
fn an_empty_broker_publishes_to_nobody() {
    let mut broker = Broker::default();
    let fan_out = broker.publish(&event(0));
    assert_eq!(fan_out, FanOut::default());
    assert_eq!(broker.subscriber_count(), 0);
}

#[test]
fn a_subscriber_receives_published_events_in_order() {
    let mut broker = Broker::default();
    let mut subscription = broker.subscribe(SubscriberId::new("s1"));

    for seq in 0..5 {
        let fan_out = broker.publish(&event(seq));
        assert_eq!(fan_out.delivered, 1);
        assert!(!fan_out.evicted_any());
    }

    let received: Vec<u64> = std::iter::from_fn(|| subscription.try_recv())
        .map(|event| event.seq)
        .collect();
    assert_eq!(received, [0, 1, 2, 3, 4]);
}

#[test]
fn every_subscriber_receives_every_event() {
    let mut broker = Broker::default();
    let mut first = broker.subscribe(SubscriberId::new("first"));
    let mut second = broker.subscribe(SubscriberId::new("second"));
    let mut third = broker.subscribe(SubscriberId::new("third"));

    let fan_out = broker.publish(&event(7));
    assert_eq!(fan_out.delivered, 3);

    for subscription in [&mut first, &mut second, &mut third] {
        let received = subscription.try_recv().expect("each gets the event");
        assert_eq!(received.seq, 7);
        assert!(subscription.try_recv().is_none(), "only once");
    }
}

#[test]
fn a_full_queue_evicts_the_slow_subscriber_and_does_not_block() {
    // Capacity two: a subscriber that reads each event fits; one that does not
    // overflows on the third publish.
    let mut broker = Broker::new(2);
    let slow = broker.subscribe(SubscriberId::new("slow"));
    let mut fast = broker.subscribe(SubscriberId::new("fast"));

    broker.publish(&event(0));
    assert_eq!(fast.try_recv().map(|event| event.seq), Some(0));
    broker.publish(&event(1));
    assert_eq!(fast.try_recv().map(|event| event.seq), Some(1));
    // Fast drained both, so only slow's full queue overflows here.
    let fan_out = broker.publish(&event(2));

    assert_eq!(fan_out.delivered, 1, "only the fast subscriber is served");
    assert_eq!(fan_out.evicted, [SubscriberId::new("slow")]);
    assert!(fan_out.evicted_any());

    // The slow subscriber is removed and its close code is available.
    assert_eq!(broker.subscriber_count(), 1);
    assert!(slow.is_evicted());
    assert_eq!(slow.close_code(), Some(CloseCode::SlowConsumer));

    // The fast subscriber kept every event and is still registered.
    assert!(!fast.is_evicted());
    assert_eq!(fast.try_recv().map(|event| event.seq), Some(2));
    assert!(fast.try_recv().is_none());
}

#[test]
fn draining_the_queue_keeps_a_subscriber_alive() {
    let mut broker = Broker::new(1);
    let mut subscription = broker.subscribe(SubscriberId::new("diligent"));

    // Read each event before the next arrives, so the queue never overflows.
    for seq in 0..10 {
        broker.publish(&event(seq));
        assert_eq!(subscription.try_recv().map(|event| event.seq), Some(seq));
    }
    assert!(!subscription.is_evicted());
    assert_eq!(broker.subscriber_count(), 1);
}

#[test]
fn a_zero_capacity_broker_still_delivers() {
    // A zero-capacity channel can never accept a `try_send`, so the broker
    // floors capacity at one instead of evicting everyone.
    let mut broker = Broker::new(0);
    assert_eq!(broker.capacity(), 1);
    let mut subscription = broker.subscribe(SubscriberId::new("s"));
    let fan_out = broker.publish(&event(0));
    assert_eq!(fan_out.delivered, 1);
    assert_eq!(subscription.try_recv().map(|event| event.seq), Some(0));
}

#[test]
fn the_capacity_accessor_reports_the_configured_value() {
    assert_eq!(Broker::new(64).capacity(), 64);
    assert_eq!(Broker::default().capacity(), DEFAULT_QUEUE_CAPACITY);
}

#[test]
fn unsubscribing_an_unknown_id_reports_false() {
    let mut broker = Broker::default();
    let subscription = broker.subscribe(SubscriberId::new("known"));
    assert!(broker.unsubscribe(subscription.id()));
    assert!(!broker.unsubscribe(subscription.id()));
    assert_eq!(broker.subscriber_count(), 0);
}

#[test]
fn a_dropped_receiver_is_removed_on_the_next_publish() {
    let mut broker = Broker::default();
    let subscription = broker.subscribe(SubscriberId::new("gone"));
    drop(subscription);

    let fan_out = broker.publish(&event(0));
    assert_eq!(fan_out.delivered, 0);
    assert!(!fan_out.evicted_any());
    assert_eq!(broker.subscriber_count(), 0, "the closed sender is reaped");
}

#[test]
fn resubscribing_the_same_id_evicts_the_previous_reader() {
    let mut broker = Broker::default();
    let previous = broker.subscribe(SubscriberId::new("s"));
    let mut current = broker.subscribe(SubscriberId::new("s"));

    assert!(
        previous.is_evicted(),
        "the replaced subscription is evicted"
    );
    assert!(!current.is_evicted());
    assert_eq!(broker.subscriber_count(), 1, "only one registration");

    broker.publish(&event(0));
    assert_eq!(current.try_recv().map(|event| event.seq), Some(0));
}

#[test]
fn subscribe_new_generates_distinct_ids() {
    let mut broker = Broker::default();
    let first = broker.subscribe_new();
    let second = broker.subscribe_new();
    assert_ne!(first.id(), second.id());
    assert_eq!(broker.subscriber_count(), 2);
}

#[test]
fn queued_reports_the_unread_depth() {
    let mut broker = Broker::default();
    let mut subscription = broker.subscribe(SubscriberId::new("s"));
    assert_eq!(subscription.queued(), 0);
    broker.publish(&event(0));
    broker.publish(&event(1));
    assert_eq!(subscription.queued(), 2);
    let _ = subscription.try_recv();
    assert_eq!(subscription.queued(), 1);
}

#[test]
fn a_subscriber_id_displays_its_string() {
    let id = SubscriberId::new("alpha");
    assert_eq!(id.as_str(), "alpha");
    assert_eq!(id.to_string(), "alpha");
    assert_eq!(SubscriberId::from("alpha"), id);
}
