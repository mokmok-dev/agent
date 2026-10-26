use std::time::Duration;

use agent_core::event::{ErrorClass, FinishReason};
use agent_prompt::{Directive, Prompt, render};

use super::{
    Completion, Delta, Message, Profile, Provider, ProviderError, Purpose, Request, RequestRecord,
    Usage, backoff,
};

fn prompt(messages: &[(agent_core::event::Role, &str)]) -> Prompt {
    let directive = Directive {
        version: "v1",
        text: "You are a careful agent.",
    };
    render(&directive, &[], messages)
}

#[test]
fn the_first_backoff_stays_under_half_a_second() {
    assert_eq!(backoff(1, 0), Some(Duration::ZERO));
    assert_eq!(backoff(1, 499), Some(Duration::from_millis(499)));
}

#[test]
fn the_second_backoff_stays_under_a_second() {
    assert_eq!(backoff(2, 0), Some(Duration::ZERO));
    assert_eq!(backoff(2, 999), Some(Duration::from_millis(999)));
}

#[test]
fn a_draw_larger_than_the_ceiling_wraps_inside_it() {
    assert_eq!(backoff(1, 500), Some(Duration::from_millis(0)));
    assert_eq!(backoff(2, 1000), Some(Duration::from_millis(0)));
    assert_eq!(backoff(2, 1001), Some(Duration::from_millis(1)));
}

#[test]
fn there_is_no_wait_after_the_last_attempt() {
    assert_eq!(backoff(3, 0), None);
    assert_eq!(backoff(4, 0), None);
}

#[test]
fn there_is_no_wait_before_the_first_attempt() {
    assert_eq!(backoff(0, 0), None);
}

#[test]
fn a_rate_limit_is_retryable_and_named() {
    let error = ProviderError::Status(429);
    assert_eq!(error.class(), ErrorClass::RateLimited);
    assert!(error.retryable(false));
}

#[test]
fn a_payload_too_large_is_a_context_length_and_is_not_retryable() {
    let error = ProviderError::Status(413);
    assert_eq!(error.class(), ErrorClass::ContextLength);
    assert!(!error.retryable(false));
}

#[test]
fn the_bounds_of_each_status_range_land_on_the_right_class() {
    assert_eq!(
        ProviderError::Status(400).class(),
        ErrorClass::InvalidRequest
    );
    assert_eq!(
        ProviderError::Status(499).class(),
        ErrorClass::InvalidRequest
    );
    assert_eq!(ProviderError::Status(500).class(), ErrorClass::ServerError);
    assert_eq!(ProviderError::Status(599).class(), ErrorClass::ServerError);
    assert_eq!(ProviderError::Status(300).class(), ErrorClass::Unknown);
    assert_eq!(ProviderError::Status(200).class(), ErrorClass::Unknown);
}

#[test]
fn every_5xx_and_a_429_is_retryable() {
    for code in [429, 500, 502, 599] {
        assert!(ProviderError::Status(code).retryable(false), "{code}");
    }
}

#[test]
fn a_4xx_that_is_not_a_429_is_not_retryable() {
    for code in [400, 401, 403, 404, 413] {
        assert!(!ProviderError::Status(code).retryable(false), "{code}");
    }
}

#[test]
fn transport_and_timeout_failures_are_retryable() {
    assert_eq!(ProviderError::Transport.class(), ErrorClass::Transport);
    assert_eq!(ProviderError::Timeout.class(), ErrorClass::Timeout);
    assert!(ProviderError::Transport.retryable(false));
    assert!(ProviderError::Timeout.retryable(false));
}

#[test]
fn a_response_that_produced_a_token_is_never_retried() {
    let errors = [
        ProviderError::Transport,
        ProviderError::Timeout,
        ProviderError::Status(429),
        ProviderError::Status(500),
    ];
    for error in errors {
        assert!(!error.retryable(true), "{error:?}");
    }
}

