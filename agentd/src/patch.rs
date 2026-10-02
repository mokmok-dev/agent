//! The in-process unified-diff applier behind the `patch` action.
//!
//! [`build`] parses the diff with `diffy`, resolves every path through
//! `coding`'s guard, reads each base under a byte cap, and applies every hunk,
//! holding the whole result as a [`Plan`] before anything is written.
//! [`Plan::commit`] then writes it. No refusal can follow a write, and no write
//! can follow a refusal.
//!
//! All-or-nothing holds in the strong sense only until `commit` starts: a
//! `commit` that fails part-way (an OS error on the third file) can leave the
//! files before it written, exactly as `git apply` can. It is reported, not
//! rolled back.
//!
//! The unified diff stays the wire format the model speaks, and the applier is in
//! process, so a session's workspace needs no `git` and no repository. `diffy`
//! owns the grammar and the application; this module owns the path policy, the
//! caps, and the reasons the model is told.

use std::fmt::Write as _;
use std::io::Read;
use std::iter::Peekable;
use std::path::{Path, PathBuf};

use diffy::Patch;
use diffy::patch_set::{FileOperation, ParseOptions, PatchSet};

use crate::coding::{Coding, resolve_for_write};

/// The most diff text this tool looks at, before `diffy` parses it.
const MAX_DIFF_BYTES: usize = 262_144;
/// The most files one patch may name, bounding the plan length and the writes.
const MAX_PATCH_FILES: usize = 64;
/// The most bytes one base file may contribute to a patch.
const MAX_PATCH_BASE_BYTES: usize = 1_048_576;
/// The most bytes all final texts of one patch may hold at once.
const MAX_PATCH_PLAN_BYTES: usize = 8_388_608;

/// The reason a diff this tool cannot apply is refused.
const NOT_TEXT_PATCH: &str = "the diff is not a text patch this tool applies";
/// The reason a diff that does not parse is refused.
const NOT_UNIFIED: &str = "the diff is not a unified diff";
/// The reason a `--- ` header with no `+++ ` line is refused.
const FILE_HEADER_INCOMPLETE: &str = "a file header is missing its `+++` line";
/// The reason a hunk body line that no header precedes is refused.
const BODY_WITHOUT_HEADER: &str = "a hunk line has no header";

/// What a patch will do, once every path resolved and every file applied.
///
/// Only a complete plan can be committed, so nothing is written before the whole
/// diff is known to apply.
#[derive(Debug)]
pub struct Plan {
    /// The file changes, in the order the diff names them.
    files: Vec<FileChange>,
}

/// One file's resolved change.
#[derive(Debug)]
enum FileChange {
    /// Create `target` with `content`; `target` must not have existed.
    Create {
        /// The workspace-relative path the model is told about.
        path: String,
        /// The resolved absolute path to write.
        target: PathBuf,
        /// The final text.
        content: String,
    },
    /// Replace `target` with `content`.
    Modify {
        /// The workspace-relative path the model is told about.
        path: String,
        /// The resolved absolute path to write.
        target: PathBuf,
        /// The final text.
        content: String,
    },
    /// Remove `target`.
    Delete {
        /// The workspace-relative path the model is told about.
        path: String,
        /// The resolved absolute path to remove.
        target: PathBuf,
    },
}

/// A terse, model-facing refusal. `Display` is the reason alone.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct Refused {
    /// The terse reason, which is what `Display` yields.
    reason: String,
    /// `diffy`'s own text, when it adds evidence to the reason.
    error: Option<String>,
}

impl Refused {
    /// A refusal whose reason stands alone.
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            error: None,
        }
    }

    /// A refusal carrying the library text that produced it.
    fn with_error(
        reason: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            reason: reason.into(),
            error: Some(error.into()),
        }
    }

    /// `diffy`'s own text, when there is any to report.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// One hunk header, parsed from `@@ -<start>[,<count>] +<start>[,<count>] @@<rest>`.
#[derive(Debug)]
struct HunkHeader<'a> {
    /// The old start number, as written.
    old_start: &'a str,
    /// The new start number, as written.
    new_start: &'a str,
    /// Everything from the second `@@` on, verbatim.
    after: &'a str,
}

impl<'a> HunkHeader<'a> {
    /// Parse `line` as a hunk header, or `None` when it is not that shape.
    fn parse(line: &'a str) -> Option<Self> {
        let rest = line.strip_prefix("@@ ")?;
        let (old_start, rest) = parse_side(rest, '-')?;
        let rest = rest.strip_prefix(' ')?;
        let (new_start, rest) = parse_side(rest, '+')?;
        if !rest.starts_with(" @@") {
            return None;
        }
        Some(Self {
            old_start,
            new_start,
            after: rest,
        })
    }

    /// Render this header with the counts taken from the body.
    fn render(
        &self,
        old: usize,
        new: usize,
    ) -> String {
        let mut out = String::with_capacity(self.old_start.len() + self.new_start.len() + 16);
        out.push_str("@@ -");
        out.push_str(self.old_start);
        push_count(&mut out, old);
        out.push_str(" +");
        out.push_str(self.new_start);
        push_count(&mut out, new);
        out.push_str(self.after);
        out
    }
}

