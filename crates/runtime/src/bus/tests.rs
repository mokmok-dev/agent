//! The socket, exercised through a real one.

#![allow(
    clippy::expect_used,
    reason = "a test reports a broken assumption by panicking"
)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use super::{Bus, Command, Refusal, read_request, reply_line, request_line};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agent-bus-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creates the directory");
    dir.join("bus.sock")
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
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("sets the bound");
    stream.write_all(line.as_bytes()).expect("writes");
    stream.write_all(b"\n").expect("writes");
    stream.flush().expect("flushes");
    let mut reader = BufReader::new(stream);
    let mut answer = String::new();
    reader.read_line(&mut answer).expect("the daemon answered");
    answer.trim_end().to_string()
}

#[test]
fn a_command_arrives_whole() {
    assert_eq!(
        read_request(r#"{"command":"send","text":"hello"}"#),
        Ok(Command {
            command: "send".to_string(),
            text: "hello".to_string()
        })
    );
}

#[test]
fn a_command_the_socket_does_not_take_is_refused() {
    assert_eq!(
        read_request(r#"{"command":"fork","text":"x"}"#),
        Err(Refusal::Unknown("fork".to_string()))
    );
}

#[test]
fn a_command_with_nothing_in_it_is_refused() {
    assert_eq!(
        read_request(r#"{"command":"send","text":"   "}"#),
        Err(Refusal::Empty("send".to_string()))
    );
    assert_eq!(
        read_request(r#"{"command":"send"}"#),
        Err(Refusal::Empty("send".to_string()))
    );
}

#[test]
fn a_line_that_is_not_json_is_refused() {
    assert_eq!(read_request("hello"), Err(Refusal::Malformed));
}

#[test]
fn a_reply_says_whether_the_line_was_taken() {
    assert_eq!(reply_line(&Ok(())), r#"{"ok":true}"#);
    assert_eq!(
        reply_line(&Err(Refusal::Unknown("fork".to_string()))),
        r#"{"message":"fork is not a command the socket accepts","ok":false}"#
    );
}

#[test]
fn the_client_line_is_the_one_the_daemon_reads() {
    let line = request_line("approve", "0102:0");
    assert_eq!(
        read_request(&line),
        Ok(Command {
            command: "approve".to_string(),
            text: "0102:0".to_string()
        })
    );
}

/// Answers the client while the test polls, which is the real arrangement: the
/// client waits for its reply and the daemon is the one that polls.
fn ask_while_polling(
    bus: &Bus,
    path: &std::path::Path,
    line: &str,
) -> (String, Vec<Result<Command, Refusal>>) {
    let target = path.to_path_buf();
    let line = line.to_string();
    let client = std::thread::spawn(move || ask(&target, &line));
    let started = std::time::Instant::now();
    loop {
        let answers = bus.poll();
        if !answers.is_empty() {
            return (client.join().expect("the client finished"), answers);
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the daemon never polled"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn the_socket_carries_a_command_to_the_polling_side() {
    let path = scratch("carries");
    let bus = Bus::bind(&path).expect("binds");
    assert_eq!(bus.path(), path.as_path());

    let (answer, answers) = ask_while_polling(&bus, &path, &request_line("send", "hello"));
    assert_eq!(answer, r#"{"ok":true}"#);
    assert_eq!(
        answers,
        vec![Ok(Command {
            command: "send".to_string(),
            text: "hello".to_string()
        })]
    );
}

#[test]
fn a_bad_line_is_answered_and_reported() {
    let path = scratch("bad");
    let bus = Bus::bind(&path).expect("binds");

    let (answer, answers) = ask_while_polling(&bus, &path, "not json");
    assert!(answer.contains(r#""ok":false"#), "{answer}");
    assert_eq!(answers, vec![Err(Refusal::Malformed)]);
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
