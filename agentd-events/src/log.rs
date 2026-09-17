//! Durable, append-only JSONL persistence for the event bus.
//!
//! Under the event-sourced model the log is the source of truth:
//! [`EventLog::publish`] returns only after the event has been appended and
//! synced, so an acknowledged event cannot be lost. Live fanout through the
//! inner fanout happens after the sync and in log order; a subscriber
//! that falls behind loses its live stream without affecting the log.
//!
//! One dedicated writer thread owns the file and the sequence number, which
//! serializes every append and keeps the log order equal to the delivery order.
//! Appends are committed in batches: the writer drains the queue, writes every
//! event of the batch, then syncs once, so concurrent publishers share a single
//! `fsync` (group commit).
//!
//! Events are stored one per line as their verbatim `CloudEvents` envelope, so
//! the log is consumable by ordinary tooling (`jq`, `rg`, `DuckDB`). Positions
//! are one-based line numbers, exposed as [`Seq`]; the position is paired with
//! the event as a [`LogEntry`] on the live bus and when reading history, but is
//! never written into the file, so the file stays a plain `CloudEvents` stream.
//!
//! The log is at-least-once across a writer failure: an event that was written
//! but not acknowledged can survive a crash, so a publisher that retries may
//! append it twice. Consumers deduplicate by the stable [`Event::id`].

use crate::{Event, EventBus, LogEntry, chain};
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};

/// The largest number of events committed in one group commit. Bounds the
/// memory a single batch can hold when the writer queue is large.
const MAX_BATCH_EVENTS: usize = 1024;

/// The default capacity of the live broadcast channel and of the writer queue.
const DEFAULT_CAPACITY: usize = 1024;

/// A one-based position in the event log.
pub type Seq = u64;

/// Errors returned by the log.
#[derive(Debug, Error)]
pub enum LogError {
    /// Reading, writing, truncating, or syncing the log file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A log line could not be decoded, or an event could not be encoded.
    #[error("event is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The writer thread has stopped, so durability can no longer be promised.
    #[error("the event log writer has stopped")]
    WriterStopped,
    /// The log path is a symbolic link. Following it would let another user
    /// redirect the append (and the recovery truncation) to a file they control.
    #[error("the event log path {0} is a symbolic link")]
    Symlink(PathBuf),
    /// The hash chain is broken at `seq`: the log was edited, reordered, or
    /// written without the chain.
    #[error("the event log hash chain is broken at position {seq}: {reason}")]
    Chain {
        /// The one-based position of the record that failed verification.
        seq: Seq,
        /// What was wrong with it.
        reason: String,
    },
}

/// An append-only JSONL log with a live fanout.
///
/// Cloning shares the same file, writer thread, and subscriber set.
#[derive(Debug, Clone)]
pub struct EventLog {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    path: PathBuf,
    writer: mpsc::Sender<Pending>,
    /// The position of the last committed event, maintained by the writer.
    tail: Arc<AtomicU64>,
    bus: EventBus,
}

/// A publish awaiting its durable append.
#[derive(Debug)]
struct Pending {
    event: Event,
    ack: oneshot::Sender<Result<Seq, LogError>>,
}

