//! A pure unified-diff parser: `&str` in, structure out.
//!
//! This module owns the grammar of a unified diff and nothing else. It never
//! touches the filesystem and never decides whether a path is safe; path policy
//! belongs to [`crate::coding`]'s guard. That split is deliberate: the grammar is
//! a function of the text alone, so it is exercised exhaustively with literals,
//! and the one thing that needs the filesystem (a path's resolved identity) is
//! checked in one place against the workspace root.
//!
//! The parser accepts what `git apply` accepts: git's own metadata lines, classic
//! `diff -u` timestamps, `/dev/null` on one side of a creation or deletion, hunks
//! whose line counts it verifies exactly, and the `\ No newline at end of file`
//! marker. It refuses binary patches, renames and copies (this tool applies text
//! changes), quoted paths, and any structural damage, each with the terse reason
//! the model is told.

/// The most diff text this parser will look at, a quarter mebibyte.
pub const MAX_DIFF_BYTES: usize = 256 * 1024;

/// The most file sections one patch may name.
pub const MAX_PATCH_FILES: usize = 64;

/// One file section of a validated diff, as workspace-relative paths with the
/// `-p` prefix already stripped. `None` is `/dev/null` (a creation or deletion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// The path the section removes, or `None` for a creation.
    pub old: Option<String>,
    /// The path the section adds, or `None` for a deletion.
    pub new: Option<String>,
}

/// A validated diff: its file sections and the `-p` strip level they share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// The file sections, in the order they appear.
    pub files: Vec<File>,
    /// The number of leading components to strip: `1` for `a/`/`b/`, `0` otherwise.
    pub strip: u8,
}

/// The structural failure, with the terse reason the model is told.
#[derive(Debug)]
pub struct Error(&'static str);

impl Error {
    /// The reason, suitable for telling the model.
    pub const fn reason(&self) -> &'static str {
        self.0
    }
}

/// `a hunk line has no prefix`.
const NO_PREFIX: &str = "a hunk line has no prefix";
/// `a hunk header does not match its lines`.
const MISMATCH: &str = "a hunk header does not match its lines";
/// `a hunk header is malformed`.
const MALFORMED: &str = "a hunk header is malformed";
/// `the diff mixes prefixed and unprefixed header paths`.
const MIXED: &str = "the diff mixes prefixed and unprefixed header paths";
/// `the diff is not a text patch this tool applies`.
const NOT_TEXT_PATCH: &str = "the diff is not a text patch this tool applies";

/// Lines git writes that carry no content and that this tool tolerates.
const METADATA: [&str; 8] = [
    "diff --git ",
    "index ",
    "old mode ",
    "new mode ",
    "new file mode ",
    "deleted file mode ",
    "similarity index ",
    "dissimilarity index ",
];

/// Lines git writes for a rename or a copy, which this tool does not apply.
const RENAME: [&str; 4] = ["rename from ", "rename to ", "copy from ", "copy to "];