/// Parse one `-<start>[,<count>]` or `+<start>[,<count>]` side.
///
/// Returns the start as written and the rest. The count is validated but
/// discarded: the rewrite derives it from the body.
fn parse_side(
    input: &str,
    sign: char,
) -> Option<(&str, &str)> {
    let input = input.strip_prefix(sign)?;
    let end = input.find(|byte: char| !byte.is_ascii_digit())?;
    if end == 0 {
        return None;
    }
    let (start, rest) = input.split_at(end);
    let rest = match rest.strip_prefix(',') {
        Some(digits) => {
            let end = digits
                .find(|byte: char| !byte.is_ascii_digit())
                .unwrap_or(digits.len());
            if end == 0 {
                return None;
            }
            &digits[end..]
        },
        None => rest,
    };
    Some((start, rest))
}

/// Write a hunk count: `,<count>` for zero or more than one, nothing for one.
fn push_count(
    out: &mut String,
    count: usize,
) {
    if count != 1 {
        let _ = write!(out, ",{count}");
    }
}

/// Whether `line` begins a hunk body line.
const fn is_body_line(line: &str) -> bool {
    matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-' | b'\\'))
}

/// Whether the next two lines at `rest` are a `--- `/`+++ ` header pair, which is
/// where one section ends and the next begins.
fn at_section_boundary<'a, I>(rest: &Peekable<I>) -> bool
where
    I: Iterator<Item = &'a str> + Clone,
{
    let mut look = rest.clone();
    let current = look.peek().copied();
    let _ = look.next();
    let next = look.peek().copied();
    current.is_some_and(|line| line.starts_with("--- "))
        && next.is_some_and(|line| line.starts_with("+++ "))
}

/// Add one body line's contribution to each side.
///
/// A context line counts for both sides, a removal for the old side, an addition
/// for the new side, and a `\ No newline at end of file` marker for neither.
const fn count_line(
    line: &str,
    old: &mut usize,
    new: &mut usize,
) {
    match line.as_bytes().first() {
        Some(b' ') => {
            *old += 1;
            *new += 1;
        },
        Some(b'-') => *old += 1,
        Some(b'+') => *new += 1,
        _ => {},
    }
}

/// Count and copy the body that follows a hunk header.
///
/// The body is the contiguous run of body lines, so a count the header wrote
/// never bounds it and a miscount is repaired from the whole body. A trailing
/// `\ No newline at end of file` marker counts for neither side.
fn scan_body<'a, I>(
    rest: &mut Peekable<I>,
    out: &mut String,
) -> (usize, usize)
where
    I: Iterator<Item = &'a str> + Clone,
{
    let mut old = 0;
    let mut new = 0;
    while let Some(line) = rest.peek().copied() {
        if !is_body_line(line) || at_section_boundary(rest) {
            break;
        }
        let _ = rest.next();
        count_line(line, &mut old, &mut new);
        out.push_str(line);
    }
    (old, new)
}

/// Rewrite every hunk header's counts from its body.
///
/// Every byte that is not a hunk header is copied through untouched, so the
/// diff's content cannot change. A header that does not parse is copied through
/// and left to `diffy`, which refuses it with its own text.
///
/// # Errors
///
/// Returns a [`Refused`] when a `--- ` header has no `+++ ` line after it, or a
/// body line appears where no hunk header precedes it.
fn recount(source: &str) -> Result<String, Refused> {
    let mut out = String::with_capacity(source.len());
    let mut rest = source.split_inclusive('\n').peekable();
    while let Some(line) = rest.next() {
        if line.starts_with("@@") {
            let mut body = String::new();
            match HunkHeader::parse(line) {
                Some(header) => {
                    let (old, new) = scan_body(&mut rest, &mut body);
                    out.push_str(&header.render(old, new));
                },
                // A header this cannot read is copied with its body untouched, so
                // `diffy` reports it with its own byte offset.
                None => out.push_str(line),
            }
            out.push_str(&body);
        } else if line.starts_with("--- ") {
            if !rest.peek().is_some_and(|next| next.starts_with("+++ ")) {
                return Err(Refused::new(FILE_HEADER_INCOMPLETE));
            }
            out.push_str(line);
            if let Some(next) = rest.next() {
                out.push_str(next);
            }
        } else if is_body_line(line) {
            return Err(Refused::new(BODY_WITHOUT_HEADER));
        } else {
            out.push_str(line);
        }
    }
    Ok(out)
}

/// Build the plan for `source`, writing nothing.
///
/// # Errors
///
/// Returns a [`Refused`] when the diff is empty, oversized, not text, not a
/// unified text patch, names too many or no files, holds a path the guard
/// refuses, a base that cannot be read under its cap, or a hunk that does not
/// apply. It also refuses a skeleton the recount cannot account for: a `--- `
/// header with no `+++ ` line, or a body line with no header before it.
pub fn build(
    coding: &Coding,
    source: &str,
) -> Result<Plan, Refused> {
    if source.is_empty() {
        return Err(Refused::new("the diff is empty"));
    }
    if source.len() > MAX_DIFF_BYTES {
        return Err(Refused::new("the diff is larger than the limit"));
    }
    if source.as_bytes().contains(&0) {
        return Err(Refused::new("the diff is not text"));
    }
    // A binary patch is named before `diffy` sees it, so the model is told what
    // this tool cannot apply rather than what a parser thought of it.
    if source
        .lines()
        .any(|line| line.starts_with("GIT binary patch") || line.starts_with("Binary files "))
    {
        return Err(Refused::new(NOT_TEXT_PATCH));
    }

    // The counts are derived from the body before `diffy` reads them, because
    // `diffy` treats a declared count as the hunk's extent and tolerates the
    // rest of the body as junk. The recount also checks the skeleton.
    let recounted = recount(source)?;
    let mut files: Vec<FileChange> = Vec::new();
    let mut plan_bytes: usize = 0;
    for parsed in PatchSet::parse(&recounted, ParseOptions::unidiff()) {
        let file = parsed.map_err(|error| Refused::with_error(NOT_UNIFIED, error.to_string()))?;
        if files.len() >= MAX_PATCH_FILES {
            return Err(Refused::new("the diff names too many files"));
        }
        let patch = file
            .patch()
            .as_text()
            .ok_or_else(|| Refused::new(NOT_TEXT_PATCH))?;
        files.push(change_for(
            coding,
            file.operation(),
            patch,
            &mut plan_bytes,
        )?);
    }
    // A dependency's behavior is not this module's invariant: `diffy` refuses a diff
    // with no file section today, and the refusal stays ours if that changes.
    if files.is_empty() {
        return Err(Refused::new("the diff names no files"));
    }
    Ok(Plan { files })
}