impl EventLog {
    /// Opens the log at `path`, recovers it, and starts the writer thread.
    ///
    /// The parent directory is created if needed. A trailing partial line left
    /// by a crash is truncated; complete lines are kept and the sequence number
    /// resumes after them.
    ///
    /// The log contains command strings and outputs, so on Unix it is created
    /// with mode `0600` and a symbolic link at `path` is refused.
    ///
    /// Only one `EventLog` may own a given `path` at a time; concurrent writers
    /// would interleave their appends.
    ///
    /// # Errors
    ///
    /// Returns [`LogError::Io`] if the file cannot be opened, recovered, or the
    /// writer thread cannot be spawned, and [`LogError::Symlink`] if `path` is a
    /// symbolic link.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LogError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(LogError::Symlink(path));
        }
        let (next_seq, last_hash) = recover(&path)?;
        restrict_permissions(&path)?;
        let file = OpenOptions::new().append(true).open(&path)?;

        let bus = EventBus::new(DEFAULT_CAPACITY);
        let (writer, pending) = mpsc::channel(DEFAULT_CAPACITY);
        let tail = Arc::new(AtomicU64::new(next_seq - 1));
        let thread_bus = bus.clone();
        let thread_tail = Arc::clone(&tail);
        std::thread::Builder::new()
            .name(String::from("eventlog"))
            .spawn(move || {
                write_loop(
                    file,
                    next_seq,
                    last_hash,
                    pending,
                    &thread_bus,
                    &thread_tail,
                );
            })?;

        Ok(Self {
            inner: Arc::new(Inner {
                path,
                writer,
                tail,
                bus,
            }),
        })
    }

    /// Durably appends `event` and returns its log position.
    ///
    /// The future resolves once the event has been written and synced, and the
    /// event has been fanned out to live subscribers. When the writer queue is
    /// full, awaiting this future applies backpressure instead of losing events.
    ///
    /// # Errors
    ///
    /// Returns [`LogError::WriterStopped`] if the writer thread has stopped, and
    /// [`LogError::Json`] if the event cannot be encoded.
    pub async fn publish(
        &self,
        event: Event,
    ) -> Result<Seq, LogError> {
        let (ack, applied) = oneshot::channel();
        self.inner
            .writer
            .send(Pending { event, ack })
            .await
            .map_err(|_| LogError::WriterStopped)?;
        applied.await.map_err(|_| LogError::WriterStopped)?
    }

    /// Subscribes to events as they are appended.
    ///
    /// A subscriber receives events in log order but only from this point on; a
    /// subscriber that falls behind loses events and observes
    /// [`broadcast::error::RecvError::Lagged`]. Use [`read_from`](Self::read_from)
    /// to consume history regardless of pace.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.inner.bus.subscribe()
    }

    /// Returns the position of the last committed event, or zero when the log
    /// is empty.
    ///
    /// This is the highest position any subscriber can have observed. A resume
    /// request for a position above `tail_seq() + 1` refers to a position that
    /// will never be appended to this log.
    #[must_use]
    pub fn tail_seq(&self) -> Seq {
        self.inner.tail.load(Ordering::Relaxed)
    }

    /// Reads the log from `from` (inclusive) to the end.
    ///
    /// # Errors
    ///
    /// Returns [`LogError::Io`] if the file cannot be opened.
    pub fn read_from(
        &self,
        from: Seq,
    ) -> Result<LogReader, LogError> {
        LogReader::open(&self.inner.path, from)
    }

    /// Verifies this log's hash chain, returning the number of chained records.
    ///
    /// # Errors
    ///
    /// Returns [`LogError::Chain`] at the first record that breaks the chain.
    pub fn verify_chain(&self) -> Result<u64, LogError> {
        verify_chain(&self.inner.path)
    }
}

/// Verifies the hash chain of the log at `path`, returning the number of
/// chained records.
///
/// A trailing line without a newline is a partial append and is ignored, so
/// verification does not fail on a log being written. A record without the
/// chain attributes (a log that predates the chain, or one written by an
/// attacker) fails as unchained.
///
/// # Errors
///
/// Returns [`LogError::Chain`] at the first record whose `prevhash` does not
/// match the previous record or whose `chainhash` does not match its contents,
/// [`LogError::Json`] for a malformed line, and [`LogError::Io`] on a read
/// failure.
pub fn verify_chain(path: &Path) -> Result<u64, LogError> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    let mut expected = String::from(chain::GENESIS_HASH);
    let mut verified = 0_u64;
    loop {
        buffer.clear();
        let read = reader.read_until(b'\n', &mut buffer)?;
        if read == 0 || buffer.last() != Some(&b'\n') {
            break;
        }
        verified += 1;
        let seq = verified;
        let value: Value =
            serde_json::from_slice(&buffer[..buffer.len() - 1]).map_err(LogError::Json)?;
        let Some((prev, hash)) = chain::attributes(&value) else {
            return Err(LogError::Chain {
                seq,
                reason: String::from(
                    "the record has no hash chain attributes; the log predates the chain or was rewritten",
                ),
            });
        };
        if prev != expected {
            return Err(LogError::Chain {
                seq,
                reason: String::from(
                    "the previous-hash attribute does not match the previous record",
                ),
            });
        }
        let canonical = chain::canonical(&value)?;
        if chain::hash(&prev, &canonical) != hash {
            return Err(LogError::Chain {
                seq,
                reason: String::from("the record hash does not match its contents"),
            });
        }
        expected = hash;
    }
    Ok(verified)
}

