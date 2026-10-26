---
type: Storage Format
title: Event store
description: One SQLite database of events, append-only enforced by triggers, verified by a content hash chain, with snapshots and online backup.
tags: [storage, durability, recovery, sqlite]
status: stable
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
verified: { by: human:pranc1ngpegasus, at: 2026-10-25T14:07:44Z }
---

# Layout

| Path | Holds |
|---|---|
| `agent.db` | Events, snapshots, and metadata |
| `agent.db-wal` | The write-ahead log, managed by SQLite |
| `agent.db-shm` | The shared-memory index that lets readers see a consistent snapshot while the writer appends |
| `LOCK` | An advisory exclusive lock, held by the one writer |
| `blobs/<hash>` | Payloads over the inline limit, content addressed |

One file holds the history. There is no segment rotation, no frame format, and no sidecar index.

# Schema

The events table. One row per event, and the row is the whole envelope.

| Column | Type | Meaning |
|---|---|---|
| `id` | text, primary key | The content hash, lowercase hex |
| `branch` | text | Branch id |
| `depth` | integer | Ancestor count, genesis at zero |
| `prev` | text or null | Predecessor id, null only for genesis |
| `type` | text | CloudEvents type |
| `source` | text | Constant producer URI |
| `subject` | text | The aggregate, such as `branch/main` |
| `time` | text | RFC 3339 UTC |
| `actor`, `causationid`, `correlationid`, `model` | text | Extension attributes |
| `schemaver` | integer | Payload schema version |
| `payload` | text | Canonical JSON for the `data` attribute |
| `dataref` | text or null | Blob path when the payload is offloaded |
| `bytes` | integer | Canonical payload length |

The `payload` column holds the canonical bytes the core wrote, verbatim. Nothing in the store re-encodes an event, so a verification is a hash of stored bytes rather than a serializer that could drift from the writer. The `dataref` and `bytes` columns arrive with blob offloading, which lands with tool output.

Three indexes. A unique index on branch and depth, so a branch cannot hold two events at the same depth. An index on `prev`, so chain verification and branch trees are cheap. An index on type and time, so the audit view and the metrics projection do not scan.

Two more tables. `snapshots` holds folded state per branch per depth, with the state hash. `meta` holds the store schema version and the instance id.

`branch` is redundant with `prev` on purpose. Reading one branch is then an index range scan instead of a recursive walk, and verification checks the redundancy rather than trusting it. A wrong `branch` is a store error, not a slow query.

# Append-only is enforced, not promised

Four layers, weakest to strongest.

1. Triggers abort any update or delete on the events table. A bug in our own code cannot rewrite history.
2. The writer connection only ever inserts. No update path exists in the code that holds it.
3. Surfaces open the database read-only, so a UI bug cannot write at all.
4. The hash chain catches what the first three cannot, which is an edit made by another tool against the file, or a backup restored from the wrong place.

The fourth layer is the one that matters over years. The first three stop our own software. The fourth stops everything else.

# Durability

- The database runs in write-ahead logging mode with full synchronous commits. A committed transaction survives a power loss.
- Group commit is one transaction per reactor batch. The batch closes at 5 ms or 256 events, and the commit returns before the reactor treats the events as accepted. A single append is a batch of one, so the same code path is exercised at both ends.
- Crash recovery is SQLite's. An uncommitted transaction is rolled back on the next open. There is no torn file to truncate, because there is no frame format to tear.
- The writer runs a passive checkpoint after a batch when the log grows past a threshold, so a long run does not leave a giant unreplayed log.

# Why this replaced framed segments

The first draft of this concept stored length-prefixed frames in rotated segment files. That version needed a frame layout, a checksum policy, segment rotation with directory syncs, a tips cache checked against frames, torn-tail truncation, and an archive format with a manifest. Every one of those is code we would have written, tested at byte boundaries, and owned for the life of the system, and the failure mode of getting one wrong is silent loss of history.

SQLite already provides crash-safe append, concurrent readers, a stable total order, and online backup. The custom format bought one thing, a storage layer that a mutation tool can see, and that advantage is narrower than it looks: SQL statements are opaque to `cargo mutants` either way, so most of the storage semantics need integration tests regardless.

The reversal is recorded in [decisions](decisions.md).

# Fork reads are a range scan

Reading a branch is one indexed query over branch and depth up to a limit. No recursion, no DAG walk.

The DAG walk is needed only for the branch tree, which is a query over `branch.created` events across all branches. That is why the fork stays cheap: the fork appends one row, and both reads stay cheap because of the two indexes.

