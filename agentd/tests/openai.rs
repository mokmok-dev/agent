//! End-to-end tests for the OpenAI-compatible client.
//!
//! Every socket here is real and every handshake is real: a throwaway `CONNECT`
//! proxy that enforces the sandbox's `Bearer` form, and a throwaway HTTPS upstream
//! whose certificate `rcgen` mints inside the test. Nothing needs a network, and
//! the TLS path — the only path a session has — is the one that runs.
#![expect(
    clippy::expect_used,
    reason = "integration test code may panic when a fixture fails"
)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use agentd_agent::model::Message;
use agentd_agent::openai::{Client, Config, Error, Proxy};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{Value, json};

/// The token the fixtures' proxy expects and the client sends.
const PROXY_TOKEN: &str = "p4ss";

/// The model id every test asks for.
const MODEL: &str = "grok-4.7";

/// The key the fixtures' client sends.
const API_KEY: &str = "provider-key";

/// What a fixture writes back when a connection arrives.
enum Answer {
    /// Write these bytes and close.
    Reply(Vec<u8>),
    /// Write nothing and hold the connection open.
    Stall(Duration),
}

/// A throwaway HTTPS upstream.
struct Upstream {
    /// The certificate's host name, as the client must ask for it.
    host: String,
    /// The port it listens on.
    port: u16,
    /// Its self-signed certificate, for a client to trust.
    certificate: CertificateDer<'static>,
    /// What each connection asked for, head and body together.
    requests: Arc<Mutex<Vec<String>>>,
}

impl Upstream {
    /// Serve one `Answer` per connection, with a certificate for `host`.
    fn start(
        host: &str,
        answers: Vec<Answer>,
    ) -> Self {
        let certified =
            rcgen::generate_simple_self_signed(vec![host.to_owned()]).expect("mints a certificate");
        let certificate = certified.cert.der().clone();
        let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(
            certified.signing_key.serialize_der(),
        ));
        let config = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![certificate.clone()], key)
                .expect("builds a server config"),
        );

        let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
        let port = listener.local_addr().expect("has an address").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&requests);
        thread::spawn(move || {
            for answer in answers {
                let Ok((tcp, _)) = listener.accept() else {
                    return;
                };
                let connection = rustls::ServerConnection::new(Arc::clone(&config))
                    .expect("starts a connection");
                let mut stream = rustls::StreamOwned::new(connection, tcp);
                if let Ok(request) = read_request(&mut stream) {
                    sink.lock().expect("unpoisoned").push(request);
                }
                match answer {
                    Answer::Reply(bytes) => {
                        let _ = stream.write_all(&bytes);
                        // A server that means to end the body does it with a close
                        // notification, so a client reading to the end of the
                        // connection sees an end rather than a truncation.
                        stream.conn.send_close_notify();
                        let _ = stream.flush();
                    },
                    Answer::Stall(hold) => thread::sleep(hold),
                }
            }
        });
        Self {
            host: host.to_owned(),
            port,
            certificate,
            requests,
        }
    }

    /// The base URL a client reaches this upstream at.
    fn base_url(&self) -> String {
        format!("https://{}:{}/v1", self.host, self.port)
    }

    /// `host:port`, which is what a `CONNECT` names and a `Host` header holds.
    fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The trust store that accepts this upstream's certificate.
    fn roots(&self) -> RootCertStore {
        let mut roots = RootCertStore::empty();
        roots
            .add(self.certificate.clone())
            .expect("adds the certificate");
        roots
    }

    /// The request the upstream read, parsed.
    fn request(&self) -> Parsed {
        let request = self
            .requests
            .lock()
            .expect("unpoisoned")
            .first()
            .cloned()
            .expect("the endpoint was asked once");
        Parsed::of(&request)
    }
}

/// A throwaway `CONNECT` proxy that requires the sandbox's `Bearer` form.
struct ProxyServer {
    /// The port it listens on.
    port: u16,
    /// The head of every `CONNECT` it was asked for.
    heads: Arc<Mutex<Vec<String>>>,
}