/// Resolve one file's operation into a [`FileChange`], applying its patch.
fn change_for(
    coding: &Coding,
    operation: &FileOperation<'_, str>,
    file_patch: &Patch<'_, str>,
    plan_bytes: &mut usize,
) -> Result<FileChange, Refused> {
    match operation {
        FileOperation::Create(path) => {
            let path = strip_side(path, "b/")?;
            let target = target_for(coding, &path)?;
            if target.exists() {
                return Err(Refused::new(format!("`{path}` already exists")));
            }
            let content = apply_text(file_patch, "", &path)?;
            account(plan_bytes, content.len())?;
            Ok(FileChange::Create {
                path,
                target,
                content,
            })
        },
        FileOperation::Delete(path) => {
            let path = strip_side(path, "a/")?;
            let target = target_for(coding, &path)?;
            let base = read_base(&target, &path)?;
            let applied = apply_text(file_patch, &base, &path)?;
            if !applied.is_empty() {
                return Err(Refused::new(format!("a hunk does not apply to `{path}`")));
            }
            Ok(FileChange::Delete { path, target })
        },
        FileOperation::Modify { original, modified } => {
            let old = strip_side(original, "a/")?;
            let new = strip_side(modified, "b/")?;
            if old != new {
                return Err(Refused::new("the diff renames a path"));
            }
            let target = target_for(coding, &new)?;
            let base = read_base(&target, &new)?;
            let content = apply_text(file_patch, &base, &new)?;
            account(plan_bytes, content.len())?;
            Ok(FileChange::Modify {
                path: new,
                target,
                content,
            })
        },
        // A rename or a copy is a text change this tool does not apply; `diffy`
        // only produces them when the caller asks for git's extended format, which
        // this module does not.
        FileOperation::Rename { .. } | FileOperation::Copy { .. } => {
            Err(Refused::new(NOT_TEXT_PATCH))
        },
    }
}

/// Drop **one** leading `prefix` from a header path, or use it as written, and
/// refuse an empty result.
fn strip_side(
    path: &str,
    prefix: &str,
) -> Result<String, Refused> {
    let path = path.strip_prefix(prefix).unwrap_or(path);
    if path.is_empty() {
        return Err(Refused::new("a file header names an empty path"));
    }
    Ok(path.to_owned())
}

/// Resolve a header path for writing through the workspace guard.
fn target_for(
    coding: &Coding,
    path: &str,
) -> Result<PathBuf, Refused> {
    resolve_for_write(coding, path).map_err(|error| Refused::new(error.reason()))
}

/// Read a base file under [`MAX_PATCH_BASE_BYTES`], bounded while it is read.
fn read_base(
    target: &Path,
    path: &str,
) -> Result<String, Refused> {
    // A base this tool cannot use as text is "no file at" whatever went wrong,
    // except a size or a text problem: the model's next move is the same either
    // way, and one reason is one thing to test.
    let file =
        std::fs::File::open(target).map_err(|_| Refused::new(format!("no file at `{path}`")))?;
    // The read is bounded while it is read, so a file that grew after it was
    // resolved cannot outgrow the cap, and the window is one byte wider than the
    // cap so a base of exactly the cap is not mistaken for one over it.
    let mut bytes = Vec::new();
    file.take(MAX_PATCH_BASE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Refused::new(format!("no file at `{path}`")))?;
    if bytes.len() > MAX_PATCH_BASE_BYTES {
        return Err(Refused::new(format!("`{path}` is too large to patch")));
    }
    if bytes.contains(&0) {
        return Err(Refused::new(format!("`{path}` is not text")));
    }
    String::from_utf8(bytes).map_err(|_| Refused::new(format!("`{path}` is not text")))
}

/// Apply one file's text patch to `base`.
fn apply_text(
    file_patch: &Patch<'_, str>,
    base: &str,
    path: &str,
) -> Result<String, Refused> {
    diffy::apply(base, file_patch).map_err(|error| {
        Refused::with_error(
            format!("a hunk does not apply to `{path}`"),
            error.to_string(),
        )
    })
}

/// Add one final text's length to the running plan total, refusing past the cap.
fn account(
    total: &mut usize,
    added: usize,
) -> Result<(), Refused> {
    let next = total.saturating_add(added);
    if next > MAX_PATCH_PLAN_BYTES {
        return Err(Refused::new("the patch result is larger than the limit"));
    }
    *total = next;
    Ok(())
}

