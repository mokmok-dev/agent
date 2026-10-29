//! Rendering a branch to the exact bytes a server will see.
//!
//! The output is an ordered list of segments and a chain of hashes over their
//! prefixes. The chain is what makes "did this fork share a prefix" a question
//! with a byte-level answer rather than a guess, and it is what gets recorded on
//! every `model.requested`.
//!
//! A segment keeps its role beside its content instead of folding both into one
//! string, because an adapter has to send them as separate fields. The chain
//! still hashes them together, so splitting them does not change the identity of
//! any prefix.

#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use agent_core::event::Role;
use agent_core::hash::digest;
use agent_core::ids::PrefixHash;

/// Roughly how many bytes carry one token.
///
/// Deliberately crude. This number decides one thing: whether a prefix clears a
/// provider's minimum cacheable length. Billing never uses it, because the
/// provider reports the tokens it actually read and wrote. A four byte token
/// underestimates Latin text and underestimates CJK text further, and
/// underestimating is the safe direction: a prefix wrongly believed too short
/// sends a request that simply misses the cache, where the opposite mistake
/// claims a saving that did not happen.
const BYTES_PER_TOKEN: u64 = 4;

/// What a segment is, so a reader can tell the stable part from the growing part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The operator's directive. Identical across requests until a human edits it.
    Directive,
    /// The tool schemas in force. Identical across requests until the tool set changes.
    Tools,
    /// One message from the transcript.
    Message,
}

/// One call a message asked for, as the provider will read it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRef {
    /// The id the answer will carry, so the two are joined.
    pub id: String,
    pub tool: String,
    pub args: String,
}

/// One message, with whatever structure its role needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message<'a> {
    pub role: Role,
    pub text: &'a str,
    /// The calls this message asked for. Empty for anything but the answer that asked.
    pub calls: &'a [CallRef],
    /// The call this message answers, for a tool's output.
    pub call: Option<&'a str>,
}

impl<'a> Message<'a> {
    /// A message that is only words.
    #[must_use]
    pub const fn plain(
        role: Role,
        text: &'a str,
    ) -> Self {
        Self {
            role,
            text,
            calls: &[],
            call: None,
        }
    }
}

/// One immutable piece of a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub kind: Kind,
    /// The speaker, for a message. Nothing for the directive and the tool block.
    pub role: Option<Role>,
    /// The content alone, with no role in front of it.
    pub text: String,
    /// The calls this message asked for, in the order it asked.
    pub calls: Vec<CallRef>,
    /// The call this message answers.
    pub call: Option<String>,
    pub tokens: u64,
}

/// The operator's standing instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive<'a> {
    /// Recorded beside the prompt, never in its bytes.
    pub version: &'a str,
    pub text: &'a str,
}

/// One tool's schema, as the model will read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSchema<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub schema: &'a str,
}

/// A rendered prompt: the segments, their prefix chain, and a size estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// Which directive produced this prompt.
    pub directive_version: String,
    pub segments: Vec<Segment>,
    /// One entry per segment. Entry `i` identifies the prompt up to and including segment `i`.
    pub chain: Vec<PrefixHash>,
    /// The sum of the segment estimates.
    pub tokens: u64,
}

/// Estimates the tokens in a byte string.
#[must_use]
pub const fn estimate_tokens(text: &str) -> u64 {
    estimate_tokens_of_bytes(text.len() as u64)
}

/// Estimates the tokens in a byte count, for text this process never assembled.
#[must_use]
pub const fn estimate_tokens_of_bytes(bytes: u64) -> u64 {
    bytes.div_ceil(BYTES_PER_TOKEN)
}

