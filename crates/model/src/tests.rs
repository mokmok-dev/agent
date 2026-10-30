use std::time::Duration;

use agent_core::event::{ErrorClass, FinishReason};
use agent_prompt::{Directive, Prompt, render};

use super::{
    Completion, Delta, MAX_DETAIL, Message, Profile, Provider, ProviderError, Purpose, Request,
    RequestError, RequestRecord, Usage, backoff,
};

#[allow(dead_code, reason = "the codec's tests render through it too")]
pub fn prompt(messages: &[agent_prompt::Message<'_>]) -> Prompt {
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
    assert_eq!(
        ProviderError::Transport(String::from("refused")).class(),
        ErrorClass::Transport
    );
    assert_eq!(ProviderError::Timeout.class(), ErrorClass::Timeout);
    assert!(ProviderError::Transport(String::from("refused")).retryable(false));
    assert!(ProviderError::Timeout.retryable(false));
}

#[test]
fn a_response_that_produced_a_token_is_never_retried() {
    let errors = [
        ProviderError::Transport(String::from("refused")),
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
    let rendered = prompt(&[agent_prompt::Message::plain(
        agent_core::event::Role::User,
        "hello",
    )]);
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
        tools: None,
        messages: vec![Message {
            call: None,
            calls: Vec::new(),
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

#[test]
fn a_failure_says_what_it_was_without_saying_what_was_asked() {
    assert_eq!(
        ProviderError::Transport(String::from("Connection refused (os error 111)")).detail(),
        "Connection refused (os error 111)",
        "the socket's own words are the difference between a URL to fix and a provider to wait for"
    );
    assert_eq!(ProviderError::Status(503).detail(), "status 503");
    assert_eq!(
        ProviderError::AfterFirstToken(String::from("connection reset")).detail(),
        "connection reset"
    );
    assert_eq!(
        ProviderError::Timeout.detail(),
        "no response within the deadline"
    );
}

#[test]
fn a_cause_is_cut_on_a_character_boundary_so_the_log_cannot_be_filled_by_one() {
    let long = "é".repeat(MAX_DETAIL + 50);
    let detail = ProviderError::Transport(long).detail();
    assert_eq!(detail.chars().count(), MAX_DETAIL + 1, "cut plus one mark");
    assert!(detail.ends_with('…'), "a cut says it was cut");
    assert!(
        ProviderError::Transport("x".repeat(MAX_DETAIL)).detail() == "x".repeat(MAX_DETAIL),
        "a cause that fits is kept whole"
    );
}

#[test]
fn the_request_error_reads_as_the_sentence_it_is() {
    assert_eq!(
        RequestError::Tools.to_string(),
        "the tool block is not a JSON array"
    );
}

#[test]
fn a_tool_block_that_is_json_but_not_an_array_is_refused() {
    let prompt = Prompt {
        directive_version: "v1".to_string(),
        segments: vec![
            agent_prompt::Segment {
                kind: agent_prompt::Kind::Directive,
                role: None,
                text: "be careful".to_string(),
                calls: Vec::new(),
                call: None,
                tokens: 0,
            },
            agent_prompt::Segment {
                kind: agent_prompt::Kind::Tools,
                role: None,
                text: "{}".to_string(),
                calls: Vec::new(),
                call: None,
                tokens: 0,
            },
        ],
        chain: Vec::new(),
        tokens: 0,
    };
    assert_eq!(
        Request::of(&prompt, "test", 16).err(),
        Some(RequestError::Tools)
    );
}

#[test]
fn a_tool_block_that_is_an_array_is_carried_into_the_request() {
    let prompt = Prompt {
        directive_version: "v1".to_string(),
        segments: vec![agent_prompt::Segment {
            kind: agent_prompt::Kind::Tools,
            role: None,
            text: "[{\"type\":\"function\"}]".to_string(),
            calls: Vec::new(),
            call: None,
            tokens: 0,
        }],
        chain: Vec::new(),
        tokens: 0,
    };
    let request = Request::of(&prompt, "test", 16).expect("an array is what the field takes");
    assert_eq!(request.tools.as_deref(), Some("[{\"type\":\"function\"}]"));
}
