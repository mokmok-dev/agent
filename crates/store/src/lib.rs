//! The event store. One SQLite database, append-only by construction, verified
//! by the hash the core computed.
//!
//! The store never re-serializes an event. It writes the canonical bytes the
//! core produced and keeps them, so verification is a hash of the stored bytes
//! rather than a re-encoding that could drift from the writer.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use agent_core::event::Event;
use agent_core::ids::{BranchId, EventId};
use rusqlite::{Connection, Transaction, params};

mod decode;

pub use decode::{DecodeError, decode};

/// The schema, verbatim. Re-running it is safe, so an open repairs an empty
/// database and does nothing to a populated one.
const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY,
    branch TEXT NOT NULL,
    depth INTEGER NOT NULL,
    prev TEXT,
    type TEXT NOT NULL,
    time TEXT NOT NULL,
    actor TEXT NOT NULL,
    body TEXT NOT NULL,
    UNIQUE (branch, depth)
) STRICT;
CREATE INDEX IF NOT EXISTS events_by_prev ON events (prev);
CREATE TRIGGER IF NOT EXISTS events_no_update BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;
CREATE TRIGGER IF NOT EXISTS events_no_delete BEFORE DELETE ON events
BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;
INSERT OR IGNORE INTO meta (key, value) VALUES ('schema', '1');
";

/// Why an append or a verification failed.
#[derive(Debug)]
pub enum StoreError {
    /// The database could not be opened.
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },
    /// A statement failed.
    Query(rusqlite::Error),
    /// A stored identifier is not the hex this build writes.
    Identifier(String),
    /// The event's predecessor is not the branch's tip.
    PrevMismatch {
        branch: BranchId,
        expected: Option<EventId>,
        found: Option<EventId>,
    },
    /// The event's depth is not one past the branch's depth.
    DepthGap {
        branch: BranchId,
        expected: u64,
        found: u64,
    },
    /// The store already has a genesis, so a second root is refused.
    SecondGenesis(BranchId),
    /// A branch's first event names an anchor that is not a stored event.
    UnknownPredecessor { branch: BranchId, prev: EventId },
    /// A stored event no longer matches its own identifier.
    Chain { id: EventId, detail: &'static str },
    /// A stored body could not be decoded into an event.
    Decode(DecodeError),
}

impl StoreError {
    /// Whether the failure is the data disagreeing with what the writer
    /// promised, as opposed to a store that could not be read.
    #[must_use]
    pub const fn is_corrupt(&self) -> bool {
        matches!(
            self,
            Self::Identifier(_)
                | Self::PrevMismatch { .. }
                | Self::DepthGap { .. }
                | Self::SecondGenesis(_)
                | Self::UnknownPredecessor { .. }
                | Self::Chain { .. }
                | Self::Decode(_)
        )
    }
}

impl fmt::Display for StoreError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::Open { path, source } => {
                write!(formatter, "cannot open {}: {source}", path.display())
            },
            Self::Query(source) => write!(formatter, "query failed: {source}"),
            Self::Identifier(text) => write!(formatter, "not an identifier: {text}"),
            Self::PrevMismatch {
                branch,
                expected,
                found,
            } => write!(
                formatter,
                "branch {branch}: expected predecessor {expected:?}, got {found:?}"
            ),
            Self::DepthGap {
                branch,
                expected,
                found,
            } => write!(
                formatter,
                "branch {branch}: expected depth {expected}, got {found}"
            ),
            Self::SecondGenesis(branch) => {
                write!(
                    formatter,
                    "branch {branch}: the store already has a genesis"
                )
            },
            Self::UnknownPredecessor { branch, prev } => {
                write!(
                    formatter,
                    "branch {branch}: predecessor {prev} is not stored"
                )
            },
            Self::Chain { id, detail } => write!(formatter, "event {id} failed {detail}"),
            Self::Decode(error) => write!(formatter, "cannot decode a stored event: {error}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Query(source)
    }
}

/// What a verification found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyReport {
    pub events: u64,
    pub branches: usize,
}

/// One database, one writer.
#[derive(Debug)]
pub struct Store {
    connection: Connection,
    path: PathBuf,
}