impl ProxyServer {
    /// Serve one tunnel, requiring `token`.
    fn start(token: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
        let port = listener.local_addr().expect("has an address").port();
        let heads = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heads);
        let expected = format!("Bearer {token}");
        thread::spawn(move || {
            let Ok((mut client, _)) = listener.accept() else {
                return;
            };
            let Ok(head) = read_head(&mut client) else {
                return;
            };
            sink.lock().expect("unpoisoned").push(head.clone());
            let Some(authority) = connect_authority(&head) else {
                let _ = client.write_all(&plain(400, "Bad Request", "no authority"));
                return;
            };
            if !head
                .lines()
                .any(|line| line.trim() == format!("Proxy-Authorization: {expected}"))
            {
                // What the sandbox's proxy does with a client that cannot
                // authenticate: it answers before it says anything about
                // destinations.
                let _ = client.write_all(&plain(407, "Proxy Authentication Required", "no bearer"));
                return;
            }
            let Ok(mut upstream) = TcpStream::connect(&authority) else {
                let _ = client.write_all(&plain(502, "Bad Gateway", "no upstream"));
                return;
            };
            let _ = client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
            let _ = client.flush();

            // Copy both ways for the life of the tunnel.
            let mut other = upstream.try_clone().expect("clones the upstream");
            let mut first = client.try_clone().expect("clones the client");
            let relay = thread::spawn(move || {
                let _ = std::io::copy(&mut first, &mut other);
            });
            let _ = std::io::copy(&mut upstream, &mut client);
            let _ = relay.join();
        });
        Self { port, heads }
    }

    /// The URL the sandbox would inject for this proxy.
    fn url(&self) -> String {
        format!("http://agent:{PROXY_TOKEN}@127.0.0.1:{}", self.port)
    }

    /// The `CONNECT` head this proxy was asked for.
    fn head(&self) -> String {
        self.heads
            .lock()
            .expect("unpoisoned")
            .first()
            .expect("the proxy was asked once")
            .clone()
    }
}

/// A request a fixture read, split into its head and its JSON body.
struct Parsed {
    /// The head, with its lines.
    head: String,
    /// The body, parsed.
    body: Value,
}

impl Parsed {
    /// Parse `request`, which is a head and a body together.
    fn of(request: &str) -> Self {
        let (head, body) = request
            .split_once("\r\n\r\n")
            .expect("the request has a blank line");
        Self {
            head: head.to_owned(),
            body: serde_json::from_str(body).expect("the body is JSON"),
        }
    }

    /// The value of the header `name`, if the head carries one.
    fn header(
        &self,
        name: &str,
    ) -> Option<String> {
        self.head.lines().find_map(|line| {
            let (found, value) = line.split_once(':')?;
            found
                .trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
}

/// Read a head, up to and including its blank line.
fn read_head(stream: &mut impl Read) -> std::io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if stream.read(&mut byte)? == 0 {
            return Err(std::io::Error::other("the connection ended early"));
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            return Ok(String::from_utf8_lossy(&head).into_owned());
        }
        if head.len() > 16 * 1024 {
            return Err(std::io::Error::other("the head is too large"));
        }
    }
}

/// Read a request: its head, then its body by `Content-Length`.
fn read_request(stream: &mut impl Read) -> std::io::Result<String> {
    let head = read_head(stream)?;
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body)?;
    Ok(format!("{head}{}", String::from_utf8_lossy(&body)))
}