#[test]
fn a_stream_that_stopped_part_way_is_unknown_and_not_retryable() {
    let error = ProviderError::AfterFirstToken("connection reset".to_string());
    assert_eq!(error.class(), ErrorClass::Unknown);
    assert!(!error.retryable(false));
}

#[test]
fn a_record_takes_the_last_chain_entry_as_its_prefix() {
    let rendered = prompt(&[(agent_core::event::Role::User, "hello")]);
    let record = RequestRecord::of(&rendered, Purpose::Turn).expect("records");
    assert_eq!(record.prefix, *rendered.chain.last().expect("a chain"));
    assert_eq!(record.segments, 3);
    assert_eq!(record.tokens, rendered.tokens);
    assert!(!record.warm);
}

#[test]
fn a_warming_request_is_marked() {
    let rendered = prompt(&[]);
    let record = RequestRecord::of(&rendered, Purpose::Warm).expect("records");
    assert!(record.warm);
}

#[test]
fn an_empty_prompt_cannot_be_recorded() {
    let empty = Prompt {
        directive_version: "v1".to_string(),
        segments: Vec::new(),
        chain: Vec::new(),
        tokens: 0,
    };
    assert_eq!(RequestRecord::of(&empty, Purpose::Turn), None);
}

struct Scripted {
    pieces: Vec<&'static str>,
    outcome: Option<ProviderError>,
}

impl Provider for Scripted {
    fn profile(&self) -> Profile {
        Profile {
            id: "scripted".to_string(),
            context_window: 1000,
            caches_prefixes: true,
            cache_ttl: Some(Duration::from_secs(300)),
            reports_cached_tokens: true,
        }
    }

    fn call(
        &mut self,
        _request: &Request,
        delta: &mut dyn FnMut(Delta<'_>),
    ) -> Result<Completion, ProviderError> {
        for piece in std::mem::take(&mut self.pieces) {
            delta(Delta::Text(piece));
        }
        self.outcome.take().map_or(
            Ok(Completion {
                calls: Vec::new(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    cached_tokens: Some(8),
                    estimated: false,
                },
                finish: Some(FinishReason::Stop),
            }),
            Err,
        )
    }
}

fn request() -> Request {
    Request {
        model: "test".to_string(),
        directive: "be careful".to_string(),
        tools: "4:read17:{\"path\":\"string\"}".to_string(),
        messages: vec![Message {
            role: agent_core::event::Role::User,
            content: "hello".to_string(),
        }],
        max_output_tokens: 16,
        input_tokens_estimate: 40,
    }
}

#[test]
fn a_provider_reports_its_text_and_its_usage() {
    let mut provider = Scripted {
        pieces: vec!["he", "llo"],
        outcome: None,
    };
    let mut seen = String::new();
    let usage = provider
        .call(&request(), &mut |delta| match delta {
            Delta::Text(text) => seen.push_str(text),
        })
        .expect("succeeds");
    assert_eq!(seen, "hello");
    assert_eq!(usage.usage.output_tokens, 2);
    assert_eq!(usage.usage.cached_tokens, Some(8));
    assert!(!usage.usage.estimated);
}

#[test]
fn a_provider_that_stops_mid_stream_reports_it() {
    let error = ProviderError::AfterFirstToken("reset".to_string());
    let mut provider = Scripted {
        pieces: vec!["par"],
        outcome: Some(error.clone()),
    };
    let mut seen = 0;
    let result = provider.call(&request(), &mut |_| seen += 1);
    assert_eq!(result, Err(error));
    assert_eq!(seen, 1, "the delta arrived before the failure");
}

#[test]
fn a_profile_that_does_not_cache_says_so() {
    let provider = Scripted {
        pieces: Vec::new(),
        outcome: None,
    };
    let profile = provider.profile();
    assert!(profile.caches_prefixes);
    assert_eq!(profile.context_window, 1000);
}
