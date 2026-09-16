//! The inference contract: the message and delta types exchanged between a
//! sandboxed node and the daemon's inference capability.
//!
//! The daemon holds the model-provider credentials and performs inference on
//! behalf of a confined agent, which reaches it over the daemon's Unix socket
//! (see `docs/inference.md`). This crate is the wire contract for that boundary:
//! [`Message`], [`ToolSpec`], [`InferenceRequest`], and the streamed [`Delta`]s
//! a [`Provider`] produces. It is deliberately provider-neutral, so a real
//! provider is a thin adapter behind the [`Provider`] trait.
//!
//! The volatile token stream is *not* part of the durable event log: only the
//! finalized [`Message`]s become events. See the [`client`] module for the
//! node-side transport.

pub mod client;
pub mod provider;
pub mod wire;

pub use client::{ClientError, InferenceClient};
pub use provider::{FakeProvider, InferenceStream, Provider, ProviderError};
pub use wire::{Delta, InferenceRequest, Message, Role, ToolCall, ToolSpec};
