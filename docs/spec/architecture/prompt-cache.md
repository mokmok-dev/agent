---
type: Architecture Overview
title: Prompt cache
description: Byte-stable prompt segments, a prefix chain per branch, a ledger that records what the provider actually cached, and a fork command that estimates the win before it spends it.
tags: [cache, prompt, fork, performance]
status: draft
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
sources:
  - id: sglang
    resource: https://arxiv.org/abs/2312.07104
    title: "SGLang: Efficient Execution of Structured Language Model Programs"
  - id: prompt-cache-paper
    resource: https://arxiv.org/abs/2311.04934
    title: "Prompt Cache: Modular Attention Reuse for Low-Latency Inference"
  - id: hydragen
    resource: https://arxiv.org/abs/2402.05099
    title: "Hydragen: High-Throughput LLM Inference with Shared Prefixes"
  - id: kv-residency
    resource: https://arxiv.org/abs/2610.22863
    title: "Understanding KV Residency in Production-Scale Agentic LLM Workloads"
  - id: dont-break-cache
    resource: https://arxiv.org/abs/2601.06007
    title: "Don't Break the Cache: Prompt Caching for Long-Horizon Agentic Tasks"
  - id: arborkv
    resource: https://arxiv.org/abs/2605.22106
    title: "ArborKV: Structure-Aware KV Cache Management for Scaling Tree-based LLM Reasoning"
  - id: forkkv
    resource: https://arxiv.org/abs/2604.06370
    title: "ForkKV: Scaling Multi-LoRA Agent Serving via Copy-on-Write Disaggregated KV Cache"
  - id: hashrope
    resource: https://arxiv.org/abs/2610.16144
    title: "Hashrope: A Persistent Data Structure for LLM Context Management"
  - id: shared-prefix-lora
    resource: https://arxiv.org/abs/2609.17109
    title: "Shared-Prefix KV Reuse Across Standard LoRA Adapters"
  - id: prompt-cache-audit
    resource: https://arxiv.org/abs/2502.07776
    title: "Auditing Prompt Caching in Language Model APIs"
  - id: continnum
    resource: https://arxiv.org/abs/2511.02230
    title: "Continnum: Efficient and Robust Multi-Turn LLM Agent Scheduling with KV Cache Time-to-Live"
  - id: vllm-apc
    resource: https://docs.vllm.ai/en/latest/features/automatic_prefix_caching/
    title: vLLM automatic prefix caching
  - id: openai-caching
    resource: https://developers.openai.com/api/docs/guides/prompt-caching
    title: OpenAI prompt caching guide
  - id: anthropic-caching
    resource: https://platform.claude.com/docs/en/build-with-claude/prompt-caching
    title: Anthropic prompt caching
  - id: deepseek-kvcache
    resource: https://api-docs.deepseek.com/guides/kv_cache
    title: DeepSeek context caching
---

# The claim, stated plainly

Rewinding history and branching from an earlier point is cheaper than starting a fresh conversation, because the new branch's prompt shares a long byte-identical prefix with the branch it forked from, and model servers can reuse the computed state of a prefix they have already processed.

The saving is in prefill: input tokens billed as cache reads and the time to first token. It is not in decoding, so a long answer barely moves in total wall time.

The mechanism is established by the serving literature. The size of the win on a hosted API is not, because it depends on a provider's cache policy, its time to live, and whether it reports what it reused. This concept specifies the mechanism, the accounting, and the measurement. It does not claim a number.

# What a prompt is made of

`agent-prompt` renders a branch to an ordered list of segments. A segment is the smallest unit the renderer will ever change on its own.

| Index | Segment | Changes when |
|---|---|---|
| 0 | System directive | A human edits the directive, or its version bumps |
| 1 | Tool schema block | A tool is added, removed, or its schema changes |
| 2..n | One per message, in depth order | Never, once appended |

Three rules make the list prefix-stable.

1. No clock, no random value, no machine name, and no branch id appears in segment 0 or 1. A system directive that says "the current time is ..." changes the first bytes of every request and destroys reuse for the whole conversation.
2. Volatile facts are appends, not prefixes. The current time, the working directory, and the repository status go into the user message that starts a turn, which sits after every earlier segment.
3. Tool schemas are ordered by tool name, not by registration order or hash map iteration. Two runs of the same code produce the same bytes.

