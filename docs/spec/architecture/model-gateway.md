---
type: Interface Spec
title: Model gateway
description: One port, thin provider adapters, retries that never duplicate effects, and a ledger that records what the cache actually did.
tags: [model, provider, streaming, cache]
status: draft
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
---

# The port

`agent-core` declares the port. Adapters live in `agent-model`.

The port has two operations. One reports the model profile, which states the model id, the context window, whether the provider caches prefixes, how long that cache is expected to live, and whether the provider reports cached token counts. The other takes a rendered request and returns a stream.

The request carries the rendered prompt segments, not one string. Segmentation survives the port, so an adapter can send a cache hint, an explicit cache key, or a plain concatenation without re-parsing text.

The stream yields token deltas, then either a usage report or a typed error. Deltas are ephemeral. They reach surfaces over the doorbell socket as non-durable frames and are never appended. Only the finished message becomes an event, so a reconnect reads the log and cannot observe half a message.

# Adapters in the first release

| Adapter | Covers |
|---|---|
| `openai-compatible` | OpenAI, DeepSeek, vLLM, llama.cpp server, Ollama, LM Studio |
| `anthropic` | Messages API with explicit cache breakpoints |

Two adapters cover essentially the whole market because everything except Anthropic speaks the OpenAI chat-completions shape. A third adapter is a later PR, not a v1 requirement.

# Retry policy

Retry is allowed only when all of these hold.

1. The request never produced a token.
2. The error is transport-level, a 5xx, a 429, or a timeout.
3. The attempt count is under 3.

Retry is refused for a 4xx that names a malformed request, a context-length rejection, and anything after the first token. A partial response that fails mid-stream is recorded as `model.failed` with `partial: true`, and the turn fails. It is not retried, because a retried retry would double the cost and could produce two different answers for one turn.

Backoff is exponential with full jitter, starting at 500 ms and capped at 30 seconds. The next attempt instant is persisted as a `schedule.armed` event, so a restart resumes the backoff instead of resetting it.

# Token accounting

Usage comes from the provider when it reports it. When it does not, the gateway records an estimate computed from the segment token counts and marks the event `estimated: true`. Every number in the ledger therefore carries whether it was reported or estimated.

Per-turn and per-day totals are projections. They are recomputed by folding, never stored as a running counter, so a restart cannot drift them.

# The cache ledger

`dev.mokmok.agent.model.cache.observed` is appended after every response whose profile claims prefix caching. The payload records the prefix hash that was sent, the cached token count the provider reported, the uncached input count, and the provider's cache key if the adapter passed one.

Three rules keep the ledger honest.

- A provider that does not report cached tokens produces `reported: false` and no cached count. The ledger never infers a hit from latency.
- A prefix hash is recorded even when the request failed, so a failed call still contributes to the pattern of reuse.
- The ledger is a projection source, not a decision input. Decisions read a coarse cache summary, never a per-call list.

# Warming

A provider cache expires. The TTL is per provider and is not observable from the client, so the design treats it as an estimate.

`agentctl warm --branch <id>` sends a minimal request whose prompt is the branch's prefix and whose output length is one token. It is recorded as a `model.requested` with `purpose: "warm"`, so the ledger learns the prefix again without appending a message to the transcript.

Warming is optional and never automatic. Automatic warming on every fork would pay latency and tokens for a cache hit that may not exist, and the cost would be invisible in the transcript.

# What the gateway does not do

- It does not choose the model. That is a decision in `agent-core`, recorded as a `turn.requested` payload.
- It does not manage conversation state. Every request is built from events.
- It does not cache prompts itself. The provider's cache and the local ledger are the whole mechanism. A local prompt cache would change the bytes sent, which would defeat the provider's exact-prefix match.

# Errors as data

Provider errors are mapped to a closed set, so the reactor can decide without parsing strings.

| Class | Retryable | Meaning |
|---|---|---|
| `transport` | yes | Connection failed, DNS failed, TLS failed |
| `timeout` | yes | No first token before the deadline |
| `rate_limited` | yes | 429 with or without a retry hint |
| `server_error` | yes | 5xx |
| `invalid_request` | no | 4xx that names the request |
| `context_length` | no | The rendered prompt exceeds the window |
| `content_filter` | no | The provider refused the content |
| `cancelled` | no | The operator or the reactor cancelled it |
| `unknown` | no | Anything else, with the raw body kept in a blob |