impl Store {
    /// Opens a store, creating the schema on first use.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Open`] when the file cannot be opened and
    /// [`StoreError::Query`] when the schema cannot be applied, which in
    /// practice means a file that is not a database.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let connection = Connection::open(path).map_err(|source| StoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self {
            connection,
            path: path.to_path_buf(),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens a store that already exists, without writing to it.
    ///
    /// A read-only command must not create the database it was pointed at, and
    /// must not repair one, because either would change what the operator is
    /// about to inspect.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Open`] when the file does not exist or cannot be
    /// opened for reading.
    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(
                |source| StoreError::Open {
                    path: path.to_path_buf(),
                    source,
                },
            )?;
        Ok(Self {
            connection,
            path: path.to_path_buf(),
        })
    }

    /// Appends one event, refusing anything that would break the chain.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::PrevMismatch`] when the predecessor is not the
    /// branch's tip, [`StoreError::DepthGap`] when the depth is not one past it,
    /// [`StoreError::SecondGenesis`] when a second root is appended,
    /// [`StoreError::UnknownPredecessor`] when a branch's first event names an
    /// anchor that is not stored, and [`StoreError::Query`] when the insert
    /// itself fails.
    pub fn append(
        &mut self,
        event: &Event,
    ) -> Result<EventId, StoreError> {
        let body = agent_core::canonical::canonical(event);
        let transaction = self.connection.transaction()?;
        check_position(&transaction, event)?;
        transaction.execute(
            "INSERT INTO events (id, branch, depth, prev, type, time, actor, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                event.id().to_string(),
                event.branch.to_string(),
                event.depth,
                event.prev.map(|prev| prev.to_string()),
                event.type_name(),
                event.time.rfc3339(),
                event.actor.to_string(),
                body,
            ],
        )?;
        transaction.commit()?;
        Ok(event.id())
    }

    /// The stored bytes of one branch, in depth order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] when the read fails and
    /// [`StoreError::Identifier`] when a stored id is not hex.
    pub fn bodies(
        &self,
        branch: BranchId,
    ) -> Result<Vec<(EventId, String)>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, body FROM events WHERE branch = ?1 ORDER BY depth")?;
        let mut rows = statement.query(params![branch.to_string()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let body: String = row.get(1)?;
            out.push((
                EventId::from_str(&id).map_err(|_| StoreError::Identifier(id))?,
                body,
            ));
        }
        Ok(out)
    }

    /// Every branch id in the store, in id order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] when the read fails and
    /// [`StoreError::Identifier`] when a stored branch id is not hex.
    pub fn branches(&self) -> Result<Vec<BranchId>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT branch FROM events ORDER BY branch")?;
        let mut rows = statement.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let text: String = row.get(0)?;
            let Ok(branch) = BranchId::from_str(&text) else {
                return Err(StoreError::Identifier(text));
            };
            out.push(branch);
        }
        Ok(out)
    }

    /// One branch, decoded into typed events, in depth order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] when the read fails and
    /// [`StoreError::Decode`] when a stored body cannot be read, which means
    /// the store and this build disagree about the format.
    pub fn load(
        &self,
        branch: BranchId,
    ) -> Result<Vec<Event>, StoreError> {
        let bodies = self.bodies(branch)?;
        let mut events = Vec::with_capacity(bodies.len());
        for (_, body) in bodies {
            events.push(decode(&body).map_err(StoreError::Decode)?);
        }
        Ok(events)
    }

    /// Walks every stored event and re-derives what the writer promised.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Chain`] when a body no longer hashes to its id,
    /// when a branch's predecessor or depth does not follow its own row, when
    /// the store does not have exactly one genesis at depth zero, or when a
    /// branch's fork anchor does not resolve one depth past its event.
    pub fn verify(&self) -> Result<VerifyReport, StoreError> {
        let (report, anchors) = self.scan()?;
        verify_anchors(&self.connection, &anchors)?;
        Ok(report)
    }

    fn scan(&self) -> Result<(VerifyReport, Vec<Anchor>), StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, branch, depth, prev, body FROM events ORDER BY branch, depth")?;
        let mut rows = statement.query([])?;
        let mut events = 0_u64;
        let mut branches = 0_usize;
        let mut current: Option<(BranchId, u64, String)> = None;
        let mut anchors = Vec::new();

        while let Some(row) = rows.next()? {
            let id_text: String = row.get(0)?;
            let branch_text: String = row.get(1)?;
            let depth: u64 = row.get(2)?;
            let prev_text: Option<String> = row.get(3)?;
            let body: String = row.get(4)?;

            let Ok(id) = EventId::from_str(&id_text) else {
                return Err(StoreError::Identifier(id_text));
            };
            let Ok(branch) = BranchId::from_str(&branch_text) else {
                return Err(StoreError::Identifier(branch_text));
            };

            let computed = EventId::from_bytes(*blake3::hash(body.as_bytes()).as_bytes());
            if computed != id {
                return Err(StoreError::Chain {
                    id,
                    detail: "its own hash",
                });
            }

            match &current {
                Some((current_branch, current_depth, current_id)) if *current_branch == branch => {
                    if prev_text.as_deref() != Some(current_id.as_str()) {
                        return Err(StoreError::Chain {
                            id,
                            detail: "the chain",
                        });
                    }
                    if depth != current_depth + 1 {
                        return Err(StoreError::Chain {
                            id,
                            detail: "the depth order",
                        });
                    }
                },
                _ => {
                    branches += 1;
                    let prev = prev_text
                        .as_deref()
                        .map(|text| {
                            EventId::from_str(text)
                                .map_err(|_| StoreError::Identifier(text.to_string()))
                        })
                        .transpose()?;
                    anchors.push(Anchor { id, depth, prev });
                },
            }

            current = Some((branch, depth, id_text));
            events += 1;
        }

        Ok((VerifyReport { events, branches }, anchors))
    }
}

