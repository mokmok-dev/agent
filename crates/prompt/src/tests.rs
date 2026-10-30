use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use agent_core::event::Role;

fn msg(
    role: Role,
    text: &str,
) -> Message<'_> {
    Message::plain(role, text)
}

use super::{CallRef, Directive, Kind, Message, ToolSchema, estimate_tokens, render};

fn directive() -> Directive<'static> {
    Directive {
        version: "2026-10-26",
        text: "You are a careful agent.",
    }
}

fn tools() -> Vec<ToolSchema<'static>> {
    vec![
        ToolSchema {
            name: "shell",
            description: "run a program",
            schema: r#"{"argv":"string"}"#,
        },
        ToolSchema {
            name: "read",
            description: "read a file",
            schema: r#"{"path":"string"}"#,
        },
    ]
}

#[test]
fn the_directive_is_the_first_segment_and_the_tools_are_the_second() {
    let prompt = render(&directive(), &tools(), &[]);
    assert_eq!(prompt.segments.len(), 2);
    assert_eq!(prompt.segments[0].kind, Kind::Directive);
    assert_eq!(prompt.segments[0].text, "You are a careful agent.");
    assert_eq!(prompt.segments[1].kind, Kind::Tools);
}

#[test]
fn tools_are_written_in_name_order_whatever_order_they_arrive_in() {
    let forward = render(&directive(), &tools(), &[]);
    let mut backward = tools();
    backward.reverse();
    let reversed = render(&directive(), &backward, &[]);
    assert_eq!(forward.segments, reversed.segments);
    assert_eq!(forward.chain, reversed.chain);
}

