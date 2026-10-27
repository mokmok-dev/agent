//! The socket, exercised through a real one.

#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agent_core::fold::FoldError;
use agent_core::fork::{ForkError, Selector};
use agent_core::ids::{BranchId, CallId, EventId, TurnId};
use agent_core::time::TimeError;
use agent_store::StoreError;

use super::{
    Bus, Command, ForkRequest, Pending, Refusal, Reply, read_request, refusal, reply_line,
};
use crate::{ROOT_BRANCH, ReactorError};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agent-bus-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creates the directory");
    dir.join("bus.sock")
}

fn branch(byte: u8) -> BranchId {
    BranchId::from_bytes([byte; 8])
}

fn call() -> CallId {
    CallId {
        turn: TurnId::from_bytes([4; 8]),
        step: 0,
    }
}

/// Polls until one client has connected, and returns it without answering.
fn poll_one(
    bus: &Bus,
    path: &std::path::Path,
) -> Pending {
    let started = Instant::now();
    loop {
        let mut pending = bus.poll();
        if pending.len() == 1 {
            return pending.pop().expect("one pending client");
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the daemon never polled {path:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_send_carries_the_branch_it_names() {
    assert_eq!(
        read_request(r#"{"command":"send","branch":"b-0101010101010101","text":"   hello   "}"#),
        Ok(Command::Send {
            branch: branch(1),
            text: "   hello   ".to_string()
        })
    );
}

#[test]
fn a_command_without_a_branch_means_the_root() {
    assert_eq!(
        read_request(r#"{"command":"send","text":"hello"}"#),
        Ok(Command::Send {
            branch: ROOT_BRANCH,
            text: "hello".to_string()
        })
    );
}

#[test]
fn a_branch_that_is_not_an_id_is_refused_by_name() {
    assert_eq!(
        read_request(r#"{"command":"send","branch":"main","text":"hello"}"#),
        Err(Refusal::Usage("branch is not a branch id".to_string()))
    );
}

#[test]
fn an_approve_and_a_deny_carry_a_branch_and_a_call() {
    assert_eq!(
        read_request(
            r#"{"command":"approve","branch":"b-0202020202020202","id":"0404040404040404:3"}"#
        ),
        Ok(Command::Approve {
            branch: branch(2),
            id: "0404040404040404:3".to_string()
        })
    );
    assert_eq!(
        read_request(r#"{"command":"deny","id":"0404040404040404:3"}"#),
        Ok(Command::Deny {
            branch: ROOT_BRANCH,
            id: "0404040404040404:3".to_string()
        })
    );
}

#[test]
fn a_command_that_needs_the_text_it_lacks_is_empty() {
    assert_eq!(
        read_request(r#"{"command":"send","text":"   "}"#),
        Err(Refusal::Empty("send".to_string()))
    );
    assert_eq!(
        read_request(r#"{"command":"send"}"#),
        Err(Refusal::Empty("send".to_string()))
    );
    assert_eq!(
        read_request(r#"{"command":"approve"}"#),
        Err(Refusal::Empty("approve".to_string()))
    );
    assert_eq!(
        read_request(r#"{"command":"deny","id":" "}"#),
        Err(Refusal::Empty("deny".to_string()))
    );
}

#[test]
fn a_line_with_nothing_in_it_is_empty() {
    assert_eq!(read_request(""), Err(Refusal::Empty(String::new())));
    assert_eq!(read_request("   "), Err(Refusal::Empty(String::new())));
}

#[test]
fn a_line_that_is_not_json_is_malformed() {
    assert_eq!(read_request("hello"), Err(Refusal::Malformed));
    assert_eq!(
        read_request(r#"{"command":"send","branch":3}"#),
        Err(Refusal::Malformed)
    );
}

#[test]
fn a_command_the_socket_does_not_take_is_unknown() {
    assert_eq!(
        read_request(r#"{"command":"dance","text":"x"}"#),
        Err(Refusal::Unknown("dance".to_string()))
    );
}

#[test]
fn a_fork_without_a_selector_starts_at_the_tip() {
    assert_eq!(
        read_request(r#"{"command":"fork","parent":"b-0202020202020202"}"#),
        Ok(Command::Fork(ForkRequest {
            parent: branch(2),
            selector: Selector::Tip,
            label: None,
            boundary: false,
            force: false,
        }))
    );
}

#[test]
fn a_fork_carries_every_selector_and_flag() {
    let at = EventId::from_bytes([3; 32]);
    let line = format!(
        r#"{{"command":"fork","parent":"b-0202020202020202","at":"{at}","label":"experiment","boundary":true,"force":true}}"#
    );
    assert_eq!(
        read_request(&line),
        Ok(Command::Fork(ForkRequest {
            parent: branch(2),
            selector: Selector::At(at),
            label: Some("experiment".to_string()),
            boundary: true,
            force: true,
        }))
    );
    assert_eq!(
        read_request(r#"{"command":"fork","parent":"b-0202020202020202","rewind":2}"#),
        Ok(Command::Fork(ForkRequest {
            parent: branch(2),
            selector: Selector::Rewind(2),
            label: None,
            boundary: false,
            force: false,
        }))
    );
    let turn = TurnId::from_bytes([4; 8]);
    let line = format!(r#"{{"command":"fork","parent":"b-0202020202020202","turn":"{turn}"}}"#);
    assert_eq!(
        read_request(&line),
        Ok(Command::Fork(ForkRequest {
            parent: branch(2),
            selector: Selector::Turn(turn),
            label: None,
            boundary: false,
            force: false,
        }))
    );
}

#[test]
fn a_fork_with_two_selectors_is_refused() {
    assert_eq!(
        read_request(
            r#"{"command":"fork","parent":"b-0202020202020202","rewind":1,"turn":"0404040404040404"}"#
        ),
        Err(Refusal::Usage(
            "fork takes one of --at, --rewind, or --turn".to_string()
        ))
    );
}

#[test]
fn a_fork_that_names_an_unreadable_field_says_which_one() {
    assert_eq!(
        read_request(r#"{"command":"fork","parent":"b-0202020202020202","at":"nope"}"#),
        Err(Refusal::Usage("at is not an event id".to_string()))
    );
    assert_eq!(
        read_request(r#"{"command":"fork","parent":"b-0202020202020202","turn":"nope"}"#),
        Err(Refusal::Usage("turn is not a turn id".to_string()))
    );
    assert_eq!(
        read_request(r#"{"command":"fork","parent":"main"}"#),
        Err(Refusal::Usage("parent is not a branch id".to_string()))
    );
    assert_eq!(
        read_request(r#"{"command":"fork"}"#),
        Err(Refusal::Usage("fork needs a parent branch".to_string()))
    );
}

#[test]
fn a_taken_reply_is_the_bare_acknowledgement() {
    assert_eq!(reply_line(&Ok(Reply::Taken)), r#"{"ok":true}"#);
}

#[test]
fn a_fork_reply_carries_the_new_branch_and_its_depth() {
    assert_eq!(
        reply_line(&Ok(Reply::Forked {
            branch: branch(1),
            depth: 3
        })),
        r#"{"ok":true,"branch":"b-0101010101010101","depth":3}"#
    );
}

#[test]
fn every_refusal_has_its_own_class_name() {
    let cases = [
        (Refusal::Malformed, "malformed"),
        (Refusal::Unknown("x".to_string()), "unknown"),
        (Refusal::Empty("x".to_string()), "empty"),
        (Refusal::Usage("x".to_string()), "usage"),
        (Refusal::Policy("x".to_string()), "policy"),
        (Refusal::Storage("x".to_string()), "storage"),
        (Refusal::Corrupt("x".to_string()), "corrupt"),
        (Refusal::Failed("x".to_string()), "failed"),
    ];
    for (refusal, class) in cases {
        assert_eq!(refusal.class(), class);
    }
}

#[test]
fn every_refusal_has_the_message_the_client_prints() {
    assert_eq!(
        Refusal::Malformed.message(),
        "the request is not the JSON this socket reads"
    );
    assert_eq!(
        Refusal::Unknown("dance".to_string()).message(),
        "dance is not a command the socket accepts"
    );
    assert_eq!(
        Refusal::Empty("send".to_string()).message(),
        "send needs text"
    );
    assert_eq!(Refusal::Empty(String::new()).message(), "the line is empty");
    for refusal in [
        Refusal::Usage("u".to_string()),
        Refusal::Policy("p".to_string()),
        Refusal::Storage("s".to_string()),
        Refusal::Corrupt("c".to_string()),
        Refusal::Failed("f".to_string()),
    ] {
        let expected = match &refusal {
            Refusal::Usage(text)
            | Refusal::Policy(text)
            | Refusal::Storage(text)
            | Refusal::Corrupt(text)
            | Refusal::Failed(text) => text.clone(),
            _ => unreachable!(),
        };
        assert_eq!(refusal.message(), expected);
    }
}

#[test]
fn the_reply_json_is_the_documented_shape_for_every_class() {
    let cases = [
        (
            Refusal::Malformed,
            r#"{"ok":false,"class":"malformed","message":"the request is not the JSON this socket reads"}"#,
        ),
        (
            Refusal::Unknown("dance".to_string()),
            r#"{"ok":false,"class":"unknown","message":"dance is not a command the socket accepts"}"#,
        ),
        (
            Refusal::Empty("send".to_string()),
            r#"{"ok":false,"class":"empty","message":"send needs text"}"#,
        ),
        (
            Refusal::Empty(String::new()),
            r#"{"ok":false,"class":"empty","message":"the line is empty"}"#,
        ),
        (
            Refusal::Usage("bad flags".to_string()),
            r#"{"ok":false,"class":"usage","message":"bad flags"}"#,
        ),
        (
            Refusal::Policy("needs force".to_string()),
            r#"{"ok":false,"class":"policy","message":"needs force"}"#,
        ),
        (
            Refusal::Storage("disk full".to_string()),
            r#"{"ok":false,"class":"storage","message":"disk full"}"#,
        ),
        (
            Refusal::Corrupt("bad hash".to_string()),
            r#"{"ok":false,"class":"corrupt","message":"bad hash"}"#,
        ),
        (
            Refusal::Failed("boom".to_string()),
            r#"{"ok":false,"class":"failed","message":"boom"}"#,
        ),
    ];
    for (refusal, line) in cases {
        assert_eq!(reply_line(&Err(refusal)), line);
    }
}

#[test]
fn a_reactor_error_becomes_the_class_the_client_reads() {
    let cases = [
        (ReactorError::NoSuchBranch(branch(1)), "usage"),
        (ReactorError::Fork(ForkError::BeforeGenesis), "usage"),
        (
            ReactorError::ForkStranded {
                call: call(),
                tool: "shell".to_string(),
            },
            "policy",
        ),
        (
            ReactorError::Store(StoreError::SecondGenesis(branch(1))),
            "corrupt",
        ),
        (
            ReactorError::Store(StoreError::BranchExists(branch(1))),
            "failed",
        ),
        (
            ReactorError::Store(StoreError::Query(rusqlite::Error::InvalidQuery)),
            "storage",
        ),
        (
            ReactorError::Fold(FoldError::UnknownBranch(branch(1))),
            "corrupt",
        ),
        (ReactorError::NoModel, "failed"),
        (ReactorError::Time(TimeError::BadShape), "failed"),
        (ReactorError::NoSuchCall(call()), "failed"),
    ];
    for (error, class) in cases {
        let refusal = refusal(&error);
        assert_eq!(refusal.class(), class, "{error}");
        assert_eq!(refusal.message(), error.to_string());
    }
}

#[test]
fn the_socket_carries_a_command_to_the_polling_side() {
    let path = scratch("carries");
    let bus = Bus::bind(&path).expect("binds");
    assert_eq!(bus.path(), path.as_path());

    let target = path.clone();
    let client = std::thread::spawn(move || {
        ask(
            &target,
            r#"{"command":"send","branch":"b-0202020202020202","text":"hello"}"#,
        )
    });
    let pending = poll_one(&bus, &path);
    assert_eq!(
        pending.command(),
        Ok(&Command::Send {
            branch: branch(2),
            text: "hello".to_string()
        })
    );
    pending.answer(&Ok(Reply::Taken));
    assert_eq!(
        client.join().expect("the client finished"),
        r#"{"ok":true}"#
    );
}

#[test]
fn a_client_waits_until_the_daemon_has_acted() {
    let path = scratch("pending");
    let bus = Bus::bind(&path).expect("binds");

    let target = path.clone();
    let (observed, saw_timeout) = mpsc::channel();
    let client = std::thread::spawn(move || {
        let mut stream = UnixStream::connect(&target).expect("connects");
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .expect("sets the bound");
        stream
            .write_all(br#"{"command":"send","text":"hello"}"#)
            .expect("writes");
        stream.write_all(b"\n").expect("writes");
        stream.flush().expect("flushes");
        let mut reader = BufReader::new(stream);
        let mut early = String::new();
        let timed_out = matches!(
            reader.read_line(&mut early),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
        );
        observed.send(timed_out).expect("reports the first read");
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("sets the bound");
        let mut answer = String::new();
        reader.read_line(&mut answer).expect("the daemon answered");
        answer.trim_end().to_string()
    });

    let pending = poll_one(&bus, &path);
    assert!(
        saw_timeout
            .recv_timeout(Duration::from_secs(5))
            .expect("the client read"),
        "the client was answered before the daemon acted"
    );
    pending.answer(&Ok(Reply::Taken));
    assert_eq!(
        client.join().expect("the client finished"),
        r#"{"ok":true}"#
    );
}

#[test]
fn a_bad_line_is_answered_and_reported() {
    let path = scratch("bad");
    let bus = Bus::bind(&path).expect("binds");

    let target = path.clone();
    let client = std::thread::spawn(move || ask(&target, "not json"));
    let pending = poll_one(&bus, &path);
    assert_eq!(pending.command(), Err(&Refusal::Malformed));
    pending.answer(&Err(Refusal::Malformed));
    let answer = client.join().expect("the client finished");
    assert_eq!(
        answer,
        r#"{"ok":false,"class":"malformed","message":"the request is not the JSON this socket reads"}"#
    );
}

#[test]
fn a_client_that_leaves_without_a_line_is_malformed() {
    let path = scratch("silent");
    let bus = Bus::bind(&path).expect("binds");

    let target = path.clone();
    let client = std::thread::spawn(move || {
        let stream = UnixStream::connect(&target).expect("connects");
        drop(stream);
    });
    let pending = poll_one(&bus, &path);
    assert_eq!(pending.command(), Err(&Refusal::Malformed));
    pending.answer(&Err(Refusal::Malformed));
    client.join().expect("the client finished");
}

#[test]
fn a_second_daemon_takes_the_socket_of_a_dead_one() {
    let path = scratch("restart");
    {
        let _bus = Bus::bind(&path).expect("binds");
    }
    // The drop removed the file; a daemon that died without dropping leaves one
    // behind, which bind replaces. Either way the second bind succeeds.
    let bus = Bus::bind(&path).expect("binds again");
    assert_eq!(bus.poll().len(), 0);
}

#[test]
fn the_socket_file_goes_away_with_the_daemon() {
    let path = scratch("drop");
    {
        let _bus = Bus::bind(&path).expect("binds");
        assert!(path.exists());
    }
    assert!(!path.exists(), "a dead daemon leaves no address behind");
}

/// Sends one line and reads the answer.
fn ask(
    path: &std::path::Path,
    line: &str,
) -> String {
    let mut stream = UnixStream::connect(path).expect("connects");
    // A daemon that never answers must fail the test rather than hang it, or the
    // mutation that stops it polling would be reported as a timeout instead of a
    // kill.
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("sets the bound");
    stream.write_all(line.as_bytes()).expect("writes");
    stream.write_all(b"\n").expect("writes");
    stream.flush().expect("flushes");
    let mut reader = BufReader::new(stream);
    let mut answer = String::new();
    reader.read_line(&mut answer).expect("the daemon answered");
    answer.trim_end().to_string()
}