impl Plan {
    /// The workspace-relative paths the patch will touch, in diff order.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        self.files
            .iter()
            .map(|file| match file {
                FileChange::Create { path, .. }
                | FileChange::Modify { path, .. }
                | FileChange::Delete { path, .. } => path.clone(),
            })
            .collect()
    }

    /// Write the plan, returning how many files it changed.
    ///
    /// # Errors
    ///
    /// Returns a [`Refused`] when a parent directory cannot be created, a file
    /// cannot be written, or a file cannot be removed. A failure part-way leaves
    /// the files before it written, which the caller reports rather than undoes.
    pub fn commit(self) -> Result<usize, Refused> {
        for file in &self.files {
            match file {
                FileChange::Create {
                    path,
                    target,
                    content,
                }
                | FileChange::Modify {
                    path,
                    target,
                    content,
                } => {
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|_| Refused::new(format!("could not write `{path}`")))?;
                    }
                    std::fs::write(target, content)
                        .map_err(|_| Refused::new(format!("could not write `{path}`")))?;
                },
                FileChange::Delete { path, target } => {
                    std::fs::remove_file(target)
                        .map_err(|_| Refused::new(format!("could not remove `{path}`")))?;
                },
            }
        }
        Ok(self.files.len())
    }
}

#[cfg(test)]
mod tests {
    // Tests for the in-process applier over a real workspace. The module is
    // private, so these are unit tests: every cap, refusal, and applied result is
    // pinned here, and no test needs `git` or a repository.

    use std::borrow::Cow;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::coding::Mode;

    /// A temp workspace, removed on drop.
    struct Workspace(PathBuf);

    impl Workspace {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("agent-patch-{tag}-{}-{unique}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("creates the workspace");
            Self(path)
        }

