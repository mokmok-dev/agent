//! The operator command surface, declared with `clap` and handed on as [`Intent`].
//!
//! `clap` owns the parts a first draft gets wrong: `--flag=value`, `--` as a
//! terminator, an option given twice, and a group where exactly one member may
//! be present. Everything past [`parse`] sees [`Intent`], never a `clap` type,
//! so the exit table and the error object stay hand-written in [`crate::exit`].

use std::ffi::OsString;
use std::path::PathBuf;
use std::str::FromStr;

use agent_core::ids::{BranchId, EventId, TurnId};
use agent_core::time::Timestamp;
use clap::{ArgGroup, Args, Parser, Subcommand, error::ErrorKind};

use crate::exit::{Class, Exit, Failure};

/// The arguments, read.
#[derive(Debug, Parser)]
#[command(
    name = "agentctl",
    version,
    about = "Operate a running agent.",
    long_about = "Operate a running agent. Commands that change history send one \
                  command to agentd over bus.sock; commands that only read open the \
                  store directly."
)]
struct Cli {
    /// Where the store lives. Defaults to the supervisor's state directory.
    #[arg(long, global = true, value_name = "PATH")]
    state_dir: Option<PathBuf>,
    /// Machine output, one object per invocation.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Tip ages, active branches, pending approvals, provider health, store size.
    Status,
    /// Branches, their depth, and their last outcome.
    Branch {
        #[command(subcommand)]
        command: BranchCommand,
    },
    /// Create a branch at the chosen point.
    Fork(Fork),
    /// Request a turn.
    Send(Send),
    /// Decide a pending approval in the affirmative.
    Approve {
        /// The pending approval.
        id: String,
    },
    /// Decide a pending approval in the negative.
    Deny {
        /// The pending approval.
        id: String,
    },
    /// Stop scheduling a branch.
    Archive {
        /// The branch to stop scheduling.
        #[arg(long)]
        branch: String,
    },
    /// Refresh the provider cache for a branch's prefix.
    Warm {
        /// The branch whose prefix to send again.
        #[arg(long)]
        branch: String,
        /// Wait for the warm request instead of returning when it is accepted.
        #[arg(long)]
        wait: bool,
    },
    /// Print the folded state transitions of one branch.
    Replay {
        /// The branch to fold.
        #[arg(long)]
        branch: String,
        /// The first event to print.
        #[arg(long)]
        from: Option<String>,
        /// The last event to print.
        #[arg(long)]
        to: Option<String>,
    },
    /// The effect ledger derived from events.
    Audit {
        /// The branch to audit.
        #[arg(long)]
        branch: String,
        /// The instant the ledger starts.
        #[arg(long)]
        since: String,
    },
    /// `CloudEvents` JSON lines for one branch.
    Export {
        /// The branch to export.
        #[arg(long)]
        branch: String,
        /// The file to write.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
    },
    /// Chain, depth, and database integrity.
    Verify {
        /// Re-derive every body instead of trusting the chain alone.
        #[arg(long)]
        deep: bool,
    },
    /// Diagnose the store and report the failing row.
    Doctor {
        /// Watch the liveness file and report a daemon that has stopped.
        #[arg(long)]
        deadman: bool,
    },
    /// Write a consistent copy while the service runs.
    Backup {
        /// The file to write.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
    },
    /// Delete unreferenced blobs.
    Gc {
        /// Report what would be deleted without deleting it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Write and enable the supervisor's unit or plist.
    #[command(group(ArgGroup::new("supervisor").required(true).multiple(false)))]
    Install {
        /// Write and enable the systemd unit.
        #[arg(long, group = "supervisor")]
        systemd: bool,
        /// Write and enable the launchd plist.
        #[arg(long, group = "supervisor")]
        launchd: bool,
    },
    /// The Open Knowledge Format bundle.
    Spec {
        #[command(subcommand)]
        command: SpecCommand,
    },
    /// Binary version, store format, and git describe.
    Version,
}

#[derive(Debug, Subcommand)]
enum BranchCommand {
    /// Every branch, with its depth and tip time.
    List,
    /// One branch, with its last turns.
    Show {
        /// The branch to show.
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum SpecCommand {
    /// Validate the bundle under docs/spec.
    Check,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("point").required(true).multiple(false)))]
struct Fork {
    /// The branch the new one starts from.
    #[arg(long)]
    branch: String,
    /// The event the new branch starts at.
    #[arg(long, group = "point")]
    at: Option<String>,
    /// How many events back from the tip to start at.
    #[arg(long, group = "point")]
    rewind: Option<u32>,
    /// The turn the new branch starts at.
    #[arg(long, group = "point")]
    turn: Option<String>,
    /// A human label for the new branch.
    #[arg(long)]
    label: Option<String>,
    /// Snap the fork to the nearest prompt segment edge.
    #[arg(long)]
    boundary: bool,
}

#[derive(Debug, Args)]
struct Send {
    /// The branch the turn belongs to.
    #[arg(long)]
    branch: String,
    /// What the operator says.
    message: String,
    /// Wait for the turn to finish instead of returning when it is accepted.
    #[arg(long)]
    wait: bool,
}

/// Where a fork starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPoint {
    At(EventId),
    Rewind(u32),
    Turn(TurnId),
}