/// Renders the directive, the tool block, and every message of a branch.
#[must_use]
pub fn render(
    directive: &Directive<'_>,
    tools: &[ToolSchema<'_>],
    messages: &[Message<'_>],
) -> Prompt {
    let mut prompt = Prompt {
        directive_version: String::from(directive.version),
        segments: Vec::with_capacity(messages.len().saturating_add(2)),
        chain: Vec::with_capacity(messages.len().saturating_add(2)),
        tokens: 0,
    };
    prompt.push(Kind::Directive, String::from(directive.text));
    prompt.push(Kind::Tools, tool_block(tools));
    for message in messages {
        prompt.push_structured(message);
    }
    prompt
}

impl Prompt {
    /// Appends one segment whose content is all there is, and extends the chain.
    ///
    /// This is the operation a turn performs: everything before it stays byte
    /// identical, so the hash of the prefix up to this point is unchanged.
    pub fn push(
        &mut self,
        kind: Kind,
        text: String,
    ) {
        self.push_segment(Segment {
            kind,
            call: None,
            calls: Vec::new(),
            role: None,
            tokens: estimate_tokens(&text),
            text,
        });
    }

    /// Appends one message, keeping its speaker beside its content.
    pub fn push_message(
        &mut self,
        role: Role,
        content: &str,
    ) {
        self.push_structured(&Message::plain(role, content));
    }

    /// Appends one message with whatever structure its role carries.
    ///
    /// The calls and the call id are part of what the provider reads, so they are
    /// part of what the chain hashes: two prompts whose only difference is the
    /// arguments of a call are not the same bytes, and the ledger must not say
    /// they are.
    pub fn push_structured(
        &mut self,
        message: &Message<'_>,
    ) {
        self.push_segment(Segment {
            kind: Kind::Message,
            call: message.call.map(String::from),
            calls: message.calls.to_vec(),
            role: Some(message.role),
            tokens: estimate_tokens(message.text),
            text: String::from(message.text),
        });
    }

    fn push_segment(
        &mut self,
        segment: Segment,
    ) {
        self.tokens = self.tokens.saturating_add(segment.tokens);
        let next = chain_entry(self.chain.last().copied(), &segment);
        self.segments.push(segment);
        self.chain.push(next);
    }

    /// How many leading chain entries two prompts agree on.
    ///
    /// This is the shared prefix, counted in segments. A value of `n` means the
    /// first `n` segments are byte identical, which is the longest prefix a
    /// server can reuse when it has already seen the other prompt.
    #[must_use]
    pub fn shared_prefix(
        &self,
        other: &Self,
    ) -> usize {
        self.chain
            .iter()
            .zip(other.chain.iter())
            .take_while(|(left, right)| left == right)
            .count()
    }
}

/// The tool block: the exact JSON array the provider's `tools` field carries.
///
/// Sorting makes two runs of the same code produce the same bytes whatever
/// order the registry handed the tools over. The schema is already JSON and is
/// spliced in as it is; the name and description are escaped here, because they
/// are text.
fn tool_block(tools: &[ToolSchema<'_>]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&ToolSchema<'_>> = tools.iter().collect();
    sorted.sort_by(|left, right| left.name.cmp(right.name));
    let mut out = String::from("[");
    for (index, tool) in sorted.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("{\"type\":\"function\",\"function\":{\"name\":");
        push_json_string(&mut out, tool.name);
        out.push_str(",\"description\":");
        push_json_string(&mut out, tool.description);
        out.push_str(",\"parameters\":");
        out.push_str(tool.schema);
        out.push_str("}}");
    }
    out.push(']');
    out
}

/// Writes one JSON string, including the quotes around it.
fn push_json_string(
    out: &mut String,
    text: &str,
) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str("\\u00");
                let byte = control as u8;
                out.push(char::from(HEX[(byte >> 4) as usize]));
                out.push(char::from(HEX[(byte & 0xf) as usize]));
            },
            other => out.push(other),
        }
    }
    out.push('"');
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The next chain entry: the hash of the previous entry followed by this segment.
///
/// The previous entry is already a digest of everything before it, so one hash
/// over one digest and one segment covers the whole prefix. A message
/// contributes its role, a colon, and its content, which is the byte string a
/// renderer keeping the role inside the text would have produced.
fn chain_entry(
    previous: Option<PrefixHash>,
    segment: &Segment,
) -> PrefixHash {
    let mut buffer = Vec::with_capacity(32 + segment.text.len() + 8);
    if let Some(previous) = previous {
        buffer.extend_from_slice(previous.as_bytes());
    }
    if let Some(role) = segment.role {
        buffer.extend_from_slice(role.as_str().as_bytes());
        buffer.push(b':');
    }
    buffer.extend_from_slice(segment.text.as_bytes());
    if !segment.calls.is_empty() {
        push_part(&mut buffer, "calls");
        push_part(&mut buffer, &segment.calls.len().to_string());
        for call in &segment.calls {
            push_part(&mut buffer, &call.id);
            push_part(&mut buffer, &call.tool);
            push_part(&mut buffer, &call.args);
        }
    }
    if let Some(call) = &segment.call {
        push_part(&mut buffer, "call");
        push_part(&mut buffer, call);
    }
    digest(&buffer)
}

/// Writes one length-prefixed part, so two different structures cannot produce
/// the same bytes.
fn push_part(
    buffer: &mut Vec<u8>,
    part: &str,
) {
    buffer.push(0);
    buffer.extend_from_slice(part.len().to_string().as_bytes());
    buffer.push(b':');
    buffer.extend_from_slice(part.as_bytes());
}

#[cfg(test)]
mod tests;
