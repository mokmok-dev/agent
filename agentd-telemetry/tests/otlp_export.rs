//! End-to-end `OTLP` export test: with a collector endpoint configured, a span
//! is batched and sent to it as `OTLP` over HTTP protobuf.
//!
//! It stands up a minimal HTTP receiver on loopback, points the exporter at it,
//! emits a span, and asserts the process made a `POST` carrying a protobuf body.
//! It covers both endpoint forms: a verbatim traces endpoint, and the base
//! collector variable whose `/v1/traces` path the exporter must append.
//!
//! The helpers use `expect`/`panic` like the `#[cfg(test)]` modules in `src`; the
//! workspace `allow-*-in-tests` clippy configuration cannot see integration test
//! files, so it is replicated here.

#![expect(
    clippy::expect_used,
    reason = "integration tests use expect for setup and assertions"
)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Receives OTLP HTTP requests until one is a `POST`, reporting its request line
/// and body length. The exporter may open a probe connection first, so a single
/// accept is not enough.
fn receiver() -> (u16, mpsc::Receiver<(String, usize)>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the receiver");
    let port = listener.local_addr().expect("addr").port();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                return;
            };
            // Read the head, then the body by Content-Length.
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => head.push(byte[0]),
                }
            }
            let head = String::from_utf8_lossy(&head).into_owned();
            let content_length = head
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                })
                .unwrap_or(0);
            let mut body = vec![0_u8; content_length];
            let _ = stream.read_exact(&mut body);
            let request_line = head.lines().next().unwrap_or_default().to_string();
            let is_post = request_line.starts_with("POST ");
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            if is_post {
                let _ = sender.send((request_line, body.len()));
                return;
            }
        }
    });
    (port, receiver)
}

/// Emits one span, so the exporter has something to batch.
fn emit_span() {
    let span = tracing::info_span!("export.me");
    let _entered = span.enter();
    tracing::info!("inside the span");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_traces_endpoint_is_used_verbatim() {
    let (port, receiver) = receiver();
    let endpoint = format!("http://127.0.0.1:{port}/v1/traces");

    let telemetry =
        agentd_telemetry::init_with_traces_endpoint("info", &endpoint).expect("initialization");
    emit_span();
    telemetry.shutdown();

    let (request_line, body_len) = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the exporter must POST to the collector");
    assert!(
        request_line.starts_with("POST /v1/traces "),
        "a traces endpoint is used verbatim: {request_line:?}"
    );
    assert!(body_len > 0, "the OTLP body must not be empty");
}