# Startup and recovery

1. Open the database read-write and take the lock. A second writer exits with code 75 instead of waiting.
2. Read the store schema version from `meta`. Refuse a newer version. Migrate an older one forward, recording the migration both in `meta` and as an event.
3. Run SQLite's quick integrity check.
4. Verify the chain forward from the newest snapshot of every active branch: each id equals the hash of its row, each `prev` resolves, each depth is its predecessor's depth plus one, and each event sits on the branch its predecessor implies.
5. Load snapshots, discarding any whose state hash disagrees with its bytes.
6. Fold forward from the newest usable snapshot.
7. Append `instance.started` with the row count, the snapshot count, and the verification outcome.

A failed verification writes nothing and fails stopped. It does not attempt repair, because a repair that guesses is worse than a refusal.

# Reading back

The store also owns the decoder. The core writes an event and never reads one back, so the shape of a stored event is defined in exactly one place on each side: the writer in the core, the reader here.

Two properties make the round trip total rather than nearly total.

- An unknown payload is kept as the exact bytes it arrived as, not re-serialized. The decoder borrows the original slice of the body, so an event this build does not understand survives a decode and an encode unchanged.
- An event whose schema version is newer than this build becomes unknown rather than truncated. Reading it into a known type would silently drop the fields the newer build added, and a re-encode would then change its identity.

The test that matters runs every event type through the writer and the decoder and asserts both the value and the bytes. A decoder that agrees with the writer on meaning but not on bytes would still break every identifier.

# Snapshots

A snapshot is the folded state of one branch at one depth, with a hash over the state bytes and the tip event id.

- Written every 10,000 events per branch and on clean shutdown.
- A snapshot whose hash does not match is discarded, and the fold falls back to the previous snapshot or to genesis.
- Snapshots never delete events. There is no compaction step, because there is nothing to compact.

Replay cost is proportional to the events after the newest snapshot, not to total history. That is what makes a cold start after 30 days practical.

# Change notification for readers

SQLite exposes `data_version`, a counter that changes when another connection commits. A surface polls it every 250 ms and re-reads when it moves. The reactor also sends a doorbell frame over the socket, so a surface usually wakes in milliseconds and the poll is only a safety net.

This replaces watching a segment file's length and rescanning a directory.

# Backup

`agentctl backup --out <path>` uses SQLite's online backup, so it produces a consistent copy while the reactor is running. A restore is three steps. Stop the service, put the file in place, start the service. Startup verification then proves the restored copy, because a bad backup fails the chain check instead of serving wrong state.

A nightly backup to a second disk is part of the install, not a suggestion. The custom format would have needed a file copy plus a manifest check for the same property.

# Blobs

A payload over 128 KiB is written to `blobs/<hash>` and synced before the event row commits. An event therefore never points at a blob that is absent.

`agentctl gc` lists blobs with no referencing row and deletes them. It runs while the service is stopped, or reports a conflict if the lock is held. Blobs are the part of the store that grows fastest, because tool output is unbounded and history is not.

# Retention

None in the first release, and none planned while the numbers stay small. At roughly 500 bytes per event and ten thousand events a day, a year is under two gigabytes. Blobs dominate long before rows do.

If retention becomes necessary, the plan is month-sharded databases attached read-only, not row deletion. Row deletion would break append-only, and append-only is worth more than disk.

# Disk full

A commit that fails for lack of space, quota, or an IO error puts the runtime in fail-stopped state.

- The reactor stops accepting commands.
- No partial state is kept in memory for later retry.
- The process logs the failing path to the journal and exits 70.
- The supervisor restarts it with backoff. Since the disk is still full, each restart fails fast and loudly, and the operator sees a repeating unit failure instead of a silent stall.

A degraded mode that keeps answering from memory was rejected. It cannot record what it did, so it would produce actions missing from history.

# Permissions

The state directory is mode 0700, owned by the service user. The database and its log are 0600. The store holds prompts, tool output, and provider responses, which routinely contain secrets and personal data. There is no sharing across users, only the service user and root.

# Invariants

Checked in tests and in `agentctl verify`.

1. Every event's id equals the hash recomputed from its row.
2. Every event except genesis has a `prev` that resolves to an existing row.
3. Every event's depth equals its predecessor's depth plus one.
4. An event's predecessor is on the same branch, or the event is the branch's first and its predecessor is on the parent branch.
5. Branch and depth are unique together.
6. No event is updated or deleted after commit. A correction is a new event.
