use std::path::Path;

use super::{Code, ToolError};

#[test]
fn an_error_reads_as_the_sentence_it_carries() {
    let error = ToolError::new(Code::TooLarge, "note.txt is over its cap");
    assert_eq!(error.to_string(), "note.txt is over its cap");
    assert_eq!(format!("{error}"), error.message());
}

#[test]
fn every_code_has_the_spelling_the_contract_names() {
    let codes = [
        (Code::InvalidArgs, "invalid_args"),
        (Code::OutsideWorkspace, "outside_workspace"),
        (Code::NotFound, "not_found"),
        (Code::NotAFile, "not_a_file"),
        (Code::NotText, "not_text"),
        (Code::TooLarge, "too_large"),
        (Code::Ambiguous, "ambiguous"),
        (Code::ProgramNotAllowed, "program_not_allowed"),
        (Code::Io, "io"),
        (Code::Exec, "exec"),
    ];
    let mut seen = Vec::new();
    for (code, spelling) in codes {
        assert_eq!(code.as_str(), spelling);
        seen.push(spelling);
    }
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 10, "two codes spell the same word");
}

#[test]
fn an_io_error_names_the_path_and_the_kernel_said_what_happened() {
    let error = ToolError::io(
        Path::new("/tmp/note.txt"),
        &std::io::Error::from(std::io::ErrorKind::NotFound),
    );
    assert_eq!(error.code(), Code::Io);
    assert!(error.message().starts_with("/tmp/note.txt: "), "{error}");
}