/// Recovers `path` and returns the next free [`Seq`] and the last record's
/// hash, so the chain can continue.
///
/// Creates an empty file when `path` is missing. When the file does not end in
/// a newline, the trailing bytes are a partial append from a crash and are
/// truncated. The file is streamed, so recovery does not load the log into
/// memory.
fn recover(path: &Path) -> Result<(Seq, String), LogError> {
    let file = recover_options().open(path)?;

    let (complete_end, lines, last_hash) = {
        let mut reader = BufReader::new(&file);
        let mut buffer = Vec::new();
        let mut complete_end: u64 = 0;
        let mut lines: u64 = 0;
        let mut last_hash: Option<String> = None;
        loop {
            buffer.clear();
            let read = reader.read_until(b'\n', &mut buffer)?;
            if read == 0 || buffer.last() != Some(&b'\n') {
                break;
            }
            complete_end += read as u64;
            lines += 1;
            last_hash = serde_json::from_slice::<Value>(&buffer[..buffer.len() - 1])
                .ok()
                .and_then(|value| chain::attributes(&value))
                .map(|(_, hash)| hash);
        }
        (complete_end, lines, last_hash)
    };

    if complete_end < file.metadata()?.len() {
        file.set_len(complete_end)?;
        file.sync_all()?;
    }
    // A log written before the chain existed (or with an unchained trailing
    // line) resumes the chain from genesis; `verify_chain` reports it when it
    // walks the whole file.
    Ok((
        lines + 1,
        last_hash.unwrap_or_else(|| String::from(chain::GENESIS_HASH)),
    ))
}

/// Opens (creating if needed) the log for recovery, private to the owner on
/// Unix so the file is never briefly readable by other users.
fn recover_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
}

/// Restricts an existing log file to mode `0600` on Unix.
fn restrict_permissions(path: &Path) -> Result<(), LogError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Drains pending publishes into batches, commits each batch, and fans it out.
fn write_loop(
    mut file: File,
    mut next_seq: Seq,
    mut last_hash: String,
    mut pending: mpsc::Receiver<Pending>,
    bus: &EventBus,
    tail: &AtomicU64,
) {
    while let Some(first) = pending.blocking_recv() {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH_EVENTS {
            match pending.try_recv() {
                Ok(item) => batch.push(item),
                Err(_) => break,
            }
        }

        if let Err(error) = commit(&mut file, &mut next_seq, &mut last_hash, batch, bus, tail) {
            tracing::error!(%error, "event log write failed; stopping the writer");
            return;
        }
    }
}

/// Appends `batch` in one write, syncs once, then fans out in log order.
///
/// Each line is sealed into the hash chain before it is written, so the chain
/// covers exactly the bytes on disk.
fn commit(
    file: &mut File,
    next_seq: &mut Seq,
    last_hash: &mut String,
    batch: Vec<Pending>,
    bus: &EventBus,
    tail: &AtomicU64,
) -> Result<(), LogError> {
    let mut buffer = String::new();
    let mut accepted = Vec::with_capacity(batch.len());
    for item in batch {
        match chain::seal(&item.event, last_hash) {
            Ok((line, hash)) => {
                *last_hash = hash;
                buffer.push_str(&line);
                buffer.push('\n');
                accepted.push((*next_seq, item));
                *next_seq += 1;
            },
            Err(error) => {
                let _ = item.ack.send(Err(LogError::Json(error)));
            },
        }
    }

    file.write_all(buffer.as_bytes())?;
    file.sync_data()?;

    // Publish the tail before the fanout, so a subscriber that observes an
    // event also observes a tail that covers its position.
    tail.store(*next_seq - 1, Ordering::Relaxed);
    for (seq, item) in accepted {
        bus.publish(LogEntry::new(seq, item.event));
        let _ = item.ack.send(Ok(seq));
    }
    Ok(())
}

