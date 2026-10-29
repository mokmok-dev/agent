//! The one object a tool writes.

use std::io;

use serde::Serialize;

use crate::error::ToolError;

const FALLBACK: &[u8] =
    br#"{"ok":false,"error":{"code":"io","message":"the result cannot be serialized"}}"#;

#[derive(Serialize)]
struct Success<T> {
    ok: bool,
    data: T,
}

#[derive(Serialize)]
struct Failure {
    ok: bool,
    error: Body,
}

#[derive(Serialize)]
struct Body {
    code: &'static str,
    message: String,
}

pub fn success<T: Serialize>(data: &T) -> Vec<u8> {
    match serde_json::to_vec(&Success { ok: true, data }) {
        Ok(bytes) => bytes,
        Err(error) => failure(&ToolError::new(
            crate::Code::Io,
            format!("the result cannot be serialized: {error}"),
        )),
    }
}

pub fn failure(error: &ToolError) -> Vec<u8> {
    let framed = Failure {
        ok: false,
        error: Body {
            code: error.code().as_str(),
            message: error.message().to_string(),
        },
    };
    serde_json::to_vec(&framed).unwrap_or_else(|_| FALLBACK.to_vec())
}

struct Count(usize);

impl io::Write for Count {
    fn write(
        &mut self,
        buffer: &[u8],
    ) -> io::Result<usize> {
        self.0 += buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// How many bytes a value takes as compact JSON.
pub fn json_len<T: Serialize + ?Sized>(value: &T) -> usize {
    let mut count = Count(0);
    match serde_json::to_writer(&mut count, value) {
        Ok(()) => count.0,
        Err(_) => usize::MAX,
    }
}

/// Keeps the longest prefix of `items` whose result fits `cap` bytes of JSON.
pub fn fit<T, O: Serialize>(
    items: &[T],
    cap: usize,
    build: impl Fn(&[T], bool) -> O,
) -> O {
    let mut fits = 0;
    let mut beyond = items.len();
    while fits < beyond {
        let middle = fits + (beyond - fits).div_ceil(2);
        if json_len(&build(&items[..middle], middle < items.len())) <= cap {
            fits = middle;
        } else {
            beyond = middle - 1;
        }
    }
    let mut keep = fits;
    loop {
        let candidate = build(&items[..keep], keep < items.len());
        if json_len(&candidate) <= cap || keep == 0 {
            return candidate;
        }
        keep -= 1;
    }
}