/// Which supervisor to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Supervisor {
    Systemd,
    Launchd,
}

/// What the operator asked for, in this project's own types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Status,
    BranchList,
    BranchShow {
        branch: BranchId,
    },
    Fork {
        parent: BranchId,
        point: ForkPoint,
        label: Option<String>,
        boundary: bool,
    },
    Send {
        branch: BranchId,
        message: String,
        wait: bool,
    },
    Approve {
        id: String,
    },
    Deny {
        id: String,
    },
    Archive {
        branch: BranchId,
    },
    Warm {
        branch: BranchId,
        wait: bool,
    },
    Replay {
        branch: BranchId,
        from: Option<EventId>,
        to: Option<EventId>,
    },
    Audit {
        branch: BranchId,
        since: Timestamp,
    },
    Export {
        branch: BranchId,
        out: PathBuf,
    },
    Verify {
        deep: bool,
    },
    Doctor {
        deadman: bool,
    },
    Backup {
        out: PathBuf,
    },
    Gc {
        dry_run: bool,
    },
    Install {
        supervisor: Supervisor,
    },
    SpecCheck,
    Version,
}

impl Intent {
    /// The command as the interface spec names it.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::BranchList => "branch list",
            Self::BranchShow { .. } => "branch show",
            Self::Fork { .. } => "fork",
            Self::Send { .. } => "send",
            Self::Approve { .. } => "approve",
            Self::Deny { .. } => "deny",
            Self::Archive { .. } => "archive",
            Self::Warm { .. } => "warm",
            Self::Replay { .. } => "replay",
            Self::Audit { .. } => "audit",
            Self::Export { .. } => "export",
            Self::Verify { .. } => "verify",
            Self::Doctor { .. } => "doctor",
            Self::Backup { .. } => "backup",
            Self::Gc { .. } => "gc",
            Self::Install { .. } => "install",
            Self::SpecCheck => "spec check",
            Self::Version => "version",
        }
    }

    /// What this build would need in order to run it.
    #[must_use]
    pub const fn needs(&self) -> &'static str {
        match self {
            Self::Status => "the daemon's status report over bus.sock",
            Self::BranchList => "the branch list over bus.sock",
            Self::BranchShow { .. } => "one branch, read over bus.sock",
            Self::Fork { .. } => "the daemon's fork command over bus.sock",
            Self::Send { .. } => "the command socket at bus.sock",
            Self::Approve { .. } | Self::Deny { .. } => "the approval queue over bus.sock",
            Self::Archive { .. } => "the daemon's archive command over bus.sock",
            Self::Warm { .. } => "the provider adapter's warm request path",
            Self::Replay { .. } => "a fold the runtime exposes as text",
            Self::Audit { .. } => "the effect ledger",
            Self::Export { .. } => "the CloudEvents exporter",
            Self::Verify { .. } | Self::Version => "nothing; this command is implemented",
            Self::Doctor { .. } => "the store diagnostics",
            Self::Backup { .. } => "SQLite's online backup",
            Self::Gc { .. } => "blob storage, which the store does not write yet",
            Self::Install { .. } => "the unit and plist templates",
            Self::SpecCheck => "the spec checker, which lives in xtask",
        }
    }
}