/// A streaming reader over a log file.
///
/// Iterates entries in order, skipping the first `from - 1` lines. A trailing
/// line without a newline is treated as the end of the log so an append in
/// progress is never read half-written.
#[derive(Debug)]
pub struct LogReader {
    reader: BufReader<File>,
    next_seq: Seq,
    buffer: Vec<u8>,
    done: bool,
}

impl LogReader {
    /// Opens `path` positioned at `from`.
    fn open(
        path: &Path,
        from: Seq,
    ) -> Result<Self, LogError> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut buffer = Vec::new();
        for _ in 1..from {
            buffer.clear();
            if reader.read_until(b'\n', &mut buffer)? == 0 {
                break;
            }
        }
        Ok(Self {
            reader,
            next_seq: from.max(1),
            buffer: Vec::new(),
            done: false,
        })
    }
}

impl Iterator for LogReader {
    type Item = Result<LogEntry, LogError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.buffer.clear();
        match self.reader.read_until(b'\n', &mut self.buffer) {
            Ok(0) => {
                self.done = true;
                None
            },
            Ok(_) if self.buffer.last() != Some(&b'\n') => {
                self.done = true;
                None
            },
            Ok(_) => {
                self.buffer.pop();
                match serde_json::from_slice::<Event>(&self.buffer) {
                    Ok(event) => {
                        let seq = self.next_seq;
                        self.next_seq += 1;
                        Some(Ok(LogEntry::new(seq, event)))
                    },
                    Err(error) => {
                        self.done = true;
                        Some(Err(error.into()))
                    },
                }
            },
            Err(error) => {
                self.done = true;
                Some(Err(error.into()))
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EventLog, LogError};
    use crate::{Event, EventBus, LogEntry};
    use serde_json::json;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    fn test_event(index: u64) -> Event {
        Event::new(format!("test.event.{index}"), json!({ "index": index }))
    }

    fn open_log(path: &Path) -> EventLog {
        EventLog::open(path).expect("log should open")
    }

    fn read_all(log: &EventLog) -> Vec<Event> {
        log.read_from(1)
            .expect("read should open")
            .map(|entry| entry.expect("entry should decode").event)
            .collect()
    }

    #[tokio::test]
    async fn publish_persists_and_fans_out() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");
        let log = open_log(&path);
        let mut subscriber = log.subscribe();

        let first = test_event(0);
        let second = test_event(1);
        assert_eq!(log.publish(first.clone()).await.expect("publish"), 1);
        assert_eq!(log.publish(second.clone()).await.expect("publish"), 2);

        assert_eq!(subscriber.recv().await, Ok(LogEntry::new(1, first.clone())));
        assert_eq!(
            subscriber.recv().await,
            Ok(LogEntry::new(2, second.clone()))
        );
        assert_eq!(read_all(&log), [first, second]);
    }

    #[tokio::test]
    async fn publish_persists_every_row_as_a_single_line() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");
        let log = open_log(&path);

        let first = test_event(0);
        let second = test_event(1);
        log.publish(first.clone()).await.expect("publish");
        log.publish(second.clone()).await.expect("publish");

