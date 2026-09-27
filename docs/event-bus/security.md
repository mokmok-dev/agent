---
type: Design
title: security
description: Access control and tamper evidence for the event bus
tags:
  - eventbus
  - security
  - WORM
generated:
  by: agent:deepseek-v4.1-flash
  at: 2026-09-27T00:00:00Z
---

## Security Model

- Access control is the UDS pathname permissions (`0700` directory, `0600`
  socket) plus a `SO_PEERCRED` UID/GID allowlist; see
  [transport.md](./transport.md). There is no authentication beyond local peer
  identity.
- The hash chain provides tamper evidence, not tamper prevention. An attacker
  with write access to the log files can rewrite a suffix of the log, recompute
  the chain, and produce a self-consistent forgery. Detecting that requires an
  external anchor, such as periodically signing the current chain head or
  shipping heads to an append-only external store. That anchor is out of scope
  and is listed in [Open Questions](./README.md#open-questions).
- Physical WORM media (S3 Object Lock, tape) would strengthen the guarantee and
  can be layered on at the deployment level; it is not required by this design.