/// One invocation: the global flags and the command they apply to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub state_dir: Option<PathBuf>,
    pub json: bool,
    pub intent: Intent,
}

/// What reading the arguments produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// A command to run.
    Invocation(Invocation),
    /// Help or the version banner, already rendered by `clap`.
    Text(String),
}

/// Reads the arguments, including the program name.
///
/// # Errors
///
/// Returns a [`Failure`] with [`Exit::Usage`] when `clap` rejects the arguments
/// or when an identifier in them does not parse.
pub fn parse<I, T>(args: I) -> Result<Parsed, Failure>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match Cli::try_parse_from(args) {
        Ok(cli) => Invocation::from_cli(cli).map(Parsed::Invocation),
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                Ok(Parsed::Text(error.to_string()))
            },
            _ => Err(Failure::usage(error.to_string())),
        },
    }
}

impl Invocation {
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per leaf command, and splitting it would put half the surface in another table"
    )]
    fn from_cli(cli: Cli) -> Result<Self, Failure> {
        let intent = match cli.command {
            Command::Status => Intent::Status,
            Command::Branch { command } => match command {
                BranchCommand::List => Intent::BranchList,
                BranchCommand::Show { id } => Intent::BranchShow {
                    branch: branch("id", &id)?,
                },
            },
            Command::Fork(fork) => Intent::Fork {
                parent: branch("--branch", &fork.branch)?,
                point: fork_point(&fork)?,
                label: fork.label,
                boundary: fork.boundary,
            },
            Command::Send(send) => Intent::Send {
                branch: branch("--branch", &send.branch)?,
                message: send.message,
                wait: send.wait,
            },
            Command::Approve { id } => Intent::Approve { id },
            Command::Deny { id } => Intent::Deny { id },
            Command::Archive { branch: id } => Intent::Archive {
                branch: branch("--branch", &id)?,
            },
            Command::Warm { branch: id, wait } => Intent::Warm {
                branch: branch("--branch", &id)?,
                wait,
            },
            Command::Replay {
                branch: id,
                from,
                to,
            } => Intent::Replay {
                branch: branch("--branch", &id)?,
                from: from
                    .as_deref()
                    .map(|text| event("--from", text))
                    .transpose()?,
                to: to.as_deref().map(|text| event("--to", text)).transpose()?,
            },
            Command::Audit { branch: id, since } => Intent::Audit {
                branch: branch("--branch", &id)?,
                since: Timestamp::parse(&since).map_err(|error| {
                    bad(
                        "--since",
                        &since,
                        &format!("an RFC 3339 instant, not {error}"),
                    )
                })?,
            },
            Command::Export { branch: id, out } => Intent::Export {
                branch: branch("--branch", &id)?,
                out,
            },
            Command::Verify { deep } => Intent::Verify { deep },
            Command::Doctor { deadman } => Intent::Doctor { deadman },
            Command::Backup { out } => Intent::Backup { out },
            Command::Gc { dry_run } => Intent::Gc { dry_run },
            Command::Install { systemd, .. } => Intent::Install {
                supervisor: if systemd {
                    Supervisor::Systemd
                } else {
                    Supervisor::Launchd
                },
            },
            Command::Spec { command } => match command {
                SpecCommand::Check => Intent::SpecCheck,
            },
            Command::Version => Intent::Version,
        };
        Ok(Self {
            state_dir: cli.state_dir,
            json: cli.json,
            intent,
        })
    }
}

fn fork_point(fork: &Fork) -> Result<ForkPoint, Failure> {
    match (&fork.at, fork.rewind, &fork.turn) {
        (Some(at), _, _) => Ok(ForkPoint::At(event("--at", at)?)),
        (_, Some(count), _) => Ok(ForkPoint::Rewind(count)),
        (_, _, Some(turn)) => {
            Ok(ForkPoint::Turn(TurnId::from_str(turn).map_err(|_| {
                bad("--turn", turn, "a turn id, which is lowercase hex")
            })?))
        },
        (None, None, None) => Err(Failure::usage(
            "fork needs one of --at, --rewind, or --turn".to_string(),
        )),
    }
}

fn branch(
    field: &str,
    text: &str,
) -> Result<BranchId, Failure> {
    BranchId::from_str(text).map_err(|_| {
        bad(
            field,
            text,
            "a branch id, which looks like `b-0123456789abcdef`",
        )
    })
}

