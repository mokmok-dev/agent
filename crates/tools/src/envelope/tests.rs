//! The cut: the longest prefix whose answer still fits the bytes it may take.

use serde::Serialize;

use super::{failure, fit, json_len};
use crate::error::ToolError;

#[derive(Debug, Serialize)]
struct Framed {
    kept: Vec<u8>,
    truncated: bool,
}

fn build(
    items: &[u8],
    over: bool,
) -> Framed {
    Framed {
        kept: items.to_vec(),
        truncated: over,
    }
}

#[test]
fn everything_is_kept_when_everything_fits() {
    let out = fit(&[1_u8, 2, 3], 1024, build);
    assert_eq!(out.kept, vec![1, 2, 3]);
    assert!(!out.truncated, "a whole answer was reported as cut");
}

#[test]
fn the_longest_prefix_that_fits_exactly_is_kept() {
    let two = json_len(&build(&[1_u8, 2], true));
    let out = fit(&[1_u8, 2, 3], two, build);
    assert_eq!(out.kept, vec![1, 2]);
    assert!(out.truncated);
}

#[test]
fn a_prefix_that_leaves_room_to_spare_is_still_the_longest() {
    let two = json_len(&build(&[1_u8, 2], true));
    let out = fit(&[1_u8, 2, 3], two + 1, build);
    assert_eq!(out.kept, vec![1, 2]);
    assert!(out.truncated);
}

#[test]
fn nothing_fits_a_cap_under_one_empty_answer() {
    let empty = json_len(&build(&[], true));
    let out = fit(&[1_u8, 2, 3], empty - 1, build);
    assert_eq!(out.kept, Vec::<u8>::new());
    assert!(out.truncated);
}

#[test]
fn an_empty_input_is_answerable_whatever_the_cap() {
    let out = fit(&[] as &[u8], 0, build);
    assert_eq!(out.kept, Vec::<u8>::new());
    assert!(!out.truncated);
}

#[test]
fn a_failure_is_one_object_with_the_code_and_the_message() {
    let bytes = failure(&ToolError::new(crate::Code::Io, "the disk said no"));
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("one JSON object");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "io");
    assert_eq!(value["error"]["message"], "the disk said no");
}