/// Parse and syntax-check a unified diff.
///
/// # Errors
///
/// Returns [`Error`] when the diff is empty, too large, not text, not a textual
/// patch, structurally malformed, or mixes prefixed and unprefixed paths.
pub fn parse(diff: &str) -> Result<Parsed, Error> {
    if diff.is_empty() {
        return Err(Error("the diff is empty"));
    }
    if diff.len() > MAX_DIFF_BYTES {
        return Err(Error("the diff is larger than the limit"));
    }
    if diff.as_bytes().contains(&0) {
        return Err(Error("the diff is not text"));
    }
    for line in diff.split('\n') {
        if line.starts_with("GIT binary patch") || line.starts_with("Binary files ") {
            return Err(Error(NOT_TEXT_PATCH));
        }
    }

    // Split on `\n` and drop exactly one trailing empty element, which is the
    // one trailing newline; every other empty line is content and is judged as
    // such.
    let mut lines: Vec<&str> = diff.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }

    let mut raw: Vec<File> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(old_raw) = line.strip_prefix("--- ") {
            if old_raw.starts_with('"') {
                return Err(Error(NOT_TEXT_PATCH));
            }
            let old = header_path(old_raw);
            i += 1;

            let Some(new_line) = lines.get(i) else {
                return Err(Error("a file header is missing its `+++` line"));
            };
            let Some(new_raw) = new_line.strip_prefix("+++ ") else {
                return Err(Error("a file header is missing its `+++` line"));
            };
            if new_raw.starts_with('"') {
                return Err(Error(NOT_TEXT_PATCH));
            }
            let new = header_path(new_raw);
            i += 1;

            if old.is_none() && new.is_none() {
                return Err(Error("a file header names no file"));
            }

            let mut hunks = 0_usize;
            while lines.get(i).is_some_and(|line| line.starts_with("@@ ")) {
                let (old_count, new_count) = parse_hunk(lines[i])?;
                i += 1;
                i = consume_hunk(&lines, i, old_count, new_count)?;
                hunks += 1;

                // The counts were satisfied; a body line here means the header
                // undercounted rather than that the hunk ended.
                if let Some(follow) = lines.get(i)
                    && !follow.starts_with("--- ")
                    && matches!(follow.as_bytes().first(), Some(b' ' | b'-' | b'+'))
                {
                    return Err(Error(MISMATCH));
                }
            }
            if hunks == 0 {
                return Err(Error("a file section has no hunk"));
            }
            if raw.len() >= MAX_PATCH_FILES {
                return Err(Error("the diff names too many files"));
            }
            raw.push(File { old, new });
        } else if METADATA.iter().any(|prefix| line.starts_with(prefix)) {
            i += 1;
        } else if RENAME.iter().any(|prefix| line.starts_with(prefix)) {
            return Err(Error(NOT_TEXT_PATCH));
        } else {
            return Err(Error("the diff is not a unified diff"));
        }
    }

    if raw.is_empty() {
        return Err(Error("the diff names no files"));
    }

    let strip = detect_strip(&raw)?;
    let files = raw
        .into_iter()
        .map(|file| strip_file(file, strip))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Parsed { files, strip })
}

/// The path a `--- `/`+++ ` header names, its `\t`-separated timestamp removed,
/// or `None` for `/dev/null`.
fn header_path(raw: &str) -> Option<String> {
    let path = raw.split('\t').next().unwrap_or(raw).trim_end();
    if path == "/dev/null" {
        None
    } else {
        Some(path.to_owned())
    }
}

/// Read one hunk header's line counts, defaulting a missing count to `1`.
fn parse_hunk(line: &str) -> Result<(u64, u64), Error> {
    let rest = line.strip_prefix("@@ ").ok_or(Error(MALFORMED))?;
    let rest = rest.strip_prefix('-').ok_or(Error(MALFORMED))?;
    let (old_spec, rest) = rest.split_once(' ').ok_or(Error(MALFORMED))?;
    let rest = rest.strip_prefix('+').ok_or(Error(MALFORMED))?;
    let (new_spec, rest) = rest.split_once(' ').ok_or(Error(MALFORMED))?;
    rest.strip_prefix("@@").ok_or(Error(MALFORMED))?;
    Ok((count_of(old_spec)?, count_of(new_spec)?))
}

/// The count of a `start[,count]` hunk range, defaulting `count` to `1`.
fn count_of(spec: &str) -> Result<u64, Error> {
    let (start, count) = match spec.split_once(',') {
        Some((start, count)) => (start, count),
        None => (spec, "1"),
    };
    start.parse::<u64>().map_err(|_| Error(MALFORMED))?;
    count.parse::<u64>().map_err(|_| Error(MALFORMED))
}

/// Consume exactly `old_count` and `new_count` body lines from `lines`, starting
/// at `start`, and return the index just past them.
fn consume_hunk(
    lines: &[&str],
    mut i: usize,
    old_count: u64,
    new_count: u64,
) -> Result<usize, Error> {
    let mut old_left = old_count;
    let mut new_left = new_count;
    while old_left > 0 || new_left > 0 {
        let Some(body) = lines.get(i) else {
            return Err(Error(MISMATCH));
        };
        match body.as_bytes().first().copied() {
            Some(b' ') if old_left > 0 && new_left > 0 => {
                old_left -= 1;
                new_left -= 1;
            },
            Some(b'-') if old_left > 0 => old_left -= 1,
            Some(b'+') if new_left > 0 => new_left -= 1,
            // `\ No newline at end of file` marks the previous line and belongs to
            // neither side, so it consumes no count.
            Some(b'\\') => {},
            Some(b' ' | b'-' | b'+') => return Err(Error(MISMATCH)),
            _ => return Err(Error(NO_PREFIX)),
        }
        i += 1;
    }
    // A `\ No newline at end of file` marker names the line before it, so one
    // may trail the hunk after its counts are already satisfied.
    while lines
        .get(i)
        .is_some_and(|line| line.as_bytes().first() == Some(&b'\\'))
    {
        i += 1;
    }
    Ok(i)
}