fn event(
    field: &str,
    text: &str,
) -> Result<EventId, Failure> {
    EventId::from_str(text).map_err(|_| {
        bad(
            field,
            text,
            "an event id, which is 64 lowercase hex characters",
        )
    })
}

fn bad(
    field: &str,
    text: &str,
    expected: &str,
) -> Failure {
    let mut failure = Failure::usage(format!("{field} takes {expected}; got `{text}`"));
    failure.class = Class::Usage;
    failure.exit = Exit::Usage;
    failure
}

#[cfg(test)]
mod tests {
    use super::{ForkPoint, Intent, Invocation, Parsed, Supervisor, parse};
    use agent_core::ids::{BranchId, EventId, TurnId};
    use agent_core::time::Timestamp;
    use std::path::PathBuf;
    use std::str::FromStr;

    fn intent(args: &[&str]) -> Intent {
        let mut all = vec!["agentctl"];
        all.extend_from_slice(args);
        match parse(all).expect("parses") {
            Parsed::Invocation(invocation) => invocation.intent,
            Parsed::Text(text) => panic!("expected a command, got {text}"),
        }
    }

    fn failure(args: &[&str]) -> super::Failure {
        let mut all = vec!["agentctl"];
        all.extend_from_slice(args);
        parse(all).expect_err("refused")
    }

    fn branch(text: &str) -> BranchId {
        BranchId::from_str(text).expect("a branch id")
    }

    fn event(text: &str) -> EventId {
        EventId::from_str(text).expect("an event id")
    }

    const B: &str = "b-0123456789abcdef";
    const E: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const T: &str = "0123456789abcdef";

    #[test]
    fn every_leaf_command_becomes_its_own_intent() {
        let table = [
            (vec!["status"], Intent::Status),
            (vec!["branch", "list"], Intent::BranchList),
            (
                vec!["branch", "show", B],
                Intent::BranchShow { branch: branch(B) },
            ),
            (
                vec!["fork", "--branch", B, "--at", E],
                Intent::Fork {
                    parent: branch(B),
                    point: ForkPoint::At(event(E)),
                    label: None,
                    boundary: false,
                },
            ),
            (
                vec!["send", "--branch", B, "hello"],
                Intent::Send {
                    branch: branch(B),
                    message: "hello".to_string(),
                    wait: false,
                },
            ),
            (
                vec!["approve", "approval-1"],
                Intent::Approve {
                    id: "approval-1".to_string(),
                },
            ),
            (
                vec!["deny", "approval-1"],
                Intent::Deny {
                    id: "approval-1".to_string(),
                },
            ),
            (
                vec!["archive", "--branch", B],
                Intent::Archive { branch: branch(B) },
            ),
            (
                vec!["warm", "--branch", B],
                Intent::Warm {
                    branch: branch(B),
                    wait: false,
                },
            ),
            (
                vec!["replay", "--branch", B],
                Intent::Replay {
                    branch: branch(B),
                    from: None,
                    to: None,
                },
            ),
            (
                vec!["audit", "--branch", B, "--since", "2026-10-26T00:00:00Z"],
                Intent::Audit {
                    branch: branch(B),
                    since: Timestamp::parse("2026-10-26T00:00:00Z").expect("an instant"),
                },
            ),
            (
                vec!["export", "--branch", B, "--out", "/tmp/out.jsonl"],
                Intent::Export {
                    branch: branch(B),
                    out: PathBuf::from("/tmp/out.jsonl"),
                },
            ),
            (vec!["verify"], Intent::Verify { deep: false }),
            (vec!["doctor"], Intent::Doctor { deadman: false }),
            (
                vec!["backup", "--out", "/tmp/copy.db"],
                Intent::Backup {
                    out: PathBuf::from("/tmp/copy.db"),
                },
            ),
            (vec!["gc"], Intent::Gc { dry_run: false }),
            (
                vec!["install", "--systemd"],
                Intent::Install {
                    supervisor: Supervisor::Systemd,
                },
            ),
            (vec!["spec", "check"], Intent::SpecCheck),
            (vec!["version"], Intent::Version),
        ];
        assert_eq!(table.len(), 19);
        for (args, expected) in table {
            assert_eq!(intent(&args), expected, "{args:?}");
        }
    }

