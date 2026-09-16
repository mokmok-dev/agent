//! The node side of the choreography: a long-lived client that consumes the
//! daemon's event log and keeps a SQLite projection in sync with it.
//!
//! The daemon broadcasts every appended event to every subscriber over a
//! WebSocket on a Unix domain socket. A node subscribes from its last checkpoint,
//! decides which events it cares about with an [`Interest`], applies them to a
//! [`SqliteProjection`], and skips the rest. The JSONL event log remains the
//! source of truth; the SQLite file is a projection that can be rebuilt from it.
//!
//! ```no_run
//! use agentd_node::rusqlite::{Connection, Transaction};
//! use agentd_node::{LogEntry, Node, SqliteError, SqliteProjection, SqliteReducer, TypePrefixes};
//!
//! struct Counts;
//!
//! impl SqliteReducer for Counts {
//!     type Error = SqliteError;
//!
//!     fn migrate(conn: &Connection) -> Result<(), Self::Error> {
//!         conn.execute_batch("CREATE TABLE IF NOT EXISTS counts (type TEXT, n INTEGER);")?;
//!         Ok(())
//!     }
//!
//!     fn reduce(tx: &Transaction<'_>, entry: &LogEntry) -> Result<(), Self::Error> {
//!         tx.execute(
//!             "INSERT INTO counts (type, n) VALUES (?1, 1)",
//!             [&entry.event.r#type],
//!         )?;
//!         Ok(())
//!     }
//! }
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let projection = SqliteProjection::<Counts>::open("node.db")?;
//! let interest = TypePrefixes::new(["sandbox."]);
//! let mut node = Node::new("/tmp/mokmokd.sock", projection, interest, "urn:mokmokd:session:1");
//! let (sender, shutdown) = tokio::sync::watch::channel(false);
//! let _ = sender;
//! node.run(shutdown).await?;
//! # Ok(())
//! # }
//! ```

mod client;
mod filter;
mod node;
mod projection;

pub use agentd_events::{Event, LogEntry, Projection, Seq, WireMessage};
pub use client::{ClientError, WsClient};
pub use filter::{Interest, TypePrefixes};
pub use node::{Node, NodeError};
pub use projection::{SqliteError, SqliteProjection, SqliteReducer};
pub use rusqlite;
