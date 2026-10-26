use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use super::{Event, MAX_EVENT, MAX_LINE, Parser, WireError};

fn data(text: &str) -> Event {
    Event::Data(text.to_string())
}

fn parse(chunks: &[&[u8]]) -> Vec<Event> {
    let mut parser = Parser::new();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(parser.push(chunk).expect("parses"));
    }
    events
}

#[test]
fn one_frame_is_one_event() {
    assert_eq!(parse(&[b"data: hello\n\n"]), vec![data("hello")]);
}

#[test]
fn a_carriage_return_and_a_line_feed_end_a_line_together() {
    assert_eq!(parse(&[b"data: hello\r\n\r\n"]), vec![data("hello")]);
}

#[test]
fn a_bare_carriage_return_ends_a_line() {
    assert_eq!(parse(&[b"data: hello\r\r"]), vec![data("hello")]);
}

#[test]
fn a_carriage_return_at_the_end_of_a_chunk_swallows_the_next_line_feed() {
    assert_eq!(parse(&[b"data: hello\r", b"\n\r\n"]), vec![data("hello")]);
}

#[test]
fn a_line_feed_after_a_bare_carriage_return_is_a_blank_line_not_a_second_one() {
    assert_eq!(
        parse(&[b"data: a\r", b"\n"]),
        Vec::new(),
        "the event has not been ended yet"
    );
}

#[test]
fn several_data_lines_join_with_a_newline() {
    assert_eq!(
        parse(&[b"data: one\ndata: two\ndata: three\n\n"]),
        vec![data("one\ntwo\nthree")]
    );
}

#[test]
fn a_single_space_after_the_colon_is_removed_and_the_rest_is_kept() {
    assert_eq!(
        parse(&[b"data:  two spaces\n\n"]),
        vec![data(" two spaces")]
    );
    assert_eq!(parse(&[b"data:tight\n\n"]), vec![data("tight")]);
}

#[test]
fn a_colon_inside_the_value_is_kept() {
    assert_eq!(parse(&[b"data: {\"a\":1}\n\n"]), vec![data("{\"a\":1}")]);
}

#[test]
fn a_comment_is_ignored() {
    assert_eq!(
        parse(&[b": keep-alive\ndata: hello\n\n"]),
        vec![data("hello")]
    );
}

#[test]
fn another_field_is_ignored() {
    assert_eq!(
        parse(&[b"event: message\nid: 7\nretry: 100\ndata: hello\n\n"]),
        vec![data("hello")]
    );
}

#[test]
fn an_event_without_data_is_not_dispatched() {
    assert_eq!(parse(&[b"event: ping\n\n"]), Vec::new());
    assert_eq!(parse(&[b"\n\n\n"]), Vec::new());
}

#[test]
fn two_frames_in_one_chunk_both_come_out() {
    assert_eq!(
        parse(&[b"data: one\n\ndata: two\n\n"]),
        vec![data("one"), data("two")]
    );
}

#[test]
fn a_frame_split_across_chunks_is_reassembled() {
    assert_eq!(
        parse(&[b"data: he", b"ll", b"o\n", b"\n"]),
        vec![data("hello")]
    );
}

#[test]
fn a_multibyte_character_split_across_chunks_survives() {
    let bytes = "é".as_bytes();
    assert_eq!(
        parse(&[
            &[b'd', b'a', b't', b'a', b':', b' ', bytes[0]],
            &[bytes[1]],
            b"\n\n"
        ]),
        vec![data("é")]
    );
}

#[test]
fn a_data_field_that_is_not_utf8_is_refused() {
    let mut parser = Parser::new();
    assert_eq!(parser.push(b"data: \xff\xfe\n\n"), Err(WireError::NotUtf8));
}

#[test]
fn a_line_without_a_colon_is_a_field_with_no_value() {
    assert_eq!(parse(&[b"data\n\n"]), vec![data("")]);
    assert_eq!(parse(&[b"nodatafield\n\n"]), Vec::new());
}

#[test]
fn a_stream_that_ends_without_a_blank_line_still_yields_its_data() {
    let mut parser = Parser::new();
    assert_eq!(parser.push(b"data: hello").expect("parses"), Vec::new());
    assert_eq!(parser.finish().expect("finishes"), vec![data("hello")]);
}

#[test]
fn finishing_a_stream_with_nothing_pending_yields_nothing() {
    let mut parser = Parser::new();
    assert_eq!(parser.push(b"data: hello\n\n").expect("parses").len(), 1);
    assert_eq!(parser.finish().expect("finishes"), Vec::new());
}

#[test]
fn a_line_longer_than_the_bound_is_refused() {
    let mut parser = Parser::new();
    let line = vec![b'x'; MAX_LINE];
    assert_eq!(parser.push(&line).expect("parses"), Vec::new());
    assert_eq!(parser.push(b"x"), Err(WireError::LineTooLong));
}

fn fill_to_the_event_bound(parser: &mut Parser) -> usize {
    let mut written = 0;
    while written < MAX_EVENT {
        let room = MAX_EVENT - written;
        let text = core::cmp::min(room - 1, MAX_LINE - 7);
        let mut line = Vec::from(&b"data: "[..]);
        line.extend_from_slice(&vec![b'x'; text]);
        line.push(b'\n');
        parser.push(&line).expect("accepts up to the bound");
        written += text + 1;
    }
    written
}

#[test]
fn an_event_of_exactly_the_bound_is_accepted() {
    let mut parser = Parser::new();
    assert_eq!(fill_to_the_event_bound(&mut parser), MAX_EVENT);
}

#[test]
fn one_byte_past_the_bound_is_refused() {
    let mut parser = Parser::new();
    fill_to_the_event_bound(&mut parser);
    assert_eq!(parser.push(b"data: y\n"), Err(WireError::EventTooLong));
}

#[test]
fn every_error_explains_itself() {
    for error in [
        WireError::LineTooLong,
        WireError::EventTooLong,
        WireError::NotUtf8,
    ] {
        assert_ne!(error.to_string(), "");
    }
}