Rule 1 is the one worth stating twice. A system prompt with a timestamp is the most common way to lose prefix reuse, and it is invisible in a transcript.

# The prefix chain

For segments `s_0 .. s_n`:

```
h_0 = BLAKE3(s_0)
h_i = BLAKE3(h_{i-1} || s_i)
```

`h_i` is the identity of the prompt prefix ending at segment `i`. It is stored in `model.requested` for every call.

The chain exists to answer four questions that the DAG alone cannot.

- Did the fork actually share a prefix, or did the renderer produce different bytes? Comparing `h_i` at the fork point on both branches answers it.
- Which prefix did the provider cache, and which call populated it?
- Is a stored cache observation still valid after a model version change?
- How long is the shared prefix, in segments and in tokens, before any request is sent?

A fork's shared prefix is the chain entry at the fork point. When the fork point is at depth `d`, that entry is `h_d`, and every call on the new branch reports a chain that starts with `h_d`.

# Why a fork saves work

Consider a branch at depth 400 with a 60,000 token prompt. The operator forks at depth 380 to retry a turn with a different model.

- The parent's last 20 events are not in the new branch's prompt, so the prefix is about 57,000 tokens.
- The new branch sends 57,000 shared tokens plus a small new suffix.
- With a warm cache, the server prefills the suffix and reads the rest from cache. Without one, it prefills 57,000 tokens again.

Starting a fresh conversation instead would prefill the whole 60,000 tokens and lose every earlier message. The saving is therefore relative to the no-fork alternatives, and it grows with conversation depth.

The fork point choice trades two things. Forking further back shares less but diverges from an earlier decision. Forking at the tip shares the most but changes nothing. `agentctl fork --estimate` prints the shared segment count, the shared token count, and the cache age of the newest matching observation, so the choice is made on numbers rather than on a feeling.

# Segment boundaries are cache boundaries

A fork at a message boundary shares a prefix that ends exactly at a segment edge, which is what the chain records. A fork inside a message is still allowed, but the shared prefix ends mid-segment, so the chain entry is stale and the server has to prefill from the previous boundary.

`--boundary` snaps a fork to the nearest segment edge and reports the adjustment. The default is on.

# What breaks a hit

| Cause | Detectable before sending | Mitigation |
|---|---|---|
| System directive edited | yes, chain differs at segment 0 | Accept the cost, or version the directive and keep the old one |
| Tool schema changed | yes, chain differs at segment 1 | Keep tool sets stable per branch |
| Provider cache expired | partly, from the last observation's age | `agentctl warm` |
| Different account, org, or region | no | Configure one identity per provider |
| Model version bump | yes, model id differs | Treat as a new cache namespace |
| Prefix below the provider minimum | yes, from token count | Nothing. Short prefixes never cache |
| Rendering difference in the adapter | yes, chain differs with unchanged input | A renderer bug. Treat as a defect |
| Cache pressure from many live branches | no | Limit concurrent warm branches |

The last row is the one that makes cache economics counter-intuitive at scale. Twenty branches each holding a 50,000 token prefix compete for the same cache, and eviction can cost more than the branches save. The reactor's active branch limit is a cache decision as much as a fairness decision.

# The ledger

`model.cache.observed` is appended after every response from a provider whose profile claims prefix caching.

| Field | Meaning |
|---|---|
| `prefix_hash` | The chain entry of the last shared segment |
| `prefix_segments` | How many segments the prefix covered |
| `prefix_tokens` | The renderer's token count for the prefix |
| `reported` | Whether the provider reported a cached token count |
| `cached_tokens` | The reported count, or absent |
| `uncached_input_tokens` | The rest of the input |
| `first_token_ms` | Time to first token |
| `total_ms` | Time to the final token |

Three rules keep it honest.

- `reported: false` is recorded when the provider says nothing. The ledger never infers a hit from latency.
- The token counts come from the provider when reported and from the renderer's estimate otherwise, and the event says which.
- A failed call still records its prefix hash, so the reuse pattern is visible even when the call did not complete.

The headline metric is `cache_read_share = cached_tokens / (cached_tokens + uncached_input_tokens)`, computed per provider and per branch, with no cross-provider average, because the denominators mean different things.

