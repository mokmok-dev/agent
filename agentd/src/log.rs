//! Durable, append-only JSONL persistence for the event bus.
//!
//! Under the event-sourced model the log is the source of truth:
//! [`EventLog::publish`] returns only after the event has been appended and
//! synced, so an acknowledged event cannot be lost. Live fanout through the
//! inner [`EventBus`] happens after the sync and in log order; a subscriber
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
//! are one-based line numbers, exposed as [`Lsn`].
//!
//! The log is at-least-once across a writer failure: an event that was written
//! but not acknowledged can survive a crash, so a publisher that retries may
//! append it twice. Consumers deduplicate by the stable [`Event::id`].

use agentd_events::{Event, EventBus};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};

/// The largest number of events committed in one group commit. Bounds the
/// memory a single batch can hold when the writer queue is large.
const MAX_BATCH_EVENTS: usize = 1024;

/// The default capacity of the live broadcast channel and of the writer queue.
const DEFAULT_CAPACITY: usize = 1024;

/// A one-based position in the event log.
pub type Lsn = u64;

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
}

/// An append-only JSONL log attached to a live [`EventBus`].
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
    bus: EventBus,
}

/// A publish awaiting its durable append.
#[derive(Debug)]
struct Pending {
    event: Event,
    ack: oneshot::Sender<Result<Lsn, LogError>>,
}

impl EventLog {
    /// Opens the log at `path`, recovers it, and starts the writer thread.
    ///
    /// The parent directory is created if needed. A trailing partial line left
    /// by a crash is truncated; complete lines are kept and the sequence number
    /// resumes after them.
    ///
    /// Only one `EventLog` may own a given `path` at a time; concurrent writers
    /// would interleave their appends.
    ///
    /// # Errors
    ///
    /// Returns [`LogError::Io`] if the file cannot be opened, recovered, or the
    /// writer thread cannot be spawned.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LogError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let next_lsn = recover(&path)?;
        let file = OpenOptions::new().append(true).open(&path)?;

        let bus = EventBus::new(DEFAULT_CAPACITY);
        let (writer, pending) = mpsc::channel(DEFAULT_CAPACITY);
        let thread_bus = bus.clone();
        std::thread::Builder::new()
            .name(String::from("eventlog"))
            .spawn(move || write_loop(file, next_lsn, pending, &thread_bus))?;

        Ok(Self {
            inner: Arc::new(Inner { path, writer, bus }),
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
    ) -> Result<Lsn, LogError> {
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
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.bus.subscribe()
    }

    /// Reads the log from `from` (inclusive) to the end.
    ///
    /// # Errors
    ///
    /// Returns [`LogError::Io`] if the file cannot be opened.
    pub fn read_from(
        &self,
        from: Lsn,
    ) -> Result<LogReader, LogError> {
        LogReader::open(&self.inner.path, from)
    }
}

/// Recovers `path` and returns the next free [`Lsn`].
///
/// Creates an empty file when `path` is missing. When the file does not end in
/// a newline, the trailing bytes are a partial append from a crash and are
/// truncated. The file is streamed, so recovery does not load the log into
/// memory.
fn recover(path: &Path) -> Result<Lsn, LogError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    let (complete_end, lines) = {
        let mut reader = BufReader::new(&file);
        let mut buffer = Vec::new();
        let mut complete_end: u64 = 0;
        let mut lines: u64 = 0;
        loop {
            buffer.clear();
            let read = reader.read_until(b'\n', &mut buffer)?;
            if read == 0 || buffer.last() != Some(&b'\n') {
                break;
            }
            complete_end += read as u64;
            lines += 1;
        }
        (complete_end, lines)
    };

    if complete_end < file.metadata()?.len() {
        file.set_len(complete_end)?;
        file.sync_all()?;
    }
    Ok(lines + 1)
}

/// Drains pending publishes into batches, commits each batch, and fans it out.
fn write_loop(
    mut file: File,
    mut next_lsn: Lsn,
    mut pending: mpsc::Receiver<Pending>,
    bus: &EventBus,
) {
    while let Some(first) = pending.blocking_recv() {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH_EVENTS {
            match pending.try_recv() {
                Ok(item) => batch.push(item),
                Err(_) => break,
            }
        }

        if let Err(error) = commit(&mut file, &mut next_lsn, batch, bus) {
            tracing::error!(%error, "event log write failed; stopping the writer");
            return;
        }
    }
}

/// Appends `batch` in one write, syncs once, then fans out in log order.
fn commit(
    file: &mut File,
    next_lsn: &mut Lsn,
    batch: Vec<Pending>,
    bus: &EventBus,
) -> Result<(), LogError> {
    let mut buffer = String::new();
    let mut accepted = Vec::with_capacity(batch.len());
    for item in batch {
        match serde_json::to_string(&item.event) {
            Ok(line) => {
                buffer.push_str(&line);
                buffer.push('\n');
                accepted.push((*next_lsn, item));
                *next_lsn += 1;
            },
            Err(error) => {
                let _ = item.ack.send(Err(LogError::Json(error)));
            },
        }
    }

    file.write_all(buffer.as_bytes())?;
    file.sync_data()?;

    for (lsn, item) in accepted {
        bus.publish(item.event);
        let _ = item.ack.send(Ok(lsn));
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
    next_lsn: Lsn,
    buffer: Vec<u8>,
    done: bool,
}

impl LogReader {
    /// Opens `path` positioned at `from`.
    fn open(
        path: &Path,
        from: Lsn,
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
            next_lsn: from.max(1),
            buffer: Vec::new(),
            done: false,
        })
    }
}

impl Iterator for LogReader {
    type Item = Result<(Lsn, Event), LogError>;

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
                        let lsn = self.next_lsn;
                        self.next_lsn += 1;
                        Some(Ok((lsn, event)))
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
    use super::{EventLog, LogError, Lsn};
    use agentd_events::{Event, EventBus};
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
            .map(|entry| entry.expect("entry should decode").1)
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

        assert_eq!(subscriber.recv().await.ok().as_ref(), Some(&first));
        assert_eq!(subscriber.recv().await.ok().as_ref(), Some(&second));
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

        let entries: Vec<(Lsn, Event)> = log
            .read_from(2)
            .expect("read should open")
            .map(|entry| entry.expect("entry should decode"))
            .collect();
        assert_eq!(entries, [(2, events[1].clone()), (3, events[2].clone())]);
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
}