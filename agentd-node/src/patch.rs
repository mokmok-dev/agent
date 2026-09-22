//! A unified-diff patcher: parse a diff, apply it to a file's content, and
//! invert it.
//!
//! The diff is the mechanism that makes an agent's edit reviewable and
//! reversible: [`Patch::inverse`] returns the patch that undoes an applied one
//! — including the deletion that undoes a creation — so an undo needs nothing
//! but the log entry that carried it.
//!
//! The representation is exact byte-for-byte, so a patch that touches the last
//! line of a file without a trailing newline round-trips: each line records
//! whether it ended in a newline, which is what the `\ No newline at end of
//! file` marker denotes. The patch text is read as lines, so a patch's own line
//! endings are not content; that also means a file whose lines end in CRLF
//! cannot be patched, because the `\r` it carries is part of each line's text
//! while the patch's own `\r` is not.
//!
//! Application is all-or-nothing by construction: [`Patch::apply`] returns every
//! file's result only once every one of them applied, so a caller writes nothing
//! unless the whole patch holds.

use std::fmt::Write as _;
use thiserror::Error;

/// The path a unified diff uses for the absent side of a creation or deletion.
const DEV_NULL: &str = "/dev/null";

/// The marker that says the preceding hunk line has no trailing newline.
const NO_NEWLINE: &str = r"\ No newline at end of file";

/// Why a patch could not be parsed or applied.
#[derive(Debug, Error)]
pub enum PatchError {
    /// The patch holds no file.
    #[error("the patch names no file")]
    Empty,
    /// A line is not part of a unified diff.
    #[error("line {line} of the patch is not part of a unified diff: {text:?}")]
    Syntax {
        /// The one-based line in the patch text.
        line: usize,
        /// The line as it was written.
        text: String,
    },
    /// A hunk header is malformed.
    #[error("line {line} of the patch is not a hunk header: {text:?}")]
    Header {
        /// The one-based line in the patch text.
        line: usize,
        /// The line as it was written.
        text: String,
    },
    /// A no-newline marker follows no hunk line.
    #[error("the no-newline marker on line {line} follows no line")]
    DanglingMarker {
        /// The one-based line in the patch text.
        line: usize,
    },
    /// The patch names the same file twice, which would apply only the last of
    /// its sections.
    #[error("the patch names {0} twice; a file is named by one section")]
    Duplicate(String),
    /// The patch renames a file, which this patcher does not do.
    #[error("the patch renames {old} to {new}; this patcher edits files in place")]
    RenameUnsupported {
        /// The path the patch reads.
        old: String,
        /// The path the patch writes.
        new: String,
    },
    /// A file the patch edits could not be read.
    #[error("{path} could not be read: {error}")]
    Read {
        /// The path the patch names.
        path: String,
        /// The read failure.
        #[source]
        error: std::io::Error,
    },
    /// The patch deletes a file but leaves lines in it.
    #[error("the patch deletes {path} but its hunks leave {lines} lines behind")]
    DeleteIncomplete {
        /// The path the patch names.
        path: String,
        /// How many lines the hunks would leave.
        lines: usize,
    },
    /// A hunk does not match the file, so the patch is rejected whole.
    #[error(
        "hunk {hunk} of {path} does not match the file: it removes {expected}, but the \
         file has {found}"
    )]
    Mismatch {
        /// The path the patch names.
        path: String,
        /// The one-based hunk number within that file.
        hunk: usize,
        /// The lines the hunk expects to find.
        expected: String,
        /// The lines the file has where the hunk applies.
        found: String,
    },
    /// A hunk starts before the previous one ends.
    #[error("hunk {hunk} of {path} starts before the hunk before it ends")]
    Overlap {
        /// The path the patch names.
        path: String,
        /// The one-based hunk number within that file.
        hunk: usize,
    },
}

/// A parsed unified diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    files: Vec<FilePatch>,
}