    #[test]
    fn the_flags_that_carry_a_value_are_kept() {
        assert_eq!(
            intent(&[
                "fork",
                "--branch",
                B,
                "--rewind",
                "3",
                "--label",
                "experiment",
                "--boundary"
            ]),
            Intent::Fork {
                parent: branch(B),
                point: ForkPoint::Rewind(3),
                label: Some("experiment".to_string()),
                boundary: true,
            }
        );
        assert_eq!(
            intent(&["send", "--branch", B, "--wait", "hello"]),
            Intent::Send {
                branch: branch(B),
                message: "hello".to_string(),
                wait: true,
            }
        );
        assert_eq!(
            intent(&["warm", "--branch", B, "--wait"]),
            Intent::Warm {
                branch: branch(B),
                wait: true,
            }
        );
        assert_eq!(
            intent(&["replay", "--branch", B, "--from", E, "--to", E]),
            Intent::Replay {
                branch: branch(B),
                from: Some(event(E)),
                to: Some(event(E)),
            }
        );
        assert_eq!(intent(&["verify", "--deep"]), Intent::Verify { deep: true });
        assert_eq!(
            intent(&["doctor", "--deadman"]),
            Intent::Doctor { deadman: true }
        );
        assert_eq!(intent(&["gc", "--dry-run"]), Intent::Gc { dry_run: true });
        assert_eq!(
            intent(&["install", "--launchd"]),
            Intent::Install {
                supervisor: Supervisor::Launchd,
            }
        );
    }

    #[test]
    fn a_fork_selector_of_each_kind_is_read() {
        assert_eq!(
            intent(&["fork", "--branch", B, "--turn", T]),
            Intent::Fork {
                parent: branch(B),
                point: ForkPoint::Turn(TurnId::from_str(T).expect("a turn id")),
                label: None,
                boundary: false,
            }
        );
    }

    #[test]
    fn the_global_flags_apply_whether_they_come_first_or_last() {
        let invocation = match parse([
            "agentctl",
            "--json",
            "--state-dir",
            "/var/lib/agent",
            "status",
        ])
        .expect("parses")
        {
            Parsed::Invocation(invocation) => invocation,
            Parsed::Text(text) => panic!("expected a command, got {text}"),
        };
        assert_eq!(
            invocation,
            Invocation {
                state_dir: Some(PathBuf::from("/var/lib/agent")),
                json: true,
                intent: Intent::Status,
            }
        );
        let trailing = match parse(["agentctl", "status", "--json"]).expect("parses") {
            Parsed::Invocation(invocation) => invocation,
            Parsed::Text(text) => panic!("expected a command, got {text}"),
        };
        assert!(trailing.json);
        assert_eq!(trailing.state_dir, None);
    }

    #[test]
    fn a_flag_may_carry_its_value_with_an_equals_sign() {
        let invocation =
            match parse(["agentctl", "--state-dir=/var/lib/agent", "status"]).expect("parses") {
                Parsed::Invocation(invocation) => invocation,
                Parsed::Text(text) => panic!("expected a command, got {text}"),
            };
        assert_eq!(invocation.state_dir, Some(PathBuf::from("/var/lib/agent")));
        assert_eq!(
            intent(&["fork", "--branch", B, "--at", E, "--label=experiment"]),
            Intent::Fork {
                parent: branch(B),
                point: ForkPoint::At(event(E)),
                label: Some("experiment".to_string()),
                boundary: false,
            }
        );
    }

    #[test]
    fn a_double_dash_ends_the_flags_so_a_message_may_start_with_one() {
        assert_eq!(
            intent(&["send", "--branch", B, "--", "--wait"]),
            Intent::Send {
                branch: branch(B),
                message: "--wait".to_string(),
                wait: false,
            }
        );
    }

    #[test]
    fn a_fork_needs_exactly_one_selector() {
        let missing = failure(&["fork", "--branch", B]);
        assert_eq!(missing.exit.code(), 2);
        assert!(missing.message.contains("--rewind") || missing.message.contains("required"));

        let both = failure(&["fork", "--branch", B, "--at", E, "--rewind", "2"]);
        assert_eq!(both.exit.code(), 2);
    }

