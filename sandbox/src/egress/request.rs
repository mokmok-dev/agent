//! Parsing the head of an HTTP `CONNECT` request.
//!
//! The proxy reads only the request head — the `CONNECT` line and headers up to
//! the blank line — under one size cap and one deadline. Everything after the
//! head is tunnelled opaquely, so this module is the whole of the untrusted-input
//! surface: it must reject a malformed or oversized head without reading past it
//! and without allocating without bound.

/// The maximum bytes of request head the proxy will read.
///
/// A `CONNECT` head is tiny; 8 KiB is far more than any client sends and bounds
/// what a pre-authentication client can make the proxy buffer.
pub const MAX_HEAD_BYTES: usize = 8 * 1024;

/// A parsed `CONNECT` request head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect {
    /// The requested host, with any bracketed IPv6 form unwrapped.
    pub host: String,
    /// The requested port.
    pub port: u16,
    /// The `Proxy-Authorization` header value, if the client sent one.
    pub authorization: Option<String>,
}

/// Why a request head could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The head exceeded [`MAX_HEAD_BYTES`].
    #[error("the request head is larger than {MAX_HEAD_BYTES} bytes")]
    HeadTooLarge,
    /// The head ended before a blank line, so it was truncated.
    #[error("the request head ended before its blank line")]
    Incomplete,
    /// The request line was not `CONNECT host:port HTTP/1.1`.
    #[error("the request line is not a valid CONNECT")]
    NotConnect,
    /// The `host:port` authority was missing or malformed.
    #[error("the CONNECT authority is malformed")]
    BadAuthority,
    /// The port was not a number in `1..=65535`.
    #[error("the CONNECT port is invalid")]
    BadPort,
    /// A header line was not `Name: value`.
    #[error("the request head has a malformed header line")]
    BadHeader,
}

/// Parse the request head, the bytes up to and including the blank line.
///
/// Returns the parsed request and the byte length of the head, so the caller can
/// tunnel the bytes that follow it. The head is not required to arrive in one
/// read; the caller accumulates until it sees the blank line or exceeds
/// [`MAX_HEAD_BYTES`].
///
/// # Errors
///
/// Returns a [`ParseError`] for an oversized, incomplete, or malformed head.
pub fn parse(head: &[u8]) -> Result<(Connect, usize), ParseError> {
    if head.len() > MAX_HEAD_BYTES {
        return Err(ParseError::HeadTooLarge);
    }
    let text = std::str::from_utf8(head).map_err(|_| ParseError::NotConnect)?;
    // The head ends at the first blank line; a request using bare LF is accepted
    // because a client may not send CRLF on a local socket.
    let Some(head_end) = find_head_end(text) else {
        return Err(ParseError::Incomplete);
    };
    let head_text = &text[..head_end];
    let mut lines = head_text
        .split('\n')
        .map(|line| line.trim_end_matches('\r'));

    let request_line = lines.next().ok_or(ParseError::NotConnect)?;
    let (host, port) = parse_request_line(request_line)?;

    let mut authorization = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':').ok_or(ParseError::BadHeader)?;
        if name.eq_ignore_ascii_case("proxy-authorization") {
            authorization = Some(value.trim().to_owned());
        }
    }

    // `find_head_end` returns the index of the blank line's first byte, so the
    // head includes the blank line and any following byte belongs to the tunnel.
    Ok((
        Connect {
            host,
            port,
            authorization,
        },
        head_end,
    ))
}

