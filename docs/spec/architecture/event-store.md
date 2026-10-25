---
type: Storage Format
title: Event store
description: Segmented append-only frames with a hash chain, group commit, torn-tail recovery, snapshots, and a fail-stopped disk-full policy.
tags: [storage, durability, recovery]
status: draft
generated: { by: hermes-agent/deepseek-v4.1-flash, at: 2026-10-25T12:57:16Z }
---

# Layout

```
<state-dir>/
  store/
    FORMAT                    # "format: 1"
    LOCK                      # advisory exclusive lock, held while writing
    segments/
      000000000001.seg
      000000000002.seg
    tips.json                 # cache, verified on load, rebuilt on mismatch
    snapshots/
      main/
        0012000-<state-hash>.snap
  blobs/
    <blake3-hex>              # payloads over the inline limit
```

`FORMAT` holds one integer. A store whose format is newer than the binary refuses to open. A store whose format is older is migrated by a forward-only step that writes a new segment recording the migration. Migration is never in place.

# Segment format

Each segment begins with an 8-byte header.

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | Magic `AGNT` |
| 4 | 2 | Format version, little endian |
| 6 | 2 | Reserved, zero |

Then a sequence of frames.

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | Payload length, little endian |
| 4 | 1 | Kind, `1` for an event. Any other value is a format error |
| 5 | n | Payload, canonical JSON event bytes |
| 5+n | 4 | CRC32C over kind and payload, little endian |

The kind byte exists so a future format revision can add a frame type without changing the header. A reader that meets an unknown kind fails stopped rather than skipping bytes it cannot measure.

Segments rotate at 64 MiB. Rotation writes a new segment header, fsyncs the file, fsyncs the directory, and only then appends the first event to it. A missing directory fsync is how a power loss loses a whole segment that the writer believed existed.

# Why CRC plus hash

The CRC catches a torn frame cheaply during a linear scan. The event hash catches a well-formed frame whose content was altered, which a CRC cannot detect if the alteration recomputed the CRC.

Both checks run on recovery and in `agentctl verify`. Verification of the whole store is O(bytes) and is a background task, not a startup task.

# Group commit

The reactor appends in batches. A batch closes when 5 ms have passed or 256 events have accumulated, whichever comes first. One `fdatasync` covers the batch. A batch is acknowledged to the reactor only after the sync returns.

Consequences.

- Append latency is bounded by the sync, not by the event count.
- A crash loses at most the open batch. Those events were never acknowledged, so no decision was taken on them.
- A batch is all or nothing at the tail. A partial batch is a torn tail, which recovery truncates whole.

# Tips cache

`tips.json` maps a branch id to its tip event id, depth, segment, and offset. It is written by atomic rename after a batch is synced.

On load, each entry is checked against the frame it points at. If the frame's id differs, the file is stale and the store rebuilds tips by scanning segments backward from the newest. The cache is never trusted, only checked.

# Startup and recovery

1. Open the store directory read-write and take the exclusive lock. A second writer fails immediately with exit code 75 rather than waiting.
2. Read `FORMAT`. Refuse a newer format. Migrate an older one by appending a migration record.
3. Find the newest segment. Scan it from the start, verifying frame lengths and CRCs, stopping at the first short read, CRC mismatch, or hash mismatch.
4. If the failure is at the end of the last segment, the tail is torn. Truncate to the last verified frame boundary, fsync, and record the byte count.
5. If the failure is in the middle of any segment, or in any earlier segment, fail stopped. The operator runs `agentctl doctor --repair` to truncate to a named event after inspecting the report.
6. Load snapshots for the branches the reactor will touch, verifying each snapshot's state hash and that its recorded tip event exists.
7. Fold from each snapshot forward. Compare the folded state hash to the snapshot hash when a snapshot covers the same depth.
8. Append `instance.started` with the recovered byte count, the segment count, and the verification outcome.

Recovery writes exactly one event, and only after recovery succeeds. A failed recovery writes nothing.

# Snapshot and compaction

A snapshot is the folded state for one branch at one depth, serialized as canonical JSON with a header carrying the tip event id, the depth, and a BLAKE3 hash of the state bytes.

- A snapshot is written every 10,000 events per branch and on clean shutdown.
- Loading a snapshot whose hash does not match its bytes discards it and folds from the previous snapshot or from genesis.
- Snapshots never delete events. Compaction of the store means adding a snapshot, never rewriting a segment.

Replay cost is therefore proportional to the events after the newest snapshot, not to total history. That is what makes a cold start after 30 days practical.

# Retention and archive

Default policy keeps every segment. An operator can move closed segments to cold storage with `agentctl archive --before <instant>`. The command writes a manifest listing each moved segment's name, byte length, first and last event id, and the count of events, then removes the segment.

A store missing an archived segment refuses to fold past the gap. It reports the gap and requires `agentctl restore` before continuing. There is no silent skip, because a silent skip would produce a state that looks plausible and is wrong.

# Blobs

A payload over 128 KiB is written to `blobs/<blake3-hex>` before the event referencing it is appended. The write is fsynced first. An event therefore never points at a blob that is absent.

`agentctl gc` lists blobs with no referencing event and deletes them. It runs only while `agentd` is stopped, or reports a conflict if the lock is held.

# Disk full

An append that fails with `ENOSPC`, `EDQUOT`, or `EIO` puts the runtime in fail-stopped state.

- The reactor stops accepting commands.
- No partial state is kept in memory for later retry.
- The process logs the failing path and errno to the journal and exits 70.
- The supervisor restarts it with backoff. Since the disk is still full, each restart fails fast and loudly, and the operator sees a repeating unit failure instead of a silent stall.

A degraded mode that keeps answering from memory was rejected. It cannot record what it did, so it would produce actions missing from history.

# Permissions

The store directory is mode 0700 and owned by the service user. Segment files are 0600. The store holds prompts, tool output, and provider responses, which routinely contain secrets and personal data. There is no read-only sharing across users, only the service user and root.

# Invariants

These are checked in tests and in `agentctl verify`.

1. Every event's `id` equals the hash recomputed from its bytes.
2. Every event except genesis has a `prev` that resolves to an existing event.
3. Every event's `depth` equals its predecessor's depth plus one.
4. An event's predecessor is on the same branch, or the event is the branch's first and its predecessor is on the parent branch.
5. Frame offsets are monotonically increasing within a segment and segments are numbered without gaps unless archived.
6. No event is ever modified after it is synced. A correction is a new event.