/// One file of a patch.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FilePatch {
    /// The path the `---` header names, with a leading `a/` stripped.
    old_path: String,
    /// The path the `+++` header names, with a leading `b/` stripped.
    new_path: String,
    hunks: Vec<Hunk>,
}

/// One hunk: where it applies on each side, and the lines it carries.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Hunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    body: Vec<BodyLine>,
}

/// Which side of the diff a hunk line belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// A line both sides have.
    Context,
    /// A line only the original has.
    Old,
    /// A line only the result has.
    New,
}

impl Side {
    /// The prefix a unified diff writes for this side.
    const fn prefix(self) -> char {
        match self {
            Self::Context => ' ',
            Self::Old => '-',
            Self::New => '+',
        }
    }

    /// The side the inverse patch carries this line on.
    const fn inverse(self) -> Self {
        match self {
            Self::Old => Self::New,
            Self::New => Self::Old,
            Self::Context => Self::Context,
        }
    }
}

/// One line of a hunk body.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BodyLine {
    side: Side,
    text: String,
    newline: bool,
}

/// One line of a file, and whether it ended in a newline.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Line {
    text: String,
    newline: bool,
}

/// One file a patch applied to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The path the patch edited, as the patch names it.
    pub path: String,
    /// The file's new content, or `None` when the patch deletes the file.
    pub content: Option<String>,
    /// The lines the patch added to it.
    pub added: usize,
    /// The lines the patch removed from it.
    pub removed: usize,
}

impl Patch {
    /// Parses the unified diff `text`.
    ///
    /// A hunk header's counts are recomputed from the body it carries, so a
    /// header whose counts disagree with its body is repaired rather than
    /// trusted; the starts are taken as written.
    ///
    /// # Errors
    ///
    /// Returns a [`PatchError`] for text that is not a unified diff, for a
    /// rename (which this patcher does not do), for a file named twice, and for
    /// a patch that names no file.
    pub fn parse(text: &str) -> Result<Self, PatchError> {
        let lines: Vec<&str> = text.lines().collect();
        let mut files = Vec::new();
        let mut index = 0;
        while index < lines.len() {
            if lines[index].trim().is_empty() {
                index += 1;
                continue;
            }
            if !is_file_header(&lines, index) {
                return Err(PatchError::Syntax {
                    line: index + 1,
                    text: lines[index].to_string(),
                });
            }
            let old_path = stripped_path(header_path(lines[index]));
            let new_path = stripped_path(header_path(lines[index + 1]));
            // A rename is two real paths that differ; a creation or a deletion
            // names the absent side `/dev/null`.
            let absent = old_path == DEV_NULL || new_path == DEV_NULL;
            if !absent && old_path != new_path {
                return Err(PatchError::RenameUnsupported {
                    old: old_path,
                    new: new_path,
                });
            }
            let (hunks, next) = parse_hunks(&lines, index + 2)?;
            index = next;
            let file = FilePatch {
                old_path,
                new_path,
                hunks,
            };
            // Two sections for one path would each be computed against the same
            // original and then written in turn, so only the last would survive
            // while the patch claimed both.
            if let Some(named) = files
                .iter()
                .find(|named: &&FilePatch| named.path() == file.path())
            {
                return Err(PatchError::Duplicate(named.path().to_string()));
            }
            files.push(file);
        }
        if files.is_empty() {
            return Err(PatchError::Empty);
        }
        Ok(Self { files })
    }