/// The authority of a `CONNECT` head.
fn connect_authority(head: &str) -> Option<String> {
    head.lines()
        .next()?
        .strip_prefix("CONNECT ")?
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

/// A plain-text HTTP reply.
fn plain(
    status: u16,
    reason: &str,
    body: &str,
) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A JSON HTTP reply.
fn json_reply(
    status: u16,
    reason: &str,
    body: &str,
) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A reply that answers with prose.
fn answer(content: &str) -> Vec<u8> {
    json_reply(
        200,
        "OK",
        &json!({"choices": [{"message": {"content": content}}]}).to_string(),
    )
}

/// A reply that asks for a tool call.
fn tool_call(
    id: &str,
    arguments: &str,
) -> Vec<u8> {
    json_reply(
        200,
        "OK",
        &json!({"choices": [{"message": {"content": null, "tool_calls": [
            {"id": id, "type": "function",
             "function": {"name": "shell", "arguments": arguments}}
        ]}}]})
        .to_string(),
    )
}

/// A client for `upstream`, through `proxy` when one is given.
fn client(
    upstream: &Upstream,
    proxy: Option<&ProxyServer>,
) -> Client {
    let mut config = Config::new(upstream.base_url(), MODEL, API_KEY).with_roots(upstream.roots());
    if let Some(proxy) = proxy {
        config = config.with_proxy(Proxy::parse(&proxy.url()).expect("parses the proxy URL"));
    }
    Client::new(config).expect("builds the client")
}

/// One user message.
fn ask() -> Vec<Message> {
    vec![Message::User {
        content: "say hello".to_owned(),
    }]
}

#[test]
fn a_client_without_a_proxy_reaches_the_endpoint_directly() {
    let upstream = Upstream::start("127.0.0.1", vec![Answer::Reply(answer("hello"))]);
    let response = client(&upstream, None).call(&ask(), &[]).expect("answers");
    assert_eq!(response.content, "hello");
    assert!(response.calls.is_empty());
}

#[test]
fn the_proxy_is_asked_for_the_endpoint_with_the_bearer_header() {
    // This is the form the sandbox's proxy authenticates: `CONNECT host:port`
    // with `Proxy-Authorization: Bearer <token>`, and nothing else.
    let upstream = Upstream::start("127.0.0.1", vec![Answer::Reply(answer("hello"))]);
    let proxy = ProxyServer::start(PROXY_TOKEN);
    client(&upstream, Some(&proxy))
        .call(&ask(), &[])
        .expect("answers");

    let head = proxy.head();
    assert_eq!(
        connect_authority(&head).as_deref(),
        Some(upstream.authority().as_str()),
        "the proxy is asked for the endpoint, not resolved here: {head}"
    );
    assert!(
        head.lines()
            .any(|line| line.trim() == format!("Proxy-Authorization: Bearer {PROXY_TOKEN}")),
        "the tunnel is authenticated as the sandbox expects: {head}"
    );
}

#[test]
fn the_request_carries_the_method_path_authorization_and_body() {
    let upstream = Upstream::start("127.0.0.1", vec![Answer::Reply(answer("hello"))]);
    let proxy = ProxyServer::start(PROXY_TOKEN);
    client(&upstream, Some(&proxy))
        .call(&ask(), &[])
        .expect("answers");

    let request = upstream.request();
    assert_eq!(
        request.head.lines().next(),
        Some("POST /v1/chat/completions HTTP/1.1"),
        "the completions path follows the base URL's"
    );
    assert_eq!(
        request.header("host").as_deref(),
        Some(upstream.authority().as_str()),
        "the host is the authority the request is for"
    );
    assert_eq!(
        request.header("authorization").as_deref(),
        Some("Bearer provider-key")
    );
    assert_eq!(
        request.header("content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(request.body["model"], MODEL);
    assert_eq!(request.body["stream"], json!(false));
    assert_eq!(request.body["messages"][0]["role"], "user");
    assert_eq!(request.body["messages"][0]["content"], "say hello");
}

#[test]
fn a_reply_maps_to_the_agents_response() {
    let upstream = Upstream::start(
        "127.0.0.1",
        vec![Answer::Reply(tool_call("call-1", "{\"argv\":[\"ls\"]}"))],
    );
    let response = client(&upstream, None).call(&ask(), &[]).expect("answers");
    assert_eq!(response.content, "");
    assert_eq!(response.calls.len(), 1);
    assert_eq!(response.calls[0].id, "call-1");
    assert_eq!(response.calls[0].name, "shell");
    assert_eq!(response.calls[0].arguments, json!({"argv": ["ls"]}));
}

#[test]
fn the_tools_the_agent_offers_reach_the_endpoint() {
    let upstream = Upstream::start("127.0.0.1", vec![Answer::Reply(answer("hello"))]);
    let tools = vec![agentd_agent::model::Tool {
        name: "shell".to_owned(),
        description: "run a program".to_owned(),
        parameters: json!({"type": "object", "required": ["argv"]}),
    }];
    client(&upstream, None)
        .call(&ask(), &tools)
        .expect("answers");
    let request = upstream.request();
    assert_eq!(request.body["tools"][0]["type"], "function");
    assert_eq!(request.body["tools"][0]["function"]["name"], "shell");
}

#[test]
fn a_chunked_reply_is_read() {
    let body = json!({"choices": [{"message": {"content": "chunked"}}]}).to_string();
    let mut reply = Vec::new();
    reply.extend_from_slice(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    for chunk in body.as_bytes().chunks(7) {
        reply.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        reply.extend_from_slice(chunk);
        reply.extend_from_slice(b"\r\n");
    }
    reply.extend_from_slice(b"0\r\n\r\n");

    let upstream = Upstream::start("127.0.0.1", vec![Answer::Reply(reply)]);
    let response = client(&upstream, None).call(&ask(), &[]).expect("answers");
    assert_eq!(response.content, "chunked");
}

#[test]
fn a_reply_that_runs_to_the_end_of_the_connection_is_read() {
    // No framing at all: the body is whatever arrives before the close.
    let body = json!({"choices": [{"message": {"content": "unframed"}}]}).to_string();
    let reply =
        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{body}").into_bytes();
    let upstream = Upstream::start("127.0.0.1", vec![Answer::Reply(reply)]);
    let response = client(&upstream, None).call(&ask(), &[]).expect("answers");
    assert_eq!(response.content, "unframed");
}

#[test]
fn a_provider_error_is_reported_with_its_own_message() {
    let upstream = Upstream::start(
        "127.0.0.1",
        vec![Answer::Reply(json_reply(
            401,
            "Unauthorized",
            &json!({"error": {"message": "invalid api key", "type": "auth"}}).to_string(),
        ))],
    );
    let error = client(&upstream, None)
        .call(&ask(), &[])
        .expect_err("refused");
    assert!(
        matches!(&error, Error::Provider { status: 401, reason } if reason == "invalid api key"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_provider_error_without_a_message_quotes_the_body() {
    let upstream = Upstream::start(
        "127.0.0.1",
        vec![Answer::Reply(plain(502, "Bad Gateway", "upstream gone"))],
    );
    let error = client(&upstream, None)
        .call(&ask(), &[])
        .expect_err("refused");
    assert!(
        matches!(&error, Error::Provider { status: 502, reason } if reason == "upstream gone"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_proxy_that_does_not_accept_the_bearer_header_is_reported() {
    // The client sends the token the sandbox injects; a proxy that wants another
    // one refuses the tunnel before the endpoint is reached at all.
    let upstream = Upstream::start("127.0.0.1", vec![Answer::Reply(answer("hello"))]);
    let proxy = ProxyServer::start("another-token");
    let error = client(&upstream, Some(&proxy))
        .call(&ask(), &[])
        .expect_err("refused");
    assert!(
        matches!(&error, Error::Proxy { status: 407, reason } if reason == "no bearer"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_reply_that_is_not_json_is_refused() {
    let upstream = Upstream::start(
        "127.0.0.1",
        vec![Answer::Reply(plain(200, "OK", "not json"))],
    );
    let error = client(&upstream, None)
        .call(&ask(), &[])
        .expect_err("refused");
    assert!(
        matches!(error, Error::Reply(_)),
        "unexpected error: {error}"
    );
}

#[test]
fn a_reply_with_no_choices_is_refused() {
    let upstream = Upstream::start(
        "127.0.0.1",
        vec![Answer::Reply(json_reply(200, "OK", "{\"choices\": []}"))],
    );
    let error = client(&upstream, None)
        .call(&ask(), &[])
        .expect_err("refused");
    assert!(
        matches!(&error, Error::Reply(reason) if reason.contains("no choices")),
        "unexpected error: {error}"
    );
}

#[test]
fn a_declared_length_over_the_cap_is_refused() {
    let upstream = Upstream::start(
        "127.0.0.1",
        vec![Answer::Reply(json_reply(200, "OK", &"x".repeat(1_100_000)))],
    );
    let error = client(&upstream, None)
        .call(&ask(), &[])
        .expect_err("refused");
    assert!(
        matches!(error, Error::TooLarge { what: "reply", .. }),
        "unexpected error: {error}"
    );
}

#[test]
fn a_body_that_runs_past_the_cap_is_refused() {
    // No framing, so the client reads until the endpoint closes — and stops at the
    // cap rather than after it.
    let oversized = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{}",
        "x".repeat(1_100_000)
    );
    let upstream = Upstream::start("127.0.0.1", vec![Answer::Reply(oversized.into_bytes())]);
    let error = client(&upstream, None)
        .call(&ask(), &[])
        .expect_err("refused");
    assert!(
        matches!(error, Error::TooLarge { what: "reply", .. }),
        "unexpected error: {error}"
    );
}

#[test]
fn a_stalled_endpoint_hits_the_deadline() {
    let upstream = Upstream::start("127.0.0.1", vec![Answer::Stall(Duration::from_secs(30))]);
    let proxy = ProxyServer::start(PROXY_TOKEN);
    let config = Config::new(upstream.base_url(), MODEL, API_KEY)
        .with_roots(upstream.roots())
        .with_proxy(Proxy::parse(&proxy.url()).expect("parses the proxy URL"))
        .with_deadline(Duration::from_millis(300));
    let client = Client::new(config).expect("builds the client");

    let started = Instant::now();
    let error = client.call(&ask(), &[]).expect_err("refused");
    assert!(
        matches!(error, Error::Deadline),
        "unexpected error: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline fired rather than waiting for the endpoint"
    );
}

#[test]
fn an_endpoint_whose_certificate_is_not_trusted_is_refused() {
    // The client verifies: a certificate the client was not told to trust is not
    // silently accepted, in tests or anywhere else.
    let upstream = Upstream::start("127.0.0.1", vec![Answer::Reply(answer("hello"))]);
    let config = Config::new(upstream.base_url(), MODEL, API_KEY);
    let client = Client::new(config).expect("builds the client");
    let error = client.call(&ask(), &[]).expect_err("refused");
    assert!(
        matches!(error, Error::Transport(_)),
        "unexpected error: {error}"
    );
}

#[test]
fn a_hostname_certificate_verifies_against_the_name_the_client_asked_for() {
    // Production asks for a name, not an address, so the DNS-name path is the one
    // that runs here.
    let upstream = Upstream::start("localhost", vec![Answer::Reply(answer("hello"))]);
    let response = client(&upstream, None).call(&ask(), &[]).expect("answers");
    assert_eq!(response.content, "hello");
}