#[test]
fn the_tool_block_starts_with_read_because_it_sorts_first() {
    let prompt = render(&directive(), &tools(), &[]);
    let block = &prompt.segments[1].text;
    let read = block
        .find(r#""name":"read""#)
        .expect("read is in the block");
    let shell = block
        .find(r#""name":"shell""#)
        .expect("shell is in the block");
    assert!(read < shell, "{block}");
}

#[test]
fn an_empty_tool_list_still_produces_a_tool_segment() {
    let prompt = render(&directive(), &[], &[]);
    assert_eq!(prompt.segments.len(), 2);
    assert_eq!(prompt.segments[1].text, "");
}

#[test]
fn messages_follow_the_tools_in_order() {
    let prompt = render(
        &directive(),
        &tools(),
        &[msg(Role::User, "hello"), msg(Role::Assistant, "hi")],
    );
    assert_eq!(prompt.segments.len(), 4);
    assert_eq!(prompt.segments[2].text, "hello");
    assert_eq!(prompt.segments[2].role, Some(Role::User));
    assert_eq!(prompt.segments[3].text, "hi");
    assert_eq!(prompt.segments[3].role, Some(Role::Assistant));
    assert_eq!(prompt.segments[2].kind, Kind::Message);
}

#[test]
fn the_chain_has_one_entry_per_segment() {
    let prompt = render(
        &directive(),
        &tools(),
        &[msg(Role::User, "a"), msg(Role::User, "b")],
    );
    assert_eq!(prompt.chain.len(), prompt.segments.len());
}

#[test]
fn the_first_chain_entry_is_the_hash_of_the_directive_alone() {
    let prompt = render(&directive(), &tools(), &[]);
    assert_eq!(
        prompt.chain[0],
        agent_core::hash::digest(b"You are a careful agent.")
    );
}

#[test]
fn each_entry_covers_everything_before_it() {
    let first = render(&directive(), &tools(), &[]);
    let mut second = first.clone();
    second.push_message(Role::User, "hello");
    assert_eq!(second.chain[..first.chain.len()], first.chain[..]);
}

#[test]
fn changing_the_directive_changes_the_whole_chain() {
    let other = Directive {
        version: "2026-10-26",
        text: "You are a rash agent.",
    };
    let left = render(&directive(), &tools(), &[]);
    let right = render(&other, &tools(), &[]);
    assert_eq!(left.chain.len(), right.chain.len());
    assert_eq!(left.shared_prefix(&right), 0);
}

#[test]
fn changing_a_tool_changes_the_chain_from_the_tool_segment_onwards() {
    let mut changed = tools();
    changed[0].schema = r#"{"path":"string","mode":"string"}"#;
    let left = render(&directive(), &tools(), &[msg(Role::User, "hi")]);
    let right = render(&directive(), &changed, &[msg(Role::User, "hi")]);
    assert_eq!(left.shared_prefix(&right), 1, "the directive is untouched");
}

#[test]
fn the_version_is_recorded_beside_the_prompt_and_never_in_its_bytes() {
    let same_text = Directive {
        version: "9999-01-01",
        text: "You are a careful agent.",
    };
    let left = render(&directive(), &tools(), &[]);
    let right = render(&same_text, &tools(), &[]);
    assert_eq!(left.chain, right.chain, "the bytes decide identity");
    assert_eq!(left.directive_version, "2026-10-26");
    assert_eq!(right.directive_version, "9999-01-01");
}

#[test]
fn a_fork_shares_every_segment_before_the_one_that_differs() {
    let prefix = [
        msg(Role::User, "one"),
        msg(Role::Assistant, "two"),
        msg(Role::User, "three"),
    ];
    let mut left_messages = prefix.to_vec();
    left_messages.push(msg(Role::User, "left"));
    let mut right_messages = prefix.to_vec();
    right_messages.push(msg(Role::User, "right"));

    let left = render(&directive(), &tools(), &left_messages);
    let right = render(&directive(), &tools(), &right_messages);
    assert_eq!(
        left.shared_prefix(&right),
        5,
        "directive, tools, three messages"
    );
}

#[test]
fn extending_a_prompt_by_hand_matches_rendering_it_in_one_go() {
    let messages = [msg(Role::User, "one"), msg(Role::Assistant, "two")];
    let one_go = render(&directive(), &tools(), &messages);

    let mut step_by_step = render(&directive(), &tools(), &[]);
    for message in &messages {
        step_by_step.push_structured(message);
    }
    assert_eq!(step_by_step.segments, one_go.segments);
    assert_eq!(step_by_step.chain, one_go.chain);
    assert_eq!(step_by_step.tokens, one_go.tokens);
}

#[test]
fn the_total_is_the_sum_of_the_segments() {
    let prompt = render(&directive(), &tools(), &[msg(Role::User, "hello there")]);
    let sum: u64 = prompt.segments.iter().map(|segment| segment.tokens).sum();
    assert_eq!(prompt.tokens, sum);
}

#[test]
fn the_estimate_is_four_bytes_to_the_token_rounded_up() {
    assert_eq!(estimate_tokens(""), 0);
    assert_eq!(estimate_tokens("a"), 1);
    assert_eq!(estimate_tokens("abcd"), 1);
    assert_eq!(estimate_tokens("abcde"), 2);
    assert_eq!(estimate_tokens("abcdefgh"), 2);
    assert_eq!(estimate_tokens("abcdefghi"), 3);
}

#[test]
fn a_prompt_shares_everything_with_itself() {
    let prompt = render(&directive(), &tools(), &[msg(Role::User, "hello")]);
    assert_eq!(prompt.shared_prefix(&prompt), prompt.segments.len());
}

#[test]
fn a_prompt_shares_nothing_with_a_shorter_one_it_does_not_extend() {
    let left = render(&directive(), &tools(), &[msg(Role::User, "hello")]);
    let right = render(&directive(), &[], &[]);
    assert_eq!(left.shared_prefix(&right), 1, "only the directive");
}

#[test]
fn the_directive_carries_no_clock_branch_or_machine_name() {
    let first = render(&directive(), &tools(), &[]);
    let second = render(&directive(), &tools(), &[]);
    assert_eq!(
        first.chain, second.chain,
        "rendering twice from the same inputs must produce the same bytes"
    );
}

#[test]
fn a_tool_segment_cannot_be_confused_by_a_name_that_looks_like_a_length() {
    let tricky = vec![
        ToolSchema {
            name: "4:read",
            description: "one",
            schema: "{}",
        },
        ToolSchema {
            name: "read",
            description: "two",
            schema: "{}",
        },
    ];
    let plain = vec![
        ToolSchema {
            name: "read",
            description: "two",
            schema: "{}",
        },
        ToolSchema {
            name: "4:read",
            description: "one",
            schema: "{}",
        },
    ];
    let left = render(&directive(), &tricky, &[]);
    let right = render(&directive(), &plain, &[]);
    assert_eq!(left.segments[1].text, right.segments[1].text);
    assert!(left.segments[1].text.contains(r#""name":"4:read""#));
    assert!(left.segments[1].text.contains(r#""name":"read""#));
}

#[test]
fn growing_a_prompt_does_not_disturb_the_original() {
    let original = render(&directive(), &tools(), &[msg(Role::User, "hello")]);
    let mut grown = original.clone();
    grown.push_message(Role::Assistant, "hi");
    assert_eq!(original.segments.len(), 3);
    assert_eq!(grown.segments.len(), 4);
    assert_eq!(original.segments[2].text, "hello");
}

#[test]
fn a_message_hashes_its_role_and_its_content_joined_by_a_colon() {
    let rendered = render(&directive(), &[], &[]);
    let mut grown = rendered.clone();
    grown.push_message(Role::User, "hello");
    let mut buffer = Vec::from(rendered.chain[1].as_bytes());
    buffer.extend_from_slice(b"user:hello");
    assert_eq!(grown.chain[2], agent_core::hash::digest(&buffer));
}

#[test]
fn the_directive_and_the_tool_block_carry_no_role() {
    let prompt = render(&directive(), &tools(), &[]);
    assert_eq!(prompt.segments[0].role, None);
    assert_eq!(prompt.segments[1].role, None);
}

#[test]
fn a_message_carries_the_calls_it_asked_for() {
    let calls = [agent_prompt_ref("t:0", "shell", r#"{"cmd":"date"}"#)];
    let message = Message {
        call: None,
        calls: &calls,
        role: Role::Assistant,
        text: "",
    };
    let prompt = render(&directive(), &tools(), &[message]);
    assert_eq!(prompt.segments[2].calls.len(), 1);
    assert_eq!(prompt.segments[2].calls[0].tool, "shell");
    assert_eq!(prompt.segments[2].call, None);
}

#[test]
fn a_tool_message_carries_the_call_it_answers() {
    let message = Message {
        call: Some("t:0"),
        calls: &[],
        role: Role::Tool,
        text: "noon",
    };
    let prompt = render(&directive(), &tools(), &[message]);
    assert_eq!(prompt.segments[2].call.as_deref(), Some("t:0"));
}

#[test]
fn two_calls_with_different_arguments_are_not_the_same_prefix() {
    let left = [agent_prompt_ref("t:0", "shell", r#"{"cmd":"date"}"#)];
    let right = [agent_prompt_ref("t:0", "shell", r#"{"cmd":"ls"}"#)];
    let left = render(
        &directive(),
        &tools(),
        &[Message {
            call: None,
            calls: &left,
            role: Role::Assistant,
            text: "",
        }],
    );
    let right = render(
        &directive(),
        &tools(),
        &[Message {
            call: None,
            calls: &right,
            role: Role::Assistant,
            text: "",
        }],
    );
    // Same role, same (empty) text, different arguments: the provider reads
    // different bytes, so the chain must not claim the prefix is shared.
    assert_eq!(left.shared_prefix(&right), 2);
}

#[test]
fn an_id_moved_between_two_messages_changes_the_prefix_too() {
    let one = |role_at: usize| {
        let calls = [agent_prompt_ref("t:0", "shell", "{}")];
        let plain = Message::plain(Role::Assistant, "hi");
        let structured = Message {
            call: None,
            calls: &calls,
            role: Role::Assistant,
            text: "hi",
        };
        let messages = if role_at == 0 {
            vec![structured, plain]
        } else {
            vec![plain, structured]
        };
        render(&directive(), &tools(), &messages)
    };
    assert_eq!(one(0).shared_prefix(&one(1)), 2, "the two messages differ");
}

fn agent_prompt_ref(
    id: &str,
    tool: &str,
    args: &str,
) -> CallRef {
    CallRef {
        args: String::from(args),
        id: String::from(id),
        tool: String::from(tool),
    }
}

#[test]
fn the_tool_block_is_the_exact_bytes_a_provider_reads() {
    assert_eq!(
        super::tool_block(&tools()),
        r#"[{"type":"function","function":{"name":"read","description":"read a file","parameters":{"path":"string"}}},{"type":"function","function":{"name":"shell","description":"run a program","parameters":{"argv":"string"}}}]"#
    );
}

#[test]
fn a_control_character_is_written_as_two_hex_digits_and_nothing_else_is() {
    let mut out = String::new();
    super::push_json_string(&mut out, "a\u{1}\u{1f} \u{7f}é\"\\");
    assert_eq!(out, "\"a\\u0001\\u001f \u{7f}é\\\"\\\\\"");
}