/// The index just past the blank line that ends the head.
fn find_head_end(text: &str) -> Option<usize> {
    let lf = text.find("\n\n").map(|at| at + 2);
    let crlf = text.find("\r\n\r\n").map(|at| at + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// Parse `CONNECT host:port HTTP/1.1`, returning the host and port.
fn parse_request_line(line: &str) -> Result<(String, u16), ParseError> {
    let mut parts = line.split(' ');
    let method = parts.next().ok_or(ParseError::NotConnect)?;
    let authority = parts.next().ok_or(ParseError::NotConnect)?;
    let version = parts.next().ok_or(ParseError::NotConnect)?;
    if !method.eq_ignore_ascii_case("CONNECT") || !version.starts_with("HTTP/") {
        return Err(ParseError::NotConnect);
    }
    parse_authority(authority)
}

/// Split a `host:port` authority, unwrapping a bracketed IPv6 host.
fn parse_authority(authority: &str) -> Result<(String, u16), ParseError> {
    let (host, port_text) = if let Some(rest) = authority.strip_prefix('[') {
        // `[::1]:443`: the host is up to the closing bracket.
        let (host, tail) = rest.split_once(']').ok_or(ParseError::BadAuthority)?;
        let port_text = tail.strip_prefix(':').ok_or(ParseError::BadAuthority)?;
        (host.to_owned(), port_text)
    } else {
        let (host, port_text) = authority.rsplit_once(':').ok_or(ParseError::BadAuthority)?;
        (host.to_owned(), port_text)
    };
    if host.is_empty() {
        return Err(ParseError::BadAuthority);
    }
    let port: u16 = port_text.parse().map_err(|_| ParseError::BadPort)?;
    if port == 0 {
        return Err(ParseError::BadPort);
    }
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    // Tests for the request-head parser. This is the untrusted-input surface, so
    // the cases cover the shapes a hostile client could send.

    use super::*;

    #[test]
    fn a_plain_connect_parses() {
        let head = b"CONNECT api.example.com:443 HTTP/1.1\r\n\r\n";
        let (connect, len) = parse(head).expect("parses");
        assert_eq!(connect.host, "api.example.com");
        assert_eq!(connect.port, 443);
        assert_eq!(connect.authorization, None);
        assert_eq!(len, head.len());
    }

    #[test]
    fn the_head_length_excludes_the_tunnelled_bytes() {
        let head = b"CONNECT h:1 HTTP/1.1\r\n\r\nGET / HTTP/1.1\r\n";
        let (_, len) = parse(head).expect("parses");
        assert_eq!(&head[len..], b"GET / HTTP/1.1\r\n");
    }

    #[test]
    fn a_proxy_authorization_header_is_read() {
        let head = b"CONNECT h:443 HTTP/1.1\r\nProxy-Authorization: Basic abc\r\n\r\n";
        let (connect, _) = parse(head).expect("parses");
        assert_eq!(connect.authorization.as_deref(), Some("Basic abc"));
    }

    #[test]
    fn the_header_name_is_case_insensitive() {
        let head = b"CONNECT h:443 HTTP/1.1\r\nproxy-authorization: Bearer t\r\n\r\n";
        let (connect, _) = parse(head).expect("parses");
        assert_eq!(connect.authorization.as_deref(), Some("Bearer t"));
    }

    #[test]
    fn a_bracketed_ipv6_authority_parses() {
        let head = b"CONNECT [::1]:8443 HTTP/1.1\r\n\r\n";
        let (connect, _) = parse(head).expect("parses");
        assert_eq!(connect.host, "::1");
        assert_eq!(connect.port, 8443);
    }

    #[test]
    fn the_bare_lf_head_reports_its_length() {
        // The length must include the blank line, so a `find_head_end` that
        // under-counts is caught: without the assertion, a shortened head still
        // parses to the same request line.
        let head = b"CONNECT h:443 HTTP/1.1\n\n";
        let (connect, len) = parse(head).expect("parses");
        assert_eq!(connect.port, 443);
        assert_eq!(len, head.len());
    }

    #[test]
    fn extra_headers_are_ignored() {
        let head = b"CONNECT h:443 HTTP/1.1\r\nHost: h\r\n\r\n";
        let (connect, _) = parse(head).expect("parses");
        assert_eq!(connect.host, "h");
    }

    #[test]
    fn an_oversized_head_is_rejected() {
        let head = vec![b'a'; MAX_HEAD_BYTES + 1];
        assert_eq!(parse(&head), Err(ParseError::HeadTooLarge));
    }

    #[test]
    fn a_head_of_exactly_the_cap_is_not_too_large() {
        // The cap check is `>`, so exactly `MAX_HEAD_BYTES` must not be
        // `HeadTooLarge`; all-`a` with no blank line is `Incomplete` instead.
        // This pins the boundary a `>=` would move.
        let head = vec![b'a'; MAX_HEAD_BYTES];
        assert_eq!(parse(&head), Err(ParseError::Incomplete));
    }

    #[test]
    fn the_cap_is_eight_kib() {
        // Pinned as a number so a mutant on the `*` is caught.
        assert_eq!(MAX_HEAD_BYTES, 8 * 1024);
    }

    #[test]
    fn the_head_length_counts_both_line_ending_styles() {
        // A `find_head_end` that adds the wrong constant returns a length inside
        // the blank line; the exact equality catches it.
        let crlf = b"CONNECT h:1 HTTP/1.1\r\n\r\n";
        let (_, len) = parse(crlf).expect("parses");
        assert_eq!(len, crlf.len());

        let lf = b"CONNECT h:1 HTTP/1.1\n\n";
        let (_, len) = parse(lf).expect("parses");
        assert_eq!(len, lf.len());
    }

    #[test]
    fn a_head_without_a_blank_line_is_incomplete() {
        assert_eq!(
            parse(b"CONNECT h:443 HTTP/1.1\r\n"),
            Err(ParseError::Incomplete)
        );
    }

    #[test]
    fn a_non_connect_method_is_rejected() {
        assert_eq!(
            parse(b"GET / HTTP/1.1\r\n\r\n"),
            Err(ParseError::NotConnect)
        );
    }

    #[test]
    fn a_non_http_version_is_rejected() {
        assert_eq!(
            parse(b"CONNECT h:443 FOO/1.1\r\n\r\n"),
            Err(ParseError::NotConnect)
        );
    }

    #[test]
    fn a_missing_authority_is_rejected() {
        assert_eq!(
            parse(b"CONNECT HTTP/1.1\r\n\r\n"),
            Err(ParseError::NotConnect)
        );
    }

    #[test]
    fn a_portless_authority_is_rejected() {
        assert_eq!(
            parse(b"CONNECT api.example.com HTTP/1.1\r\n\r\n"),
            Err(ParseError::BadAuthority)
        );
    }

    #[test]
    fn a_non_numeric_or_zero_port_is_rejected() {
        assert_eq!(
            parse(b"CONNECT h:abc HTTP/1.1\r\n\r\n"),
            Err(ParseError::BadPort)
        );
        assert_eq!(
            parse(b"CONNECT h:0 HTTP/1.1\r\n\r\n"),
            Err(ParseError::BadPort)
        );
        assert_eq!(
            parse(b"CONNECT h:65536 HTTP/1.1\r\n\r\n"),
            Err(ParseError::BadPort)
        );
    }

    #[test]
    fn a_header_without_a_colon_is_rejected() {
        assert_eq!(
            parse(b"CONNECT h:443 HTTP/1.1\r\nbroken\r\n\r\n"),
            Err(ParseError::BadHeader)
        );
    }

    #[test]
    fn non_utf8_is_rejected() {
        assert_eq!(
            parse(&[0xff, 0xfe, b'\r', b'\n', b'\r', b'\n']),
            Err(ParseError::NotConnect)
        );
    }

    #[test]
    fn an_empty_bracketed_host_is_rejected() {
        assert_eq!(
            parse(b"CONNECT []:443 HTTP/1.1\r\n\r\n"),
            Err(ParseError::BadAuthority)
        );
    }
}