        let contents = std::fs::read_to_string(&path).expect("log should be readable");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            serde_json::from_str::<Event>(lines[0]).expect("line should decode"),
            first
        );
        assert_eq!(
            serde_json::from_str::<Event>(lines[1]).expect("line should decode"),
            second
        );
    }

    #[tokio::test]
    async fn open_resumes_the_sequence_after_existing_lines() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");
        let first = test_event(0);
        let second = test_event(1);
        {
            let log = open_log(&path);
            log.publish(first.clone()).await.expect("publish");
        }

        let log = open_log(&path);
        assert_eq!(log.publish(second.clone()).await.expect("publish"), 2);
        assert_eq!(read_all(&log), [first, second]);
    }

    #[tokio::test]
    async fn open_truncates_a_partial_trailing_line() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");
        let first = test_event(0);
        let second = test_event(1);
        {
            let log = open_log(&path);
            log.publish(first.clone()).await.expect("publish");
        }
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("log should open");
            file.write_all(br#"{"id":"partial""#)
                .expect("partial write should succeed");
        }

        let log = open_log(&path);
        assert_eq!(log.publish(second.clone()).await.expect("publish"), 2);
        assert_eq!(read_all(&log), [first, second]);
    }

    #[tokio::test]
    async fn read_from_skips_earlier_entries() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");
        let log = open_log(&path);
        let events: Vec<Event> = (0..3).map(test_event).collect();
        for event in &events {
            log.publish(event.clone()).await.expect("publish");
        }

        let entries: Vec<LogEntry> = log
            .read_from(2)
            .expect("read should open")
            .map(|entry| entry.expect("entry should decode"))
            .collect();
        assert_eq!(
            entries,
            [
                LogEntry::new(2, events[1].clone()),
                LogEntry::new(3, events[2].clone())
            ]
        );
    }

    #[tokio::test]
    async fn tail_seq_tracks_the_last_committed_position() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let log = open_log(&dir.path().join("events.jsonl"));

        assert_eq!(log.tail_seq(), 0);
        log.publish(test_event(0)).await.expect("publish");
        log.publish(test_event(1)).await.expect("publish");
        assert_eq!(log.tail_seq(), 2);
    }

    #[tokio::test]
    async fn a_stopped_writer_fails_the_publish() {
        // A stopped writer is simulated by a queue with no receiver: `publish`
        // must surface the closed channel as `WriterStopped`.
        let inner = super::Inner {
            path: PathBuf::from("unused.jsonl"),
            writer: {
                let (sender, receiver) = tokio::sync::mpsc::channel(1);
                drop(receiver);
                sender
            },
            tail: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            bus: EventBus::default(),
        };
        let dead = EventLog {
            inner: std::sync::Arc::new(inner),
        };

        assert!(matches!(
            dead.publish(test_event(0)).await,
            Err(LogError::WriterStopped)
        ));
    }

    #[test]
    fn open_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("nested/events.jsonl");

        open_log(&path);

        assert!(Path::new(&path).exists());
    }

    #[tokio::test]
    async fn published_records_form_a_verifiable_chain() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");
        let log = open_log(&path);

        for index in 0..4 {
            log.publish(test_event(index)).await.expect("publish");
        }

        assert_eq!(log.verify_chain().expect("chain should verify"), 4);
        assert_eq!(super::verify_chain(&path).expect("chain should verify"), 4);
    }

    #[tokio::test]
    async fn editing_a_record_breaks_the_chain_at_its_position() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");
        let log = open_log(&path);
        for index in 0..3 {
            log.publish(test_event(index)).await.expect("publish");
        }
        drop(log);

        // Rewrite the payload of the second line, leaving its hash attribute.
        let contents = std::fs::read_to_string(&path).expect("read");
        let mut lines: Vec<String> = contents.lines().map(String::from).collect();
        lines[1] = lines[1].replace("\"index\":1", "\"index\":99");
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write");

        assert!(matches!(
            super::verify_chain(&path),
            Err(LogError::Chain { seq: 2, .. })
        ));
    }

    #[tokio::test]
    async fn reordering_records_breaks_the_chain() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");
        let log = open_log(&path);
        for index in 0..3 {
            log.publish(test_event(index)).await.expect("publish");
        }
        drop(log);

        let contents = std::fs::read_to_string(&path).expect("read");
        let mut lines: Vec<String> = contents.lines().map(String::from).collect();
        lines.swap(0, 1);
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write");

        assert!(matches!(
            super::verify_chain(&path),
            Err(LogError::Chain { seq: 1, .. })
        ));
    }

    #[tokio::test]
    async fn the_chain_resumes_across_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");

        {
            let log = open_log(&path);
            log.publish(test_event(0)).await.expect("publish");
            log.publish(test_event(1)).await.expect("publish");
        }
        {
            let log = open_log(&path);
            log.publish(test_event(2)).await.expect("publish");
            assert_eq!(log.verify_chain().expect("chain should verify"), 3);
        }
    }

    #[tokio::test]
    async fn a_record_without_chain_attributes_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let path = dir.path().join("events.jsonl");
        let event = test_event(0);
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&event).expect("serialize")),
        )
        .expect("write");

        assert!(matches!(
            super::verify_chain(&path),
            Err(LogError::Chain { seq: 1, .. })
        ));
    }
}