    #[test]
    fn an_install_needs_exactly_one_supervisor() {
        assert_eq!(failure(&["install"]).exit.code(), 2);
        assert_eq!(
            failure(&["install", "--systemd", "--launchd"]).exit.code(),
            2
        );
    }

    #[test]
    fn a_flag_given_twice_is_refused_rather_than_silently_taken() {
        assert_eq!(
            failure(&["status", "--state-dir", "/a", "--state-dir", "/b"])
                .exit
                .code(),
            2
        );
    }

    #[test]
    fn another_flag_after_the_command_is_refused() {
        assert_eq!(failure(&["status", "--verbose"]).exit.code(), 2);
    }

    #[test]
    fn an_identifier_that_does_not_parse_is_a_usage_error_naming_the_field() {
        let refused = failure(&["branch", "show", "not-an-id"]);
        assert_eq!(refused.exit.code(), 2);
        assert_eq!(refused.class.name(), "usage");
        assert!(refused.message.contains("id takes a branch id"));

        assert!(
            failure(&["replay", "--branch", B, "--from", "zz"])
                .message
                .contains("--from")
        );
        assert!(
            failure(&["fork", "--branch", "b-zz", "--at", E])
                .message
                .contains("--branch")
        );
        assert!(
            failure(&["fork", "--branch", B, "--turn", "nope"])
                .message
                .contains("--turn")
        );
        assert!(
            failure(&["audit", "--branch", B, "--since", "yesterday"])
                .message
                .contains("--since")
        );
    }

    #[test]
    fn help_and_version_are_text_not_commands() {
        for args in [
            vec!["agentctl", "--help"],
            vec!["agentctl", "help"],
            vec!["agentctl", "--version"],
        ] {
            match parse(args.clone()).expect("reads") {
                Parsed::Text(text) => assert!(!text.is_empty(), "{args:?}"),
                Parsed::Invocation(_) => panic!("{args:?} should be text"),
            }
        }
    }

    #[test]
    fn every_command_names_itself_and_what_it_needs() {
        let table = [
            (Intent::Status, "status", true),
            (Intent::BranchList, "branch list", true),
            (
                Intent::BranchShow { branch: branch(B) },
                "branch show",
                true,
            ),
            (
                Intent::Fork {
                    parent: branch(B),
                    point: ForkPoint::Rewind(1),
                    label: None,
                    boundary: false,
                },
                "fork",
                true,
            ),
            (
                Intent::Send {
                    branch: branch(B),
                    message: String::new(),
                    wait: false,
                },
                "send",
                true,
            ),
            (Intent::Approve { id: String::new() }, "approve", true),
            (Intent::Deny { id: String::new() }, "deny", true),
            (Intent::Archive { branch: branch(B) }, "archive", true),
            (
                Intent::Warm {
                    branch: branch(B),
                    wait: false,
                },
                "warm",
                true,
            ),
            (
                Intent::Replay {
                    branch: branch(B),
                    from: None,
                    to: None,
                },
                "replay",
                true,
            ),
            (
                Intent::Audit {
                    branch: branch(B),
                    since: Timestamp::parse("2026-10-26T00:00:00Z").expect("an instant"),
                },
                "audit",
                true,
            ),
            (
                Intent::Export {
                    branch: branch(B),
                    out: PathBuf::new(),
                },
                "export",
                true,
            ),
            (Intent::Verify { deep: false }, "verify", false),
            (Intent::Doctor { deadman: false }, "doctor", true),
            (
                Intent::Backup {
                    out: PathBuf::new(),
                },
                "backup",
                true,
            ),
            (Intent::Gc { dry_run: false }, "gc", true),
            (
                Intent::Install {
                    supervisor: Supervisor::Systemd,
                },
                "install",
                true,
            ),
            (Intent::SpecCheck, "spec check", true),
            (Intent::Version, "version", false),
        ];
        assert_eq!(table.len(), 19);
        for (command, name, is_waiting) in table {
            assert_eq!(command.name(), name);
            let waiting = !command.needs().contains("this command is implemented");
            assert_eq!(waiting, is_waiting, "{name}");
            assert!(!command.needs().is_empty(), "{name}");
        }
    }
}