/// The one strip level that fits every section, or a refusal when the sections
/// disagree.
///
/// A section is prefixed when every path it names carries its side's prefix and
/// unprefixed when none does; a section that does both, and a diff whose sections
/// disagree, are the mix this refuses. One strip value has to govern the whole
/// diff, because that is what a single `git apply -p{n}` takes.
fn detect_strip(files: &[File]) -> Result<u8, Error> {
    let mut any_prefixed = false;
    let mut any_unprefixed = false;
    for file in files {
        let prefixed = file
            .old
            .as_deref()
            .is_none_or(|path| path.starts_with("a/"))
            && file
                .new
                .as_deref()
                .is_none_or(|path| path.starts_with("b/"));
        let unprefixed = file
            .old
            .as_deref()
            .is_none_or(|path| !path.starts_with("a/"))
            && file
                .new
                .as_deref()
                .is_none_or(|path| !path.starts_with("b/"));
        if !prefixed && !unprefixed {
            return Err(Error(MIXED));
        }
        any_prefixed |= prefixed;
        any_unprefixed |= unprefixed;
    }
    match (any_prefixed, any_unprefixed) {
        (true, false) => Ok(1),
        (false, true) => Ok(0),
        _ => Err(Error(MIXED)),
    }
}

/// Strip one section's `a/`/`b/` prefix when `strip` is `1`, refusing an empty
/// result.
fn strip_file(
    file: File,
    strip: u8,
) -> Result<File, Error> {
    Ok(File {
        old: strip_side(file.old, "a/", strip)?,
        new: strip_side(file.new, "b/", strip)?,
    })
}