    /// Applies every file patch, reading each file's current content through
    /// `read` unless the patch creates it.
    ///
    /// The results are returned only once every file applied, so a caller that
    /// writes them afterwards writes nothing when any hunk does not match. A
    /// result whose content is `None` deletes the file, which is how the inverse
    /// of a creation undoes it.
    ///
    /// # Errors
    ///
    /// Returns [`PatchError::Read`] when a file cannot be read,
    /// [`PatchError::Mismatch`] when a hunk does not match the file it names, and
    /// [`PatchError::DeleteIncomplete`] when a deletion leaves lines behind.
    pub fn apply<F>(
        &self,
        mut read: F,
    ) -> Result<Vec<Applied>, PatchError>
    where
        F: FnMut(&str) -> std::io::Result<String>,
    {
        let mut applied = Vec::with_capacity(self.files.len());
        for file in &self.files {
            let path = file.path();
            let original = if file.creates() {
                String::new()
            } else {
                read(path).map_err(|error| PatchError::Read {
                    path: path.to_string(),
                    error,
                })?
            };
            let lines = split_lines(&original);
            let mut result: Vec<Line> = Vec::new();
            let mut cursor = 0;
            for (number, hunk) in file.hunks.iter().enumerate() {
                let start = hunk.old_start.saturating_sub(1);
                if start < cursor {
                    return Err(PatchError::Overlap {
                        path: path.to_string(),
                        hunk: number + 1,
                    });
                }
                let end = start.saturating_add(hunk.old_count);
                let expected: Vec<&BodyLine> = hunk
                    .body
                    .iter()
                    .filter(|line| line.side != Side::New)
                    .collect();
                let mismatch = || PatchError::Mismatch {
                    path: path.to_string(),
                    hunk: number + 1,
                    expected: preview(expected.iter().map(|line| line.text.as_str())),
                    found: preview(
                        lines
                            .get(start..)
                            .unwrap_or_default()
                            .iter()
                            .map(|line| line.text.as_str()),
                    ),
                };
                // A hunk whose range is not a range of the file cannot match,
                // so the range is checked before it is sliced.
                let Some(found) = lines.get(start..end) else {
                    return Err(mismatch());
                };
                if !matches(found, &expected) {
                    return Err(mismatch());
                }
                result.extend_from_slice(&lines[cursor..start]);
                result.extend(
                    hunk.body
                        .iter()
                        .filter(|line| line.side != Side::Old)
                        .map(|line| Line {
                            text: line.text.clone(),
                            newline: line.newline,
                        }),
                );
                cursor = end;
            }
            if cursor < lines.len() {
                result.extend_from_slice(&lines[cursor..]);
            }
            // A deletion the hunks do not complete would silently drop whatever
            // they left behind, because the file is removed rather than emptied.
            if file.deletes() && !result.is_empty() {
                return Err(PatchError::DeleteIncomplete {
                    path: path.to_string(),
                    lines: result.len(),
                });
            }
            applied.push(Applied {
                path: path.to_string(),
                content: (!file.deletes()).then(|| render_lines(&result)),
                added: file.count(Side::New),
                removed: file.count(Side::Old),
            });
        }
        Ok(applied)
    }

    /// The patch that undoes this one: the sides and their paths swapped, and
    /// each hunk's `-` lines turned into `+` lines and back.
    ///
    /// Applying it to the state this patch produced restores what it was applied
    /// to; the inverse of a creation is the deletion of the file it created.
    #[must_use]
    pub fn inverse(&self) -> Self {
        Self {
            files: self.files.iter().map(FilePatch::inverse).collect(),
        }
    }

    /// The paths the patch edits, in the order it names them.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(FilePatch::path)
    }
}

impl std::str::FromStr for Patch {
    type Err = PatchError;

    fn from_str(text: &str) -> Result<Self, PatchError> {
        Self::parse(text)
    }
}

impl FilePatch {
    /// The path this patch edits.
    fn path(&self) -> &str {
        if self.creates() {
            &self.new_path
        } else {
            &self.old_path
        }
    }

    /// Whether the patch creates the file, so there is no original to read.
    fn creates(&self) -> bool {
        self.old_path == DEV_NULL
    }

    /// Whether the patch deletes the file.
    fn deletes(&self) -> bool {
        self.new_path == DEV_NULL
    }

    /// How many lines of `side` the file patch carries.
    fn count(
        &self,
        side: Side,
    ) -> usize {
        self.hunks
            .iter()
            .flat_map(|hunk| hunk.body.iter())
            .filter(|line| line.side == side)
            .count()
    }