# Measurement plan

The design is a hypothesis until it is measured on the provider we actually use. The plan is a controlled comparison, not a demo.

1. Build a branch with `N` turns, then fork it at the start of the last turn and at five turns back. Replay each fork with the same input.
2. For each fork, record the ledger row and the time to first token.
3. Build a control branch that carries the same suffix but no shared prefix, so the two arms differ in prefix reuse and nothing else.
4. Pin the rendered prompt token count within 1 percent between arms. A smaller prompt also lowers time to first token, and that would be the wrong explanation for a win.
5. Report `cached_tokens`, `first_token_ms`, and `cache_read_share` per arm, with the arm's exact prompt token count beside it.

If the provider does not report cached tokens, the result is reported as a latency observation with the cache state unknown, and no cache claim is made. That is a real outcome, not a failure of the experiment.

A local model served by vLLM or SGLang is the control case with a known answer, because prefix reuse there is a documented property of the server rather than a vendor policy. Running the same probe against a local server and a hosted API separates "our prefix really is shared" from "the provider chose to charge us less".

# Why not cache prompts locally

A local prompt cache would change the bytes sent, or would require the provider to accept a cache key we control. The first defeats an exact-prefix match. The second exists on some APIs and is worth adding as an adapter feature later, but it is not a substitute for stable bytes.

The stable-bytes discipline is also what makes the ledger meaningful. Without it, a "hit rate" measures the cache's tolerance for churn rather than the design's reuse.

# Evidence

Two questions decide whether the claim holds. Does prefix reuse reduce work, and does a fork reuse anything?

## What the literature establishes

- A radix tree over cached prefixes serves the longest shared prefix first. SGLang reports up to 6.4 times the throughput of its baselines, with hit rates of 50 to 99 percent on workloads that share prefixes: few-shot evaluation, tree-of-thought search, multi-turn chat, and RAG.[^sglang]
- Modular reuse works when the reused span keeps its absolute positions. Prompt Cache reports 8 times lower time to first token on GPU and 60 times on CPU, with no accuracy change.[^prompt-cache-paper]
- Shared prefixes can also be decomposed rather than cached. Hydragen batches attention over a shared prefix and reports up to 32 times end-to-end throughput at large batch, and it generalizes explicitly to tree-structured prompt sharing.[^hydragen]
- Agent traces are where this pays. Across 121 million model calls in production traces, about 80 percent of context tokens were served from the prefix cache.[^kv-residency]
- On commercial APIs the effect is real but smaller and split between cost and latency. Long-horizon agent sessions saw 41 to 80 percent lower cost and 6 to 31 percent lower time to first token, with a minimum cacheable prefix of 1,024 or 4,096 tokens depending on the provider.[^dont-break-cache]

## What the literature does not establish

- Nobody measures a fork against a fresh conversation. The nearest works are ForkKV for copy-on-write KV storage across adapters, ArborKV for memory under tree search, and Hashrope for branch and snapshot context structures. None reports the saving in an experiment loop, which is the exact claim here.[^forkkv][^arborkv][^hashrope]
- The effect is prefill only. vLLM's automatic prefix caching documents that it does not speed up generation,[^vllm-apc] and SGLang reports little gain when decoding dominates the request.[^sglang] A long answer barely moves.
- Branching multiplies cached state. ArborKV needs roughly a 4 times memory reduction to hold a tree-search frontier at usable quality,[^arborkv] and pinning every live agent session in a production trace would need half a petabyte of HBM.[^kv-residency] Forks add cache pressure, and pressure evicts the parent prefix the next fork needs.
- Residency is fragile. Reuse fell from 89 percent to 29 percent when the next call landed on a different deployment.[^kv-residency]
- Reuse can be a copy rather than a share. One shared-prefix study found the prefix was never physically shared, so memory did not fall and quality drifted by up to 4.6 EM.[^shared-prefix-lora]
- Tool-call gaps are a documented eviction point, which is the reason a time-to-live policy for agent sessions exists as its own line of work.[^continnum]

## What providers actually offer

Fetched 2026-10-25. Prices and thresholds change, so the design reads them from the ledger rather than hard-coding them.