/// The first row of one branch: the genesis, or the fork anchor it hangs from.
struct Anchor {
    id: EventId,
    depth: u64,
    prev: Option<EventId>,
}

fn verify_anchors(
    connection: &Connection,
    anchors: &[Anchor],
) -> Result<(), StoreError> {
    let mut genesis: Option<&Anchor> = None;
    for anchor in anchors {
        if anchor.prev.is_some() {
            continue;
        }
        if genesis.is_some() {
            return Err(StoreError::Chain {
                id: anchor.id,
                detail: "the genesis",
            });
        }
        genesis = Some(anchor);
    }
    if let Some(anchor) = genesis
        && anchor.depth != 0
    {
        return Err(StoreError::Chain {
            id: anchor.id,
            detail: "the genesis depth",
        });
    }

    // A non-empty store with no genesis cannot pass the checks below: every
    // anchor must sit one depth past its target, so anchors cannot form a
    // cycle and some branch must end at a prev-less row.
    let mut statement = connection.prepare("SELECT depth FROM events WHERE id = ?1")?;
    for anchor in anchors {
        let Some(prev) = anchor.prev else {
            continue;
        };
        let mut rows = statement.query(params![prev.to_string()])?;
        let Some(row) = rows.next()? else {
            return Err(StoreError::Chain {
                id: anchor.id,
                detail: "the fork anchor",
            });
        };
        let depth: u64 = row.get(0)?;
        if Some(anchor.depth) != depth.checked_add(1) {
            return Err(StoreError::Chain {
                id: anchor.id,
                detail: "the fork depth",
            });
        }
    }
    Ok(())
}

fn check_position(
    transaction: &Transaction<'_>,
    event: &Event,
) -> Result<(), StoreError> {
    let tip = {
        let mut statement = transaction.prepare(
            "SELECT depth, id FROM events WHERE branch = ?1 ORDER BY depth DESC LIMIT 1",
        )?;
        let mut rows = statement.query(params![event.branch.to_string()])?;
        match rows.next()? {
            Some(row) => {
                let depth: u64 = row.get(0)?;
                let id_text: String = row.get(1)?;
                Some((depth, id_text))
            },
            None => None,
        }
    };

    let Some((depth, id_text)) = tip else {
        return check_anchor(transaction, event);
    };

    let tip = EventId::from_str(&id_text).map_err(|_| StoreError::Identifier(id_text))?;
    if event.prev != Some(tip) {
        return Err(StoreError::PrevMismatch {
            branch: event.branch,
            expected: Some(tip),
            found: event.prev,
        });
    }
    let expected = depth.saturating_add(1);
    if event.depth != expected {
        return Err(StoreError::DepthGap {
            branch: event.branch,
            expected,
            found: event.depth,
        });
    }
    Ok(())
}

/// A branch with no rows yet either establishes the one genesis or forks from
/// an event that is already stored.
fn check_anchor(
    transaction: &Transaction<'_>,
    event: &Event,
) -> Result<(), StoreError> {
    let Some(prev) = event.prev else {
        let mut statement = transaction.prepare("SELECT EXISTS (SELECT 1 FROM events)")?;
        let stored: bool = statement.query_row([], |row| row.get(0))?;
        if stored {
            return Err(StoreError::SecondGenesis(event.branch));
        }
        if event.depth != 0 {
            return Err(StoreError::DepthGap {
                branch: event.branch,
                expected: 0,
                found: event.depth,
            });
        }
        return Ok(());
    };

    let mut statement = transaction.prepare("SELECT depth FROM events WHERE id = ?1")?;
    let mut rows = statement.query(params![prev.to_string()])?;
    let Some(row) = rows.next()? else {
        return Err(StoreError::UnknownPredecessor {
            branch: event.branch,
            prev,
        });
    };
    let depth: u64 = row.get(0)?;
    let expected = depth.saturating_add(1);
    if event.depth != expected {
        return Err(StoreError::DepthGap {
            branch: event.branch,
            expected,
            found: event.depth,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