    fn inverse(&self) -> Self {
        Self {
            old_path: self.new_path.clone(),
            new_path: self.old_path.clone(),
            hunks: self.hunks.iter().map(Hunk::inverse).collect(),
        }
    }
}

impl Hunk {
    fn inverse(&self) -> Self {
        Self {
            old_start: self.new_start,
            old_count: self.new_count,
            new_start: self.old_start,
            new_count: self.old_count,
            body: self
                .body
                .iter()
                .map(|line| BodyLine {
                    side: line.side.inverse(),
                    text: line.text.clone(),
                    newline: line.newline,
                })
                .collect(),
        }
    }
}

impl std::fmt::Display for Patch {
    /// Writes the canonical diff form: explicit counts, `a/`/`b/` paths, and
    /// the no-newline marker where a line lacks one.
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        for file in &self.files {
            writeln!(formatter, "--- {}", header_name(&file.old_path, 'a'))?;
            writeln!(formatter, "+++ {}", header_name(&file.new_path, 'b'))?;
            for hunk in &file.hunks {
                writeln!(
                    formatter,
                    "@@ -{},{} +{},{} @@",
                    hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
                )?;
                for line in &hunk.body {
                    writeln!(formatter, "{}{}", line.side.prefix(), line.text)?;
                    if !line.newline {
                        writeln!(formatter, "{NO_NEWLINE}")?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Whether `lines[index]` opens a file patch: a `---` header followed by a
/// `+++` header, which a hunk's `-`/`+` body line cannot produce.
fn is_file_header(
    lines: &[&str],
    index: usize,
) -> bool {
    lines[index].starts_with("--- ")
        && lines
            .get(index + 1)
            .is_some_and(|line| line.starts_with("+++ "))
}

/// Whether `line` opens a hunk.
fn is_hunk_header(line: &str) -> bool {
    line.starts_with("@@ -")
}

/// The path a `---`/`+++` header names, without the timestamp GNU diffs append.
fn header_path(header: &str) -> &str {
    header
        .split_once(' ')
        .map_or(header, |(_, rest)| rest)
        .split('\t')
        .next()
        .unwrap_or(header)
        .trim()
}

/// Strips the conventional `a/`/`b/` prefix a diff writes to keep the two paths
/// distinguishable without naming a directory.
fn stripped_path(path: &str) -> String {
    ["a/", "b/"]
        .iter()
        .find_map(|prefix| path.strip_prefix(prefix))
        .unwrap_or(path)
        .to_string()
}

/// The header form of `path`: `a/`/`b/` prefixed, or `/dev/null` unchanged.
fn header_name(
    path: &str,
    side: char,
) -> String {
    if path == DEV_NULL {
        String::from(DEV_NULL)
    } else {
        format!("{side}/{path}")
    }
}

/// Parses every hunk of one file patch from `index`, returning the index of the
/// first line that is not part of it.
fn parse_hunks(
    lines: &[&str],
    mut index: usize,
) -> Result<(Vec<Hunk>, usize), PatchError> {
    let mut hunks = Vec::new();
    while index < lines.len() {
        if is_file_header(lines, index) {
            break;
        }
        if !is_hunk_header(lines[index]) {
            return Err(PatchError::Syntax {
                line: index + 1,
                text: lines[index].to_string(),
            });
        }
        let (hunk, next) = parse_hunk(lines, index)?;
        hunks.push(hunk);
        index = next;
    }
    Ok((hunks, index))
}

/// Parses one hunk and its body, recounting the header's counts from the body.
fn parse_hunk(
    lines: &[&str],
    start: usize,
) -> Result<(Hunk, usize), PatchError> {
    let header = lines[start];
    let malformed = || PatchError::Header {
        line: start + 1,
        text: header.to_string(),
    };
    let rest = header.strip_prefix("@@ ").ok_or_else(malformed)?;
    let (ranges, _context) = rest.split_once(" @@").ok_or_else(malformed)?;
    let (old, new) = ranges.split_once(' ').ok_or_else(malformed)?;
    if new.contains(' ') {
        return Err(malformed());
    }
    let old = start_of(old, '-', malformed)?;
    let new = start_of(new, '+', malformed)?;

    let mut body: Vec<BodyLine> = Vec::new();
    let mut index = start + 1;
    while let Some(line) = lines.get(index).copied() {
        if is_hunk_header(line) || is_file_header(lines, index) {
            break;
        }
        let (side, text) = match line.chars().next() {
            Some(' ') => (Side::Context, &line[1..]),
            Some('+') => (Side::New, &line[1..]),
            Some('-') => (Side::Old, &line[1..]),
            // An empty line is an empty context line with its leading space
            // dropped, which a hand-written diff does and `git apply` accepts
            // — unless it separates this hunk from the next header, where it is
            // a separator rather than a line of the file.
            None if !lines
                .get(index + 1)
                .is_some_and(|next| is_hunk_header(next) || next.starts_with("--- ")) =>
            {
                (Side::Context, "")
            },
            Some('\\') => {
                let Some(last) = body.last_mut() else {
                    return Err(PatchError::DanglingMarker { line: index + 1 });
                };
                last.newline = false;
                index += 1;
                continue;
            },
            _ => break,
        };
        body.push(BodyLine {
            side,
            text: text.to_string(),
            newline: true,
        });
        index += 1;
    }
    let hunk = Hunk {
        old_start: old,
        old_count: body.iter().filter(|line| line.side != Side::New).count(),
        new_start: new,
        new_count: body.iter().filter(|line| line.side != Side::Old).count(),
        body,
    };
    Ok((hunk, index))
}

/// The line number a `-l,s` or `+l,s` range starts at, checking its side and
/// count.
fn start_of(
    range: &str,
    sign: char,
    malformed: impl Fn() -> PatchError,
) -> Result<usize, PatchError> {
    let body = range.strip_prefix(sign).ok_or_else(&malformed)?;
    let (start, count) = body.split_once(',').unwrap_or((body, "1"));
    count.parse::<usize>().map_err(|_| malformed())?;
    start.parse().map_err(|_| malformed())
}

/// Splits `content` into lines, recording which ones end in a newline.
fn split_lines(content: &str) -> Vec<Line> {
    content
        .split_inclusive('\n')
        .map(|chunk| {
            chunk.strip_suffix('\n').map_or_else(
                || Line {
                    text: chunk.to_string(),
                    newline: false,
                },
                |text| Line {
                    text: text.to_string(),
                    newline: true,
                },
            )
        })
        .collect()
}

/// Writes `lines` back into content, newline for newline.
fn render_lines(lines: &[Line]) -> String {
    let mut content = String::new();
    for line in lines {
        content.push_str(&line.text);
        if line.newline {
            content.push('\n');
        }
    }
    content
}

/// Whether the file's lines are exactly the lines a hunk expects.
fn matches(
    file: &[Line],
    hunk: &[&BodyLine],
) -> bool {
    file.len() == hunk.len()
        && file
            .iter()
            .zip(hunk)
            .all(|(line, body)| line.text == body.text && line.newline == body.newline)
}

/// How many lines a mismatch message quotes.
const PREVIEW_LINES: usize = 3;

/// How long a mismatch message's quote may grow.
const PREVIEW_BYTES: usize = 160;

/// A short preview of some lines, for a mismatch message.
///
/// It is bounded: the text reaches the model's context and the durable log, so
/// quoting a whole file's tail on every rejected patch is both expensive and a
/// disclosure the tool has no reason to make.
fn preview<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    let mut text = String::new();
    for line in lines.take(PREVIEW_LINES) {
        if !text.is_empty() {
            text.push_str("\\n");
        }
        let _ = write!(text, "{line}");
    }
    if text.is_empty() {
        return String::from("nothing");
    }
    if text.len() > PREVIEW_BYTES {
        let mut end = PREVIEW_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{Applied, NO_NEWLINE, Patch, PatchError};
    use std::collections::BTreeMap;

    /// Reads `files` as a patch's reader does, failing on an unknown path.
    fn reader(
        files: &BTreeMap<String, String>
    ) -> impl FnMut(&str) -> std::io::Result<String> + '_ {
        move |path: &str| {
            files
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::other(format!("no such file: {path}")))
        }
    }

    /// Applies `patch` to `content` and returns the whole result.
    fn apply_to_patch(
        patch: &Patch,
        name: &str,
        content: &str,
    ) -> Vec<Applied> {
        let files = BTreeMap::from([(name.to_string(), content.to_string())]);
        patch.apply(reader(&files)).expect("the patch should apply")
    }

    /// Applies `patch` to `content` and returns the content it produces.
    fn apply_to(
        patch: &Patch,
        name: &str,
        content: &str,
    ) -> String {
        let applied = apply_to_patch(patch, name, content);
        assert_eq!(applied.len(), 1);
        applied[0]
            .content
            .clone()
            .expect("an edit produces content")
    }

    #[test]
    fn a_multi_hunk_patch_applies_and_its_inverse_restores_the_original() {
        let original = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
        let diff = "\
--- a/notes.txt
+++ b/notes.txt
@@ -1,3 +1,4 @@
 one
+one and a half
 two
 three
@@ -6,3 +7,3 @@
 six
-seven
+SEVEN
 eight
";
        let patch = Patch::parse(diff).expect("the diff should parse");
        let applied = apply_to(&patch, "notes.txt", original);
        assert_eq!(
            applied,
            "one\none and a half\ntwo\nthree\nfour\nfive\nsix\nSEVEN\neight\n"
        );

        // The inverse is what the log carries, so applying it to the applied
        // content must restore the original bytes.
        let inverse = patch.inverse();
        let restored = apply_to(&inverse, "notes.txt", &applied);
        assert_eq!(restored, original);
        assert_eq!(
            inverse.to_string(),
            "\
--- a/notes.txt
+++ b/notes.txt
@@ -1,4 +1,3 @@
 one
-one and a half
 two
 three
@@ -7,3 +6,3 @@
 six
+seven
-SEVEN
 eight
"
        );
    }

    #[test]
    fn a_patch_around_a_line_without_a_newline_round_trips_exactly() {
        let original = "keep\ntail without newline";
        let diff = "\
--- a/file.txt\t2026-01-01 00:00:00.000000000 +0000
+++ b/file.txt\t2026-01-01 00:00:00.000000000 +0000
@@ -1,2 +1,2 @@
 keep
-tail without newline
\\ No newline at end of file
+tail without newline either
\\ No newline at end of file
";
        let patch = Patch::parse(diff).expect("the diff should parse");
        let applied = apply_to(&patch, "file.txt", original);
        assert_eq!(applied, "keep\ntail without newline either");
        assert_eq!(apply_to(&patch.inverse(), "file.txt", &applied), original);
        assert!(patch.to_string().contains(NO_NEWLINE));
    }

    #[test]
    fn a_hunk_header_whose_counts_disagree_with_its_body_is_repaired() {
        // The header claims three old and three new lines while the body holds
        // two of each, which is what a hand-written diff looks like.
        let diff = "\
--- a/file.txt
+++ b/file.txt
@@ -1,3 +1,3 @@
 alpha
-beta
+BETA
";
        let patch = Patch::parse(diff).expect("the diff should parse");
        assert!(patch.to_string().contains("@@ -1,2 +1,2 @@"));
        assert_eq!(
            apply_to(&patch, "file.txt", "alpha\nbeta\n"),
            "alpha\nBETA\n"
        );
    }

    #[test]
    fn a_hunk_with_a_short_header_range_applies_where_it_says() {
        // `-1 +1` omits both counts, which mean one line each.
        let diff = "\
--- a/file.txt
+++ b/file.txt
@@ -2 +2 @@
-gone
+here
";
        let patch = Patch::parse(diff).expect("the diff should parse");
        assert!(patch.to_string().contains("@@ -2,1 +2,1 @@"));
        assert_eq!(
            apply_to(&patch, "file.txt", "first\ngone\n"),
            "first\nhere\n"
        );
    }

    #[test]
    fn a_hunk_that_does_not_match_is_rejected_with_nothing_applied() {
        let diff = "\
--- a/file.txt
+++ b/file.txt
@@ -1,2 +1,2 @@
 alpha
-gamma
+GAMMA
";
        let patch = Patch::parse(diff).expect("the diff should parse");
        let error = patch
            .apply(reader(&BTreeMap::from([(
                String::from("file.txt"),
                String::from("alpha\nbeta\n"),
            )])))
            .expect_err("the hunk does not match");
        let PatchError::Mismatch {
            path,
            hunk,
            expected,
            found,
        } = error
        else {
            panic!("a mismatch was expected");
        };
        assert_eq!(path, "file.txt");
        assert_eq!(hunk, 1);
        assert_eq!(expected, "alpha\\ngamma");
        assert_eq!(found, "alpha\\nbeta");
    }

    #[test]
    fn no_file_applies_when_a_later_hunk_does_not_match() {
        let diff = "\
--- a/first.txt
+++ b/first.txt
@@ -1,1 +1,1 @@
-alpha
+ALPHA
--- a/second.txt
+++ b/second.txt
@@ -1,1 +1,1 @@
-gamma
+GAMMA
";
        let applied = Patch::parse(diff)
            .expect("the diff should parse")
            .apply(reader(&BTreeMap::from([
                (String::from("first.txt"), String::from("alpha\n")),
                (String::from("second.txt"), String::from("beta\n")),
            ])));
        assert!(applied.is_err(), "the second file does not match");
    }

    #[test]
    fn a_creation_patch_writes_the_whole_file_and_its_inverse_deletes_it() {
        let diff = "\
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
";
        let patch = Patch::parse(diff).expect("the diff should parse");
        // The reader is never called: there is no original to read.
        let applied = patch
            .apply(|path| Err(std::io::Error::other(format!("unexpected read of {path}"))))
            .expect("a creation needs no original");
        assert_eq!(applied[0].path, "new.txt");
        assert_eq!(applied[0].content.as_deref(), Some("hello\nworld\n"));
        assert_eq!(applied[0].added, 2);
        assert_eq!(applied[0].removed, 0);

        // The inverse is the deletion of the file it created, and applying it to
        // the created content removes the file rather than editing it.
        let inverse = patch.inverse();
        assert!(inverse.to_string().contains("+++ /dev/null"));
        let undo = inverse
            .apply(reader(&BTreeMap::from([(
                String::from("new.txt"),
                String::from("hello\nworld\n"),
            )])))
            .expect("the inverse should apply");
        assert_eq!(undo[0].path, "new.txt");
        assert_eq!(undo[0].content, None);
    }

    #[test]
    fn a_deletion_patch_removes_the_file_and_its_inverse_creates_it() {
        let diff = "\
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-goodbye
-world
";
        let patch = Patch::parse(diff).expect("the diff should parse");
        let applied = apply_to_patch(&patch, "gone.txt", "goodbye\nworld\n");
        assert_eq!(applied[0].content, None);
        assert_eq!(applied[0].removed, 2);

        let inverse = patch.inverse();
        assert!(inverse.to_string().contains("--- /dev/null"));
        let restored = inverse
            .apply(|path| Err(std::io::Error::other(format!("unexpected read of {path}"))))
            .expect("a creation needs no original");
        assert_eq!(restored[0].content.as_deref(), Some("goodbye\nworld\n"));
    }

    #[test]
    fn a_deletion_that_leaves_lines_behind_is_rejected() {
        let diff = "\
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-goodbye
";
        let error = Patch::parse(diff)
            .expect("the diff should parse")
            .apply(reader(&BTreeMap::from([(
                String::from("gone.txt"),
                String::from("goodbye\nworld\n"),
            )])));
        assert!(matches!(
            error,
            Err(PatchError::DeleteIncomplete { path, lines: 1 }) if path == "gone.txt"
        ));
    }

    #[test]
    fn a_file_named_by_two_sections_is_rejected() {
        let diff = "\
--- a/file.txt
+++ b/file.txt
@@ -1,1 +1,1 @@
-alpha
+ALPHA
--- a/file.txt
+++ b/file.txt
@@ -2,1 +2,1 @@
-beta
+BETA
";
        assert!(matches!(
            Patch::parse(diff),
            Err(PatchError::Duplicate(path)) if path == "file.txt"
        ));
    }

    #[test]
    fn a_deletion_or_rename_is_reported_rather_than_guessed_at() {
        let renamed = "\
--- a/old.txt
+++ b/new.txt
@@ -1,1 +1,1 @@
-old
+new
";
        assert!(matches!(
            Patch::parse(renamed),
            Err(PatchError::RenameUnsupported { old, new }) if old == "old.txt" && new == "new.txt"
        ));
    }

    #[test]
    fn text_that_is_not_a_diff_is_rejected_with_its_line() {
        assert!(matches!(Patch::parse(""), Err(PatchError::Empty)));
        assert!(matches!(
            Patch::parse("here is the patch:\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n"),
            Err(PatchError::Syntax { line: 1, .. })
        ));
        assert!(matches!(
            Patch::parse("--- a/x\n+++ b/x\n@@ -1 +1 @@\nnot a body line\n"),
            Err(PatchError::Syntax { line: 4, .. })
        ));
        assert!(matches!(
            Patch::parse("--- a/x\n+++ b/x\n@@ -1,1 1,1 @@\n-a\n+b\n"),
            Err(PatchError::Header { line: 3, .. })
        ));
    }

    #[test]
    fn a_render_parses_back_to_the_same_patch() {
        let diff = "\
--- a/one.txt
+++ b/one.txt
@@ -1,2 +1,2 @@
 keep
-tail
+head
--- a/two.txt
+++ b/two.txt
@@ -3,0 +4,1 @@
+inserted
";
        let patch = Patch::parse(diff).expect("the diff should parse");
        let rendered = patch.to_string();
        assert_eq!(
            Patch::parse(&rendered).expect("the render should parse"),
            patch
        );
    }

    #[test]
    fn an_empty_context_line_may_be_written_as_an_empty_line() {
        let diff = "\
--- a/file.txt
+++ b/file.txt
@@ -1,3 +1,3 @@
 alpha

-beta
+BETA
";
        let patch = Patch::parse(diff).expect("the diff should parse");
        assert_eq!(
            apply_to(&patch, "file.txt", "alpha\n\nbeta\n"),
            "alpha\n\nBETA\n"
        );
    }

    #[test]
    fn an_overlapping_hunk_is_rejected() {
        let diff = "\
--- a/file.txt
+++ b/file.txt
@@ -1,2 +1,2 @@
 alpha
-beta
+BETA
@@ -2,2 +2,2 @@
 beta
-gamma
+GAMMA
";
        let error = Patch::parse(diff)
            .expect("the diff should parse")
            .apply(reader(&BTreeMap::from([(
                String::from("file.txt"),
                String::from("alpha\nbeta\ngamma\n"),
            )])));
        assert!(matches!(error, Err(PatchError::Overlap { hunk: 2, .. })));
    }

    #[test]
    fn a_hunk_past_the_end_of_the_file_is_rejected_rather_than_slicing_past_it() {
        // An insertion at a line far past the file cannot match, and reporting
        // that must not be a panic.
        let diff = "\
--- a/file.txt
+++ b/file.txt
@@ -99,0 +100,1 @@
+inserted
";
        let error = Patch::parse(diff)
            .expect("the diff should parse")
            .apply(reader(&BTreeMap::from([(
                String::from("file.txt"),
                String::from("alpha\nbeta\n"),
            )])));
        assert!(matches!(error, Err(PatchError::Mismatch { hunk: 1, .. })));
    }
}