/// Strip `prefix` from one side's path when `strip` is `1`.
fn strip_side(
    path: Option<String>,
    prefix: &str,
    strip: u8,
) -> Result<Option<String>, Error> {
    let Some(path) = path else {
        return Ok(None);
    };
    let path = if strip == 1 {
        path.strip_prefix(prefix).unwrap_or(&path).to_owned()
    } else {
        path
    };
    if path.is_empty() {
        return Err(Error("a file header names an empty path"));
    }
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    // Tests for the pure parser. Every case is a literal diff in and a literal
    // structure or reason out; no filesystem, no child.

    use super::*;

    /// A file section with these two sides.
    fn file(
        old: Option<&str>,
        new: Option<&str>,
    ) -> File {
        File {
            old: old.map(str::to_owned),
            new: new.map(str::to_owned),
        }
    }

    /// Parse `diff`, expecting it to be accepted.
    fn accepts(diff: &str) -> Parsed {
        parse(diff).unwrap_or_else(|error| panic!("expected a valid diff: {}", error.reason()))
    }

    /// Parse `diff`, expecting this exact reason.
    fn refuses(
        diff: &str,
        reason: &str,
    ) {
        match parse(diff) {
            Ok(parsed) => panic!("expected a refusal, got {parsed:?}"),
            Err(error) => assert_eq!(error.reason(), reason, "diff was:\n{diff}"),
        }
    }

    #[test]
    fn the_bounds_are_pinned() {
        assert_eq!(MAX_DIFF_BYTES, 262_144);
        assert_eq!(MAX_PATCH_FILES, 64);
    }

    #[test]
    fn an_empty_diff_is_refused() {
        refuses("", "the diff is empty");
    }

    #[test]
    fn a_diff_with_a_nul_byte_is_refused() {
        refuses("--- f\n+++ f\n@@ -1 +1 @@\n a\0\n", "the diff is not text");
    }

    #[test]
    fn a_binary_patch_is_refused() {
        refuses(
            "GIT binary patch\n",
            "the diff is not a text patch this tool applies",
        );
        refuses(
            "Binary files a/x and b/x differ\n",
            "the diff is not a text patch this tool applies",
        );
    }

    #[test]
    fn a_quoted_path_is_refused() {
        refuses(
            "--- \"a/x\"\n+++ \"b/x\"\n@@ -1 +1 @@\n a\n",
            "the diff is not a text patch this tool applies",
        );
    }

    #[test]
    fn a_rename_is_refused() {
        refuses(
            "diff --git a/x b/y\nrename from x\nrename to y\n--- a/x\n+++ b/y\n@@ -1 +1 @@\n a\n",
            "the diff is not a text patch this tool applies",
        );
    }

    #[test]
    fn a_git_style_diff_strips_one_component() {
        let parsed = accepts(
            "diff --git a/x b/x\nindex 111..222 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n",
        );
        assert_eq!(parsed.strip, 1);
        assert_eq!(parsed.files, vec![file(Some("x"), Some("x"))]);
    }

    #[test]
    fn an_unprefixed_nested_path_strips_nothing() {
        // A real `git apply -p1` would strip `sub/` and miss the file; the parser
        // records `strip = 0` so the caller passes `-p0`.
        let parsed = accepts("--- sub/f.txt\n+++ sub/f.txt\n@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!(parsed.strip, 0);
        assert_eq!(
            parsed.files,
            vec![file(Some("sub/f.txt"), Some("sub/f.txt"))]
        );
    }

    #[test]
    fn one_section_prefixed_and_one_not_is_refused() {
        refuses(
            "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n--- y\n+++ y\n@@ -1 +1 @@\n-a\n+b\n",
            "the diff mixes prefixed and unprefixed header paths",
        );
    }

    #[test]
    fn a_section_that_mixes_prefixes_is_refused() {
        refuses(
            "--- a/x\n+++ x\n@@ -1 +1 @@\n-a\n+b\n",
            "the diff mixes prefixed and unprefixed header paths",
        );
    }

    #[test]
    fn a_creation_names_no_old_path() {
        let parsed = accepts("--- /dev/null\n+++ b/x\n@@ -0,0 +1 @@\n+new\n");
        assert_eq!(parsed.strip, 1);
        assert_eq!(parsed.files, vec![file(None, Some("x"))]);
    }

    #[test]
    fn a_deletion_names_no_new_path() {
        let parsed = accepts("--- a/x\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n");
        assert_eq!(parsed.strip, 1);
        assert_eq!(parsed.files, vec![file(Some("x"), None)]);
    }

    #[test]
    fn an_unprefixed_creation_strips_nothing() {
        let parsed = accepts("--- /dev/null\n+++ x\n@@ -0,0 +1 @@\n+new\n");
        assert_eq!(parsed.strip, 0);
        assert_eq!(parsed.files, vec![file(None, Some("x"))]);
    }

    #[test]
    fn a_header_without_its_plus_line_is_refused() {
        refuses(
            "--- a/x\n@@ -1 +1 @@\n a\n",
            "a file header is missing its `+++` line",
        );
    }

    #[test]
    fn a_header_naming_no_file_is_refused() {
        refuses(
            "--- /dev/null\n+++ /dev/null\n",
            "a file header names no file",
        );
    }

    #[test]
    fn an_empty_path_after_stripping_is_refused() {
        refuses(
            "--- a/\n+++ b/\n@@ -1 +1 @@\n-a\n+b\n",
            "a file header names an empty path",
        );
    }

    #[test]
    fn a_missing_hunk_count_defaults_to_one() {
        let parsed = accepts("--- f\n+++ f\n@@ -3 +4 @@\n-a\n+b\n");
        assert_eq!(parsed.strip, 0);
        assert_eq!(parsed.files, vec![file(Some("f"), Some("f"))]);
    }

    #[test]
    fn a_malformed_hunk_header_is_refused() {
        refuses("--- f\n+++ f\n@@ nope @@\n", "a hunk header is malformed");
    }

    #[test]
    fn a_hunk_count_one_too_many_is_refused() {
        refuses(
            "--- f\n+++ f\n@@ -1,2 +1,2 @@\n a\n",
            "a hunk header does not match its lines",
        );
    }

    #[test]
    fn a_hunk_count_one_too_few_is_refused() {
        refuses(
            "--- f\n+++ f\n@@ -1 +1 @@\n a\n-b\n",
            "a hunk header does not match its lines",
        );
    }

    #[test]
    fn a_deletion_line_that_begins_with_dashes_is_a_body_line() {
        // The raw body line is `--- gone` (the `-` prefix plus the content
        // `-- gone`); it is a deletion, not a file header, because the counts are
        // read exactly and the section only ends when they are satisfied.
        let parsed = accepts("--- f\n+++ f\n@@ -2,2 +2,2 @@\n keep\n--- gone\n+new\n");
        assert_eq!(parsed.files, vec![file(Some("f"), Some("f"))]);
    }

    #[test]
    fn a_context_line_with_no_removal_left_is_refused() {
        // The counts are exact, and a line that would need a side already at zero is
        // a mismatch rather than a line with an unknown prefix.
        refuses(
            "--- f\n+++ f\n@@ -1 +0,0 @@\n a\n",
            "a hunk header does not match its lines",
        );
    }

    #[test]
    fn a_context_line_with_no_addition_left_is_refused() {
        refuses(
            "--- f\n+++ f\n@@ -0,0 +1 @@\n a\n",
            "a hunk header does not match its lines",
        );
    }

    #[test]
    fn a_removal_with_no_removal_left_is_refused() {
        refuses(
            "--- f\n+++ f\n@@ -0,0 +1 @@\n-x\n",
            "a hunk header does not match its lines",
        );
    }

    #[test]
    fn an_addition_with_no_addition_left_is_refused() {
        refuses(
            "--- f\n+++ f\n@@ -1,1 +0,0 @@\n+x\n",
            "a hunk header does not match its lines",
        );
    }

    #[test]
    fn a_no_newline_marker_counts_for_neither_side() {
        let parsed = accepts(
            "--- f\n+++ f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n",
        );
        assert_eq!(parsed.files, vec![file(Some("f"), Some("f"))]);
    }

    #[test]
    fn a_hunk_line_without_a_prefix_is_refused() {
        refuses(
            "--- f\n+++ f\n@@ -1 +1 @@\nx\n",
            "a hunk line has no prefix",
        );
    }

    #[test]
    fn a_section_with_no_hunk_is_refused() {
        refuses("--- f\n+++ f\n", "a file section has no hunk");
    }

    #[test]
    fn a_line_outside_a_header_is_refused() {
        refuses("nonsense\n", "the diff is not a unified diff");
    }

    #[test]
    fn a_diff_with_no_file_sections_is_refused() {
        refuses(
            "diff --git a/x b/x\nindex 111..222 100644\n",
            "the diff names no files",
        );
    }

    #[test]
    fn a_header_timestamp_is_not_part_of_the_path() {
        let parsed = accepts(
            "--- f\t2020-01-01 00:00:00\n+++ f\t2020-01-01 00:00:00\n@@ -1 +1 @@\n-a\n+b\n",
        );
        assert_eq!(parsed.files, vec![file(Some("f"), Some("f"))]);
    }

    #[test]
    fn exactly_the_size_bound_is_accepted_and_one_more_is_refused() {
        let base = "--- f\n+++ f\n@@ -1,1 +1,1 @@\n ";
        let pad = "x".repeat(MAX_DIFF_BYTES - base.len());
        let exact = format!("{base}{pad}");
        assert_eq!(exact.len(), MAX_DIFF_BYTES);
        assert_eq!(accepts(&exact).strip, 0);

        let over = format!("{exact}y");
        assert_eq!(over.len(), MAX_DIFF_BYTES + 1);
        refuses(&over, "the diff is larger than the limit");
    }

    #[test]
    fn exactly_the_file_bound_is_accepted_and_one_more_is_refused() {
        let section = |i: usize| format!("--- f{i}\n+++ f{i}\n@@ -1 +1 @@\n a\n");
        let exact: String = (0..MAX_PATCH_FILES).map(section).collect();
        assert_eq!(accepts(&exact).files.len(), MAX_PATCH_FILES);

        let over: String = (0..=MAX_PATCH_FILES).map(section).collect();
        refuses(&over, "the diff names too many files");
    }
}