| Provider | Minimum cacheable prefix | Lifetime | Cache read price |
|---|---|---|---|
| OpenAI | 1,024 tokens on current models | 30 minutes by default, 12 hours as an option | 0.1 times the input price |
| Anthropic | 512 to 4,096 tokens depending on model, at most four breakpoints | 5 minutes, refreshed on use, or 1 hour at a higher write price | 0.1 times the input price |
| DeepSeek | Matched as whole prefix units, not arbitrary prefixes | Disk cache, on by default | About 96.7 to 98 percent below a miss |

[^openai-caching] [^anthropic-caching] [^deepseek-kvcache]

Three consequences follow for this design.

1. Below the provider minimum there is no reuse at all, so a short conversation gets nothing and the ledger will show it.
2. DeepSeek matches whole prefix units persisted at boundaries, so `A+B` then `A+C` misses until `A` alone is cached. Segment boundaries make `A` cacheable on its own, which is a second reason to render by segment.
3. Cache residency can be shared across customers. An audit of commercial APIs found caching in 8 of 17 providers with global sharing in 7, reachable through timing.[^prompt-cache-audit] Treat a prompt as potentially provider-global, and never put a secret in one. The tool design already keeps secret values out of prompts, and this is the second reason for it.

## What this design does about the gap

- The ledger records `reported: false` instead of inferring a hit from latency.
- The measurement plan is the only source for a number in this bundle. A published number from another workload is cited as evidence about the mechanism, never as a prediction for this one.
- The active branch limit is a cache decision. Twenty live 50,000 token prefixes compete with each other, and the limit exists to keep that working set inside the provider's cache.

# Consequences for the reactor

- Compaction and snapshotting must not reorder events. A fold order change changes the rendered prompt, so it changes every chain entry. Snapshot format changes are versioned for the same reason.
- Prompt rendering is a pure function with its own golden tests. A rendering change that alters whitespace is a cache-invalidating event, and the test suite says so.
- A branch's model id is part of its cache namespace. Switching models mid-branch is allowed and starts a cold prefix, which the ledger shows.
- Forking is cheap in storage and expensive in cache pressure. The active branch limit is set with both in mind.

[^sglang]: SGLang: Efficient Execution of Structured Language Model Programs, https://arxiv.org/abs/2312.07104
[^prompt-cache-paper]: Prompt Cache: Modular Attention Reuse for Low-Latency Inference, https://arxiv.org/abs/2311.04934
[^hydragen]: Hydragen: High-Throughput LLM Inference with Shared Prefixes, https://arxiv.org/abs/2402.05099
[^kv-residency]: Understanding KV Residency in Production-Scale Agentic LLM Workloads, https://arxiv.org/abs/2610.22863
[^dont-break-cache]: Don't Break the Cache: Prompt Caching for Long-Horizon Agentic Tasks, https://arxiv.org/abs/2601.06007
[^forkkv]: ForkKV: Scaling Multi-LoRA Agent Serving via Copy-on-Write Disaggregated KV Cache, https://arxiv.org/abs/2604.06370
[^arborkv]: ArborKV: Structure-Aware KV Cache Management for Scaling Tree-based LLM Reasoning, https://arxiv.org/abs/2605.22106
[^hashrope]: Hashrope: A Persistent Data Structure for LLM Context Management, https://arxiv.org/abs/2610.16144
[^shared-prefix-lora]: Shared-Prefix KV Reuse Across Standard LoRA Adapters, https://arxiv.org/abs/2609.17109
[^continnum]: Continnum: Efficient and Robust Multi-Turn LLM Agent Scheduling with KV Cache Time-to-Live, https://arxiv.org/abs/2511.02230
[^vllm-apc]: vLLM automatic prefix caching, https://docs.vllm.ai/en/latest/features/automatic_prefix_caching/
[^prompt-cache-audit]: Auditing Prompt Caching in Language Model APIs, https://arxiv.org/abs/2502.07776
[^openai-caching]: OpenAI prompt caching guide, https://developers.openai.com/api/docs/guides/prompt-caching
[^anthropic-caching]: Anthropic prompt caching, https://platform.claude.com/docs/en/build-with-claude/prompt-caching
[^deepseek-kvcache]: DeepSeek context caching, https://api-docs.deepseek.com/guides/kv_cache