        /// Write `contents` to `rel`, creating its parents.
        fn write(
            &self,
            rel: &str,
            contents: &str,
        ) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("creates the parents");
            }
            std::fs::write(&path, contents).expect("writes the file");
        }

        /// Write raw `bytes` to `rel`.
        fn write_bytes(
            &self,
            rel: &str,
            bytes: &[u8],
        ) {
            std::fs::write(self.0.join(rel), bytes).expect("writes the file");
        }

        /// Read `rel` back.
        fn read(
            &self,
            rel: &str,
        ) -> String {
            std::fs::read_to_string(self.0.join(rel)).expect("the file is readable")
        }

        /// The host path of `rel`.
        fn path(
            &self,
            rel: &str,
        ) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A read-write belt over `workspace`.
    fn belt(workspace: &Workspace) -> Coding {
        Coding::new(&workspace.0, Mode::ReadWrite).expect("the workspace opens")
    }

    /// A git-style modify of `path`, from `old` to `new`.
    fn modify(path: &str) -> String {
        format!("--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-old\n+new\n")
    }

    /// Build `diff` with `coding`, expecting this exact reason.
    fn refuses(
        coding: &Coding,
        diff: &str,
        reason: &str,
    ) {
        match build(coding, diff) {
            Ok(plan) => panic!("expected a refusal, got paths {:?}", plan.paths()),
            Err(refused) => assert_eq!(refused.to_string(), reason, "diff was:\n{diff}"),
        }
    }

    /// Write a file whose first line is `old` and whose total size is `size`.
    fn write_sized(
        workspace: &Workspace,
        rel: &str,
        size: usize,
    ) {
        let mut content = String::from("old\n");
        content.push_str(&"x".repeat(size - 4));
        workspace.write(rel, &content);
        assert_eq!(workspace.read(rel).len(), size);
    }

    /// A modify section for each of `paths`.
    fn modify_all(paths: &[String]) -> String {
        let mut diff = String::new();
        for path in paths {
            diff.push_str(&modify(path));
        }
        diff
    }

    /// A creation section for each of `paths`.
    fn create_all(paths: &[String]) -> String {
        use std::fmt::Write as _;
        paths.iter().fold(String::new(), |mut diff, path| {
            write!(diff, "--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+x\n")
                .expect("writing to a String cannot fail");
            diff
        })
    }

    #[test]
    fn the_bounds_are_pinned() {
        assert_eq!(MAX_DIFF_BYTES, 262_144);
        assert_eq!(MAX_PATCH_FILES, 64);
        assert_eq!(MAX_PATCH_BASE_BYTES, 1_048_576);
        assert_eq!(MAX_PATCH_PLAN_BYTES, 8_388_608);
    }

    #[test]
    fn a_git_style_diff_applies() {
        let workspace = Workspace::new("git-style");
        workspace.write("f.txt", "hello\n");
        let coding = belt(&workspace);
        let diff = "diff --git a/f.txt b/f.txt\nindex 111..222 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-hello\n+world\n";
        let plan = build(&coding, diff).expect("the diff applies");
        assert_eq!(plan.paths(), ["f.txt"]);
        assert_eq!(plan.commit().expect("commits"), 1);
        assert_eq!(workspace.read("f.txt"), "world\n");
    }

    #[test]
    fn a_plain_prefixed_diff_applies() {
        let workspace = Workspace::new("plain");
        workspace.write("f.txt", "hello\n");
        let coding = belt(&workspace);
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-hello\n+world\n";
        let plan = build(&coding, diff).expect("the diff applies");
        assert_eq!(plan.commit().expect("commits"), 1);
        assert_eq!(workspace.read("f.txt"), "world\n");
    }

    #[test]
    fn a_modify_applies_in_a_directory_that_is_not_a_git_repository() {
        // The applier is in process: no `git`, no repository, no environment.
        let workspace = Workspace::new("not-a-repo");
        assert!(
            !workspace.path(".git").exists(),
            "the fixture is deliberately not a repository"
        );
        workspace.write("f.txt", "old\n");
        let coding = belt(&workspace);
        assert_eq!(
            build(&coding, &modify("f.txt"))
                .expect("applies")
                .commit()
                .expect("commits"),
            1
        );
        assert_eq!(workspace.read("f.txt"), "new\n");
    }

    #[test]
    fn an_unprefixed_nested_path_applies_to_the_nested_file() {
        // A stray `f.txt` at the root must not be touched by a diff that names
        // `sub/f.txt`; the prefix strip is per path and must not eat a component.
        let workspace = Workspace::new("nested");
        workspace.write("f.txt", "old\n");
        workspace.write("sub/f.txt", "old\n");
        let coding = belt(&workspace);
        let diff = "--- sub/f.txt\n+++ sub/f.txt\n@@ -1 +1 @@\n-old\n+new\n";
        build(&coding, diff)
            .expect("applies")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("sub/f.txt"), "new\n");
        assert_eq!(workspace.read("f.txt"), "old\n");
    }

    #[test]
    fn a_creation_applies() {
        let workspace = Workspace::new("create");
        let coding = belt(&workspace);
        let diff = "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n";
        let plan = build(&coding, diff).expect("applies");
        assert_eq!(plan.paths(), ["new.txt"]);
        assert_eq!(plan.commit().expect("commits"), 1);
        assert_eq!(workspace.read("new.txt"), "hello\n");
    }

    #[test]
    fn a_creation_of_an_existing_file_is_refused() {
        let workspace = Workspace::new("create-exists");
        workspace.write("new.txt", "already here\n");
        let coding = belt(&workspace);
        let diff = "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n";
        refuses(&coding, diff, "`new.txt` already exists");
        assert_eq!(workspace.read("new.txt"), "already here\n");
    }

    #[test]
    fn a_creation_creates_parent_directories() {
        let workspace = Workspace::new("create-parents");
        let coding = belt(&workspace);
        let diff = "--- /dev/null\n+++ b/sub/dir/new.txt\n@@ -0,0 +1 @@\n+hello\n";
        build(&coding, diff)
            .expect("applies")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("sub/dir/new.txt"), "hello\n");
    }

    #[test]
    fn a_deletion_applies() {
        let workspace = Workspace::new("delete");
        workspace.write("f.txt", "hello\n");
        let coding = belt(&workspace);
        let diff = "--- a/f.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-hello\n";
        let plan = build(&coding, diff).expect("applies");
        assert_eq!(plan.paths(), ["f.txt"]);
        assert_eq!(plan.commit().expect("commits"), 1);
        assert!(!workspace.path("f.txt").exists(), "the file is gone");
    }

    #[test]
    fn a_deletion_of_a_missing_file_is_refused() {
        let workspace = Workspace::new("delete-missing");
        let coding = belt(&workspace);
        let diff = "--- a/f.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-hello\n";
        refuses(&coding, diff, "no file at `f.txt`");
    }

    #[test]
    fn a_multi_file_diff_applies_every_file() {
        let workspace = Workspace::new("multi");
        workspace.write("a.txt", "old\n");
        workspace.write("b.txt", "old\n");
        let coding = belt(&workspace);
        let diff = format!("{}{}", modify("a.txt"), modify("b.txt"));
        let plan = build(&coding, &diff).expect("applies");
        assert_eq!(plan.paths(), ["a.txt", "b.txt"]);
        assert_eq!(plan.commit().expect("commits"), 2);
        assert_eq!(workspace.read("a.txt"), "new\n");
        assert_eq!(workspace.read("b.txt"), "new\n");
    }

    #[test]
    fn a_hunk_that_does_not_apply_is_refused_and_leaves_the_other_file_alone() {
        // The plan is built in full before any write, so a refusal on the second
        // file leaves the first untouched.
        let workspace = Workspace::new("mismatch");
        workspace.write("a.txt", "AAA\n");
        workspace.write("b.txt", "BBB\n");
        let coding = belt(&workspace);
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-AAA\n+new-a\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-not-BBB\n+new-b\n";
        let refused = build(&coding, diff).expect_err("refused");
        assert_eq!(refused.to_string(), "a hunk does not apply to `b.txt`");
        assert!(
            refused.error().is_some_and(|error| error.contains("hunk")),
            "diffy's own text is carried: {:?}",
            refused.error()
        );
        assert_eq!(
            workspace.read("a.txt"),
            "AAA\n",
            "the first file is untouched"
        );
        assert_eq!(workspace.read("b.txt"), "BBB\n");
    }

    #[test]
    fn a_hunk_whose_counts_lie_is_recounted_and_patched() {
        let workspace = Workspace::new("lying");
        workspace.write("f", "a\n");
        let coding = belt(&workspace);
        let plan = build(&coding, "--- f\n+++ f\n@@ -1,2 +1,2 @@\n a\n")
            .expect("the lying count is repaired");
        assert_eq!(plan.commit().expect("commits"), 1);
        assert_eq!(workspace.read("f"), "a\n", "the no-op patch applied");
    }

    #[test]
    fn a_count_off_by_one_is_recounted_and_applied() {
        let workspace = Workspace::new("off-by-one");
        let coding = belt(&workspace);

        // The old count is one too few; the new count is exact, so it bounds.
        workspace.write("few.txt", "one\ntwo\nthree\n");
        let few =
            "--- a/few.txt\n+++ b/few.txt\n@@ -1,2 +1,3 @@\n one\n-two\n-three\n+TWO\n+THREE\n";
        build(&coding, few)
            .expect("the too-few count applies")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("few.txt"), "one\nTWO\nTHREE\n");

        // The old count is one too many, so the body under-runs and is recounted.
        workspace.write("many.txt", "one\ntwo\nthree\n");
        let many =
            "--- a/many.txt\n+++ b/many.txt\n@@ -1,4 +1,3 @@\n one\n-two\n-three\n+TWO\n+THREE\n";
        build(&coding, many)
            .expect("the too-many count applies")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("many.txt"), "one\nTWO\nTHREE\n");
    }

    #[test]
    fn omitted_counts_in_a_four_line_body_are_recounted() {
        // `diffy` would satisfy `-1 +1` with the first line and drop the rest;
        // an omitted count carries no bound, so the whole body is counted.
        let workspace = Workspace::new("omitted-counts");
        workspace.write("f.txt", "one\ntwo\nthree\n");
        let coding = belt(&workspace);
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n one\n-two\n+TWO\n three\n";
        build(&coding, diff)
            .expect("the four-line body applies")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("f.txt"), "one\nTWO\nthree\n");
    }

    #[test]
    fn a_creation_with_a_wrong_added_count_creates_the_content() {
        let workspace = Workspace::new("create-recount");
        let coding = belt(&workspace);
        let diff = "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+one\n+two\n";
        build(&coding, diff)
            .expect("the count is recounted to two")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("new.txt"), "one\ntwo\n");
    }

    #[test]
    fn a_deletion_with_a_wrong_removed_count_removes_the_file() {
        let workspace = Workspace::new("delete-recount");
        workspace.write("f.txt", "one\ntwo\n");
        let coding = belt(&workspace);
        let diff = "--- a/f.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-one\n-two\n";
        build(&coding, diff)
            .expect("the count is recounted to two")
            .commit()
            .expect("commits");
        assert!(!workspace.path("f.txt").exists(), "the file is gone");
    }

    #[test]
    fn two_hunks_in_one_section_are_both_recounted_and_applied() {
        let workspace = Workspace::new("two-hunks");
        workspace.write("f.txt", "one\ntwo\nthree\nfour\nfive\nsix\n");
        let coding = belt(&workspace);
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,3 @@\n one\n-two\n+TWO\n three\n@@ -4,2 +4,3 @@\n four\n-five\n+FIVE\n six\n";
        build(&coding, diff)
            .expect("both hunks apply")
            .commit()
            .expect("commits");
        assert_eq!(
            workspace.read("f.txt"),
            "one\nTWO\nthree\nfour\nFIVE\nsix\n"
        );
    }

    #[test]
    fn two_sections_are_both_recounted_and_applied() {
        let workspace = Workspace::new("two-sections");
        workspace.write("a.txt", "one\ntwo\nthree\n");
        workspace.write("b.txt", "x\n");
        let coding = belt(&workspace);
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,3 @@\n one\n-two\n+TWO\n three\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-x\n+y\n";
        build(&coding, diff)
            .expect("both sections apply")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("a.txt"), "one\nTWO\nthree\n");
        assert_eq!(workspace.read("b.txt"), "y\n");
    }

    #[test]
    fn a_body_line_that_begins_with_dashes_is_not_a_section() {
        // `--- gone` is a removal of `-- gone`; the next line is a context line,
        // not the `+++ ` of a file header, so the body continues.
        let workspace = Workspace::new("dashes");
        workspace.write("f.txt", "before\n-- gone\nafter\n");
        let coding = belt(&workspace);
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,2 @@\n before\n--- gone\n after\n";
        build(&coding, diff)
            .expect("the dashed body line applies")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("f.txt"), "before\nafter\n");
    }

    #[test]
    fn a_no_newline_marker_counts_for_neither_side() {
        let workspace = Workspace::new("no-newline");
        workspace.write("f.txt", "old");
        let coding = belt(&workspace);
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n";
        build(&coding, diff)
            .expect("the markers count for neither side")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("f.txt"), "new");
    }

    #[test]
    fn a_stray_body_line_is_absorbed_into_the_hunk() {
        // The body is contiguous, so the trailing `-two` is counted with the
        // rest and the header grows to `@@ -1,4 +1,3 @@`; it is not refused.
        let workspace = Workspace::new("stray");
        workspace.write("f.txt", "one\ntwo\nthree\ntwo\n");
        let coding = belt(&workspace);
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n-two\n";
        build(&coding, diff)
            .expect("the trailing body line is absorbed")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("f.txt"), "one\nTWO\nthree\n");
    }

    #[test]
    fn a_count_too_small_on_both_sides_is_recounted_and_patched() {
        // `diffy` would refuse `@@ -1,2 +1,2 @@` over this three-line body;
        // the counts are derived from the body and the hunk applies.
        let workspace = Workspace::new("both-too-few");
        workspace.write("f.txt", "one\ntwo\nthree\n");
        let coding = belt(&workspace);
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n one\n two\n three\n";
        build(&coding, diff)
            .expect("the lying count is repaired")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("f.txt"), "one\ntwo\nthree\n");
    }

    #[test]
    fn a_blank_line_inside_a_hunk_is_refused() {
        let workspace = Workspace::new("blank-inside");
        let coding = belt(&workspace);
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1,4 +1,4 @@\n one\n-two\n\n+TWO\n three\n";
        refuses(&coding, diff, "a hunk line has no header");
    }

    #[test]
    fn a_file_header_without_its_plus_line_is_refused() {
        let workspace = Workspace::new("missing-plus");
        let coding = belt(&workspace);
        refuses(
            &coding,
            "--- a/f.txt\n@@ -1 +1 @@\n-a\n+b\n",
            "a file header is missing its `+++` line",
        );
    }

    #[test]
    fn blank_and_metadata_lines_between_sections_still_apply() {
        let workspace = Workspace::new("between-sections");
        let coding = belt(&workspace);
        let diff = "diff --git a/a.txt b/a.txt\nindex 111..222 100644\n--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1 @@\n+a\n\nindex 333..444 100644\n--- /dev/null\n+++ b/b.txt\n@@ -0,0 +1 @@\n+b\n";
        build(&coding, diff)
            .expect("the filler lines are copied through")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("a.txt"), "a\n");
        assert_eq!(workspace.read("b.txt"), "b\n");
    }

    #[test]
    fn a_section_heading_after_the_hunk_header_is_preserved() {
        let workspace = Workspace::new("heading");
        workspace.write("f.txt", "old\n");
        let coding = belt(&workspace);
        let diff = "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@ keep this heading\n-old\n+new\n";
        build(&coding, diff)
            .expect("the heading is preserved")
            .commit()
            .expect("commits");
        assert_eq!(workspace.read("f.txt"), "new\n");
    }

    #[test]
    fn the_size_cap_is_checked_before_the_skeleton() {
        // A diff that is both oversized and has a skeleton the recount would
        // refuse is refused for its size, proving the recount runs after the cap.
        let workspace = Workspace::new("size-first");
        let coding = belt(&workspace);
        let mut diff = String::from("--- a/f.txt\n--- not-a-plus-line\n");
        diff.push_str(&" ".repeat(MAX_DIFF_BYTES));
        assert!(diff.len() > MAX_DIFF_BYTES);
        refuses(&coding, &diff, "the diff is larger than the limit");
    }

    #[test]
    fn recount_rewrites_each_header_from_its_body() {
        // One crafted input holds an omitted count, a count above one, a zero
        // count, a section heading, and a `\ No newline` marker.
        let input = "--- a/f.txt\n+++ b/f.txt\n@@ -10 +20,5 @@ keep this heading\n context\n-old\n+new\n\\ No newline at end of file\n@@ -1,4 +3 @@\n-a\n-b\n-c\n-d\n";
        let expected = "--- a/f.txt\n+++ b/f.txt\n@@ -10,2 +20,2 @@ keep this heading\n context\n-old\n+new\n\\ No newline at end of file\n@@ -1,4 +3,0 @@\n-a\n-b\n-c\n-d\n";
        assert_eq!(recount(input).expect("recounts"), expected);
    }

    #[test]
    fn a_binary_diff_is_refused() {
        let workspace = Workspace::new("binary");
        let coding = belt(&workspace);
        for diff in [
            "GIT binary patch\n",
            "diff --git a/i.png b/i.png\nnew file mode 100644\nindex 0000000..7c4530c\nBinary files /dev/null and b/i.png differ\n",
        ] {
            refuses(
                &coding,
                diff,
                "the diff is not a text patch this tool applies",
            );
        }
    }

    #[test]
    fn a_modify_of_a_directory_is_refused() {
        let workspace = Workspace::new("modify-dir");
        std::fs::create_dir_all(workspace.path("sub")).expect("creates the directory");
        let coding = belt(&workspace);
        refuses(&coding, &modify("sub"), "no file at `sub`");
    }

    #[test]
    fn a_rename_and_a_copy_are_refused() {
        // `diffy` only names a rename or a copy under git's extended format, which
        // this module never asks for; the operations are still handled and refused.
        let workspace = Workspace::new("rename-copy");
        let coding = belt(&workspace);
        let patch = Patch::from_str("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n").expect("parses");
        let mut plan_bytes = 0;
        for operation in [
            FileOperation::Rename {
                from: Cow::Borrowed("x"),
                to: Cow::Borrowed("y"),
            },
            FileOperation::Copy {
                from: Cow::Borrowed("x"),
                to: Cow::Borrowed("y"),
            },
        ] {
            let refused =
                change_for(&coding, &operation, &patch, &mut plan_bytes).expect_err("refused");
            assert_eq!(refused.to_string(), NOT_TEXT_PATCH);
        }
    }

    #[test]
    fn a_diff_that_renames_a_path_is_refused() {
        let workspace = Workspace::new("renamed-path");
        workspace.write("x", "a\n");
        let coding = belt(&workspace);
        refuses(
            &coding,
            "--- a/x\n+++ b/y\n@@ -1 +1 @@\n-a\n+b\n",
            "the diff renames a path",
        );
        assert_eq!(workspace.read("x"), "a\n", "nothing moved");
    }

    #[test]
    fn a_header_that_names_an_empty_path_is_refused() {
        let workspace = Workspace::new("empty-path");
        let coding = belt(&workspace);
        refuses(
            &coding,
            "--- a/\n+++ b/\n@@ -1 +1 @@\n-a\n+b\n",
            "a file header names an empty path",
        );
    }

    #[test]
    fn an_escaping_path_is_refused() {
        let workspace = Workspace::new("escape");
        let coding = belt(&workspace);
        refuses(
            &coding,
            "--- a/../escaped.txt\n+++ b/../escaped.txt\n@@ -1 +1 @@\n-a\n+b\n",
            "the path contains a parent component",
        );
    }

    #[test]
    fn a_git_write_is_refused() {
        let workspace = Workspace::new("git-protected");
        let coding = belt(&workspace);
        refuses(
            &coding,
            "--- a/.git/config\n+++ b/.git/config\n@@ -1 +1 @@\n-a\n+b\n",
            "`.git` is protected",
        );
    }

    #[test]
    fn a_base_of_exactly_the_cap_is_patched_whole() {
        // The read window is one byte wider than the cap, and the whole base is what
        // the applier sees: a window one byte short would drop the file's last byte
        // into the written result.
        let workspace = Workspace::new("base-exact");
        write_sized(&workspace, "f.txt", MAX_PATCH_BASE_BYTES);
        let coding = belt(&workspace);
        build(&coding, &modify("f.txt"))
            .expect("a base at the cap is patched")
            .commit()
            .expect("commits");
        let patched = workspace.read("f.txt");
        assert_eq!(
            patched.len(),
            MAX_PATCH_BASE_BYTES,
            "the whole base reached the applier"
        );
        assert!(patched.starts_with("new\n"), "the hunk applied");
    }

    #[test]
    fn a_base_over_the_cap_is_refused() {
        let workspace = Workspace::new("base-cap");
        write_sized(&workspace, "big.txt", MAX_PATCH_BASE_BYTES + 1);
        let coding = belt(&workspace);
        refuses(
            &coding,
            &modify("big.txt"),
            "`big.txt` is too large to patch",
        );
    }

    #[test]
    fn a_base_that_is_not_text_is_refused() {
        let workspace = Workspace::new("base-binary");
        workspace.write_bytes("f.bin", b"old\n\0binary\n");
        let coding = belt(&workspace);
        refuses(&coding, &modify("f.bin"), "`f.bin` is not text");
    }

    #[test]
    fn a_base_that_is_not_utf8_is_refused() {
        let workspace = Workspace::new("base-utf8");
        workspace.write_bytes("f.bin", b"old\n\xff\xfe\n");
        let coding = belt(&workspace);
        refuses(&coding, &modify("f.bin"), "`f.bin` is not text");
    }

    #[test]
    fn an_empty_diff_is_refused() {
        let workspace = Workspace::new("empty-diff");
        let coding = belt(&workspace);
        refuses(&coding, "", "the diff is empty");
    }

    #[test]
    fn a_diff_with_a_nul_byte_is_refused() {
        let workspace = Workspace::new("nul-diff");
        let coding = belt(&workspace);
        refuses(
            &coding,
            "--- f\n+++ f\n@@ -1 +1 @@\n a\0\n",
            "the diff is not text",
        );
    }

    #[test]
    fn the_diff_size_cap_is_exactly_pinned_and_one_over_is_refused() {
        let workspace = Workspace::new("diff-cap");
        let coding = belt(&workspace);
        let prefix = "--- /dev/null\n+++ b/f.txt\n@@ -0,0 +1 @@\n+";
        let pad = "x".repeat(MAX_DIFF_BYTES - prefix.len() - 1);
        let exact = format!("{prefix}{pad}\n");
        assert_eq!(exact.len(), MAX_DIFF_BYTES);
        assert_eq!(
            build(&coding, &exact)
                .expect("exactly the cap is legal")
                .paths(),
            ["f.txt"]
        );

        let over = format!("{exact}y");
        assert_eq!(over.len(), MAX_DIFF_BYTES + 1);
        refuses(&coding, &over, "the diff is larger than the limit");
    }

    #[test]
    fn the_file_count_cap_is_exactly_pinned_and_one_over_is_refused() {
        let workspace = Workspace::new("file-cap");
        let coding = belt(&workspace);
        let exact: Vec<String> = (0..MAX_PATCH_FILES).map(|i| format!("f{i}")).collect();
        assert_eq!(
            build(&coding, &create_all(&exact))
                .expect("exactly the cap is legal")
                .paths()
                .len(),
            MAX_PATCH_FILES
        );

        let over: Vec<String> = (0..=MAX_PATCH_FILES).map(|i| format!("f{i}")).collect();
        refuses(&coding, &create_all(&over), "the diff names too many files");
    }

    #[test]
    fn the_plan_cap_is_exactly_pinned_and_one_over_is_refused() {
        // The plan holds every final text, and a base need not appear in the diff,
        // so a small diff can fill it. Eight mebibyte-sized bases plus one land
        // exactly on the cap; one byte more is refused.
        let workspace = Workspace::new("plan-cap");
        let coding = belt(&workspace);
        let mut paths: Vec<String> = (0..8).map(|i| format!("f{i}")).collect();
        paths.push("last".to_owned());
        for path in &paths[..8] {
            write_sized(&workspace, path, 1_000_000);
        }
        write_sized(&workspace, "last", 388_608);

        let diff = modify_all(&paths);
        let plan = build(&coding, &diff).expect("exactly the cap is legal");
        assert_eq!(plan.paths().len(), 9);

        write_sized(&workspace, "last", 388_609);
        refuses(&coding, &diff, "the patch result is larger than the limit");
    }
}
