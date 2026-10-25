//! The bundle check. Every function that decides something takes a string and
//! returns a verdict, so a test needs no filesystem and no fixture.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
}

pub struct Finding {
    pub level: Level,
    pub path: PathBuf,
    pub message: String,
}

pub struct Report {
    findings: Vec<Finding>,
    files: usize,
    concepts: usize,
}

fn finding(
    level: Level,
    path: &Path,
    message: impl Into<String>,
) -> Finding {
    Finding {
        level,
        path: path.to_path_buf(),
        message: message.into(),
    }
}

impl Report {
    pub fn print(
        &self,
        root: &Path,
    ) -> ExitCode {
        let mut errors = 0;
        let mut warnings = 0;
        for entry in &self.findings {
            let level = match entry.level {
                Level::Error => {
                    errors += 1;
                    "error"
                },
                Level::Warning => {
                    warnings += 1;
                    "warn"
                },
            };
            let shown = entry.path.strip_prefix(root).unwrap_or(&entry.path);
            println!("{level}: {}: {}", shown.display(), entry.message);
        }
        println!(
            "checked {} files, {} concepts, {errors} errors, {warnings} warnings",
            self.files, self.concepts
        );
        if errors == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Concept,
    RootIndex,
    SubIndex,
    Log,
}

fn classify(
    path: &Path,
    root: &Path,
) -> Kind {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    match name {
        "index.md" if path.parent() == Some(root) => Kind::RootIndex,
        "index.md" => Kind::SubIndex,
        "log.md" => Kind::Log,
        _ => Kind::Concept,
    }
}

/// Splits a markdown file into its frontmatter block and its body.
///
/// `Ok(None)` means the file has no frontmatter, which is legal for the
/// reserved files and an error for a concept. An opening fence with no closing
/// fence is an error rather than a body, because treating it as a body would
/// hide a typo that makes a whole concept unparseable.
fn split_frontmatter(text: &str) -> Result<Option<(&str, &str)>, &'static str> {
    let Some(rest) = text.strip_prefix("---\n") else {
        return Ok(None);
    };
    let Some(end) = rest.find("\n---\n") else {
        return Err("frontmatter opens but never closes");
    };
    Ok(Some((&rest[..end], &rest[end + 5..])))
}

fn parse_frontmatter(block: &str) -> Result<serde_yaml_ng::Mapping, String> {
    match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(block) {
        Ok(serde_yaml_ng::Value::Mapping(map)) => Ok(map),
        Ok(_) => Err("frontmatter is not a mapping".to_owned()),
        Err(error) => Err(format!("frontmatter is not valid YAML: {error}")),
    }
}

fn string_field<'a>(
    map: &'a serde_yaml_ng::Mapping,
    key: &str,
) -> Option<&'a str> {
    map.get(serde_yaml_ng::Value::String(key.to_owned()))
        .and_then(serde_yaml_ng::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn source_ids(map: &serde_yaml_ng::Mapping) -> BTreeSet<&str> {
    let Some(serde_yaml_ng::Value::Sequence(entries)) =
        map.get(serde_yaml_ng::Value::String("sources".to_owned()))
    else {
        return BTreeSet::new();
    };
    entries
        .iter()
        .filter_map(|entry| match entry {
            serde_yaml_ng::Value::Mapping(entry) => string_field(entry, "id"),
            _ => None,
        })
        .collect()
}

/// Returns the footnote labels the body cites and the labels it defines.
///
/// Both forms share the `[^label]` shape, so the scanner looks one byte past
/// the closing bracket: a colon there makes the label a definition, anything
/// else makes it a citation.
fn footnotes(body: &str) -> (BTreeSet<&str>, BTreeSet<&str>) {
    let mut cited = BTreeSet::new();
    let mut defined = BTreeSet::new();
    let mut cursor = 0;
    while let Some(offset) = body[cursor..].find("[^") {
        let start = cursor + offset + 2;
        let Some(close) = body[start..].find(']') else {
            break;
        };
        let end = start + close;
        let label = &body[start..end];
        if !label.is_empty() && !label.contains(char::is_whitespace) {
            if body[end + 1..].starts_with(':') {
                defined.insert(label);
            } else {
                cited.insert(label);
            }
        }
        cursor = end + 1;
    }
    (cited, defined)
}

fn is_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

fn log_dates(body: &str) -> Vec<&str> {
    body.lines()
        .filter_map(|line| {
            let date = line.strip_prefix("## ")?.trim();
            is_date(date).then_some(date)
        })
        .collect()
}

/// Checks the ISO 8601 shape the format requires, including the offset.
///
/// A timestamp with no offset leaves the instant unknown, which is the one
/// thing the field exists to remove, so the check rejects it.
fn is_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20 {
        return false;
    }
    let digit = |index: usize| bytes.get(index).is_some_and(u8::is_ascii_digit);
    if !(digit(0) && digit(1) && digit(2) && digit(3)) {
        return false;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return false;
    }
    if !(digit(5)
        && digit(6)
        && digit(8)
        && digit(9)
        && digit(11)
        && digit(12)
        && digit(14)
        && digit(15)
        && digit(17)
        && digit(18))
    {
        return false;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return false;
    }
    let mut cursor = 19;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let start = cursor;
        while digit(cursor) {
            cursor += 1;
        }
        if cursor == start {
            return false;
        }
    }
    match bytes.get(cursor) {
        Some(&b'Z') => bytes.len() == cursor + 1,
        Some(&sign) if sign == b'+' || sign == b'-' => {
            bytes.len() == cursor + 6
                && digit(cursor + 1)
                && digit(cursor + 2)
                && bytes.get(cursor + 3) == Some(&b':')
                && digit(cursor + 4)
                && digit(cursor + 5)
        },
        _ => false,
    }
}

fn links(body: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = body[cursor..].find("](") {
        let start = cursor + offset + 2;
        let Some(close) = body[start..].find(')') else {
            break;
        };
        out.push(&body[start..start + close]);
        cursor = start + close + 1;
    }
    out
}

fn is_external(target: &str) -> bool {
    target.contains("://") || target.starts_with('#') || target.starts_with("mailto:")
}

fn resolves(
    from: &Path,
    target: &str,
) -> bool {
    let clean = target.split('#').next().unwrap_or(target);
    if clean.is_empty() {
        return true;
    }
    let Some(parent) = from.parent() else {
        return false;
    };
    parent.join(clean).exists()
}

fn check_links(
    path: &Path,
    body: &str,
    level: Level,
    findings: &mut Vec<Finding>,
) {
    for link in links(body) {
        if !is_external(link) && !resolves(path, link) {
            findings.push(finding(
                level,
                path,
                format!("link does not resolve: {link}"),
            ));
        }
    }
}

fn check_concept(
    path: &Path,
    text: &str,
    findings: &mut Vec<Finding>,
) {
    let split = match split_frontmatter(text) {
        Ok(split) => split,
        Err(message) => {
            findings.push(finding(Level::Error, path, message));
            return;
        },
    };
    let Some((block, body)) = split else {
        findings.push(finding(Level::Error, path, "concept has no frontmatter"));
        return;
    };

    let map = match parse_frontmatter(block) {
        Ok(map) => map,
        Err(message) => {
            findings.push(finding(Level::Error, path, message));
            return;
        },
    };

    if string_field(&map, "type").is_none() {
        findings.push(finding(
            Level::Error,
            path,
            "type is required and must be non-empty",
        ));
    }
    if let Some(status) = string_field(&map, "status")
        && !matches!(status, "draft" | "stable" | "deprecated")
    {
        findings.push(finding(
            Level::Error,
            path,
            format!("status must be draft, stable, or deprecated, got {status}"),
        ));
    }
    if let Some(serde_yaml_ng::Value::Mapping(generated)) =
        map.get(serde_yaml_ng::Value::String("generated".to_owned()))
    {
        match string_field(generated, "at") {
            None => findings.push(finding(
                Level::Error,
                path,
                "generated is present but has no at",
            )),
            Some(at) if !is_timestamp(at) => findings.push(finding(
                Level::Error,
                path,
                format!("generated.at is not ISO 8601 with an offset: {at}"),
            )),
            Some(_) => {},
        }
    }

    let ids = source_ids(&map);
    let (cited, defined) = footnotes(body);
    for label in cited.difference(&defined) {
        findings.push(finding(
            Level::Error,
            path,
            format!("footnote [^{label}] is cited but never defined"),
        ));
    }
    for label in &defined {
        if !ids.contains(label) {
            findings.push(finding(
                Level::Error,
                path,
                format!("footnote [^{label}] has no matching sources[].id"),
            ));
        }
    }

    check_links(path, body, Level::Warning, findings);
}

fn check_index(
    path: &Path,
    text: &str,
    is_root: bool,
    findings: &mut Vec<Finding>,
) {
    let split = match split_frontmatter(text) {
        Ok(split) => split,
        Err(message) => {
            findings.push(finding(Level::Error, path, message));
            return;
        },
    };

    match split {
        None => {
            if is_root {
                findings.push(finding(
                    Level::Warning,
                    path,
                    "root index does not declare okf_version",
                ));
            }
            check_links(path, text, Level::Error, findings);
        },
        Some((block, body)) => {
            if is_root {
                check_root_frontmatter(path, block, findings);
            } else {
                findings.push(finding(
                    Level::Error,
                    path,
                    "only the bundle-root index.md may carry frontmatter",
                ));
            }
            check_links(path, body, Level::Error, findings);
        },
    }
}

fn check_root_frontmatter(
    path: &Path,
    block: &str,
    findings: &mut Vec<Finding>,
) {
    let map = match parse_frontmatter(block) {
        Ok(map) => map,
        Err(message) => {
            findings.push(finding(Level::Error, path, message));
            return;
        },
    };
    for key in map.keys() {
        if key.as_str() != Some("okf_version") {
            let name = key
                .as_str()
                .map_or_else(|| format!("{key:?}"), str::to_owned);
            findings.push(finding(
                Level::Error,
                path,
                format!("root index frontmatter may only hold okf_version, found {name}"),
            ));
        }
    }
    if string_field(&map, "okf_version") != Some("0.2") {
        findings.push(finding(Level::Error, path, "okf_version must be \"0.2\""));
    }
}

fn check_log(
    path: &Path,
    text: &str,
    findings: &mut Vec<Finding>,
) {
    if let Ok(Some(_)) = split_frontmatter(text) {
        findings.push(finding(
            Level::Error,
            path,
            "log.md must not carry frontmatter",
        ));
        return;
    }
    let dates = log_dates(text);
    if dates.is_empty() {
        findings.push(finding(
            Level::Error,
            path,
            "log.md has no YYYY-MM-DD headings",
        ));
        return;
    }
    for pair in dates.windows(2) {
        if pair[1] > pair[0] {
            findings.push(finding(
                Level::Error,
                path,
                format!(
                    "log dates are not newest first: {} then {}",
                    pair[0], pair[1]
                ),
            ));
        }
    }
}

fn walk(
    dir: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            walk(&path, out)?;
        } else if path.extension().is_some_and(|extension| extension == "md") {
            out.push(path);
        }
    }
    Ok(())
}

pub fn check_bundle(root: &Path) -> Result<Report, String> {
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let mut files = Vec::new();
    walk(root, &mut files)?;

    let mut findings = Vec::new();
    let mut concepts = 0;
    let mut index_text = String::new();

    for path in &files {
        let text = fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        match classify(path, root) {
            Kind::Concept => {
                concepts += 1;
                check_concept(path, &text, &mut findings);
            },
            Kind::RootIndex => {
                check_index(path, &text, true, &mut findings);
                index_text.push_str(&text);
                index_text.push('\n');
            },
            Kind::SubIndex => {
                check_index(path, &text, false, &mut findings);
                index_text.push_str(&text);
                index_text.push('\n');
            },
            Kind::Log => check_log(path, &text, &mut findings),
        }
    }

    for path in &files {
        if classify(path, root) != Kind::Concept {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !index_text.contains(name) {
            findings.push(finding(
                Level::Error,
                path,
                "concept is not listed in any index.md",
            ));
        }
    }

    Ok(Report {
        findings,
        files: files.len(),
        concepts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_splits_at_the_fence() {
        assert_eq!(
            split_frontmatter("---\ntype: A\n---\n\nbody\n"),
            Ok(Some(("type: A", "\nbody\n")))
        );
    }

    #[test]
    fn a_file_without_a_fence_has_no_frontmatter() {
        assert_eq!(split_frontmatter("# Title\n"), Ok(None));
    }

    #[test]
    fn an_unclosed_fence_is_an_error() {
        assert_eq!(
            split_frontmatter("---\ntype: A\n"),
            Err("frontmatter opens but never closes")
        );
    }

    #[test]
    fn frontmatter_parses_into_a_mapping() {
        let map = parse_frontmatter("type: Policy\nstatus: draft\n").expect("parses");
        assert_eq!(string_field(&map, "type"), Some("Policy"));
    }

    #[test]
    fn a_scalar_frontmatter_is_rejected() {
        assert!(parse_frontmatter("just a string").is_err());
    }

    #[test]
    fn blank_values_do_not_count_as_a_type() {
        let map = parse_frontmatter("type: \"  \"\n").expect("parses");
        assert_eq!(string_field(&map, "type"), None);
    }

    #[test]
    fn sources_yield_their_ids_in_order() {
        let map = parse_frontmatter("sources:\n  - id: b\n  - id: a\n").expect("parses");
        assert_eq!(source_ids(&map), BTreeSet::from(["a", "b"]));
    }

    #[test]
    fn a_concept_without_sources_has_no_ids() {
        let map = parse_frontmatter("type: Policy\n").expect("parses");
        assert!(source_ids(&map).is_empty());
    }

    #[test]
    fn footnotes_separate_citations_from_definitions() {
        let (cited, defined) = footnotes("A claim.[^one]\n\n[^one]: The source\n");
        assert_eq!(cited, BTreeSet::from(["one"]));
        assert_eq!(defined, BTreeSet::from(["one"]));
    }

    #[test]
    fn a_footnote_with_no_closing_bracket_is_ignored() {
        let (cited, defined) = footnotes("A claim.[^one\n");
        assert!(cited.is_empty());
        assert!(defined.is_empty());
    }

    #[test]
    fn a_footnote_label_with_a_space_is_ignored() {
        let (cited, defined) = footnotes("text[^not a label] more\n");
        assert!(cited.is_empty());
        assert!(defined.is_empty());
    }

    #[test]
    fn dates_are_only_matched_in_heading_position() {
        assert_eq!(
            log_dates("## 2026-10-25\n* x\n## not a date\n"),
            vec!["2026-10-25"]
        );
    }

    #[test]
    fn a_date_needs_ten_shaped_characters() {
        assert!(is_date("2026-10-25"));
        assert!(!is_date("2026-1-25"));
        assert!(!is_date("2026/10/25"));
        assert!(!is_date("2026-10-25T00:00:00Z"));
    }

    #[test]
    fn timestamps_need_an_offset() {
        assert!(is_timestamp("2026-10-25T12:57:16Z"));
        assert!(is_timestamp("2026-10-25T12:57:16+09:00"));
        assert!(is_timestamp("2026-10-25T12:57:16.123Z"));
        assert!(!is_timestamp("2026-10-25T12:57:16"));
        assert!(!is_timestamp("2026-10-25"));
    }

    #[test]
    fn a_fraction_without_digits_is_not_a_timestamp() {
        assert!(!is_timestamp("2026-10-25T12:57:16.Z"));
    }

    #[test]
    fn an_offset_without_minutes_is_not_a_timestamp() {
        assert!(!is_timestamp("2026-10-25T12:57:16+0900"));
    }

    #[test]
    fn links_are_extracted_in_order() {
        assert_eq!(
            links("see [a](a.md) and [b](https://x/y)"),
            vec!["a.md", "https://x/y"]
        );
    }

    #[test]
    fn an_unclosed_link_is_ignored() {
        assert_eq!(links("see [a](a.md"), Vec::<&str>::new());
    }

    #[test]
    fn absolute_targets_are_external() {
        assert!(is_external("https://example.com"));
        assert!(is_external("#anchor"));
        assert!(!is_external("../a.md"));
    }

    #[test]
    fn reserved_names_are_classified_by_position() {
        let root = Path::new("/bundle");
        assert_eq!(
            classify(Path::new("/bundle/index.md"), root),
            Kind::RootIndex
        );
        assert_eq!(
            classify(Path::new("/bundle/sub/index.md"), root),
            Kind::SubIndex
        );
        assert_eq!(classify(Path::new("/bundle/sub/log.md"), root), Kind::Log);
        assert_eq!(classify(Path::new("/bundle/sub/a.md"), root), Kind::Concept);
    }

    #[test]
    fn a_concept_missing_its_type_is_an_error() {
        let mut findings = Vec::new();
        check_concept(
            Path::new("a.md"),
            "---\ntitle: A\n---\nbody\n",
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .any(|entry| entry.message.contains("type is required"))
        );
    }

    #[test]
    fn a_cited_but_undefined_footnote_is_an_error() {
        let mut findings = Vec::new();
        check_concept(
            Path::new("a.md"),
            "---\ntype: A\nsources:\n  - id: one\n---\nclaim.[^one]\n",
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .any(|entry| entry.message.contains("never defined"))
        );
    }

    #[test]
    fn a_defined_footnote_outside_sources_is_an_error() {
        let mut findings = Vec::new();
        check_concept(
            Path::new("a.md"),
            "---\ntype: A\n---\nclaim.[^one]\n\n[^one]: Source\n",
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .any(|entry| entry.message.contains("no matching sources"))
        );
    }

    #[test]
    fn a_matching_footnote_and_source_are_clean() {
        let mut findings = Vec::new();
        check_concept(
            Path::new("a.md"),
            "---\ntype: A\nsources:\n  - id: one\n---\nclaim.[^one]\n\n[^one]: Source\n",
            &mut findings,
        );
        assert!(findings.is_empty());
    }

    #[test]
    fn an_unknown_status_is_an_error() {
        let mut findings = Vec::new();
        check_concept(
            Path::new("a.md"),
            "---\ntype: A\nstatus: wip\n---\n",
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .any(|entry| entry.message.contains("status must be"))
        );
    }

    #[test]
    fn a_generated_block_without_at_is_an_error() {
        let mut findings = Vec::new();
        check_concept(
            Path::new("a.md"),
            "---\ntype: A\ngenerated: { by: agent/x }\n---\n",
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .any(|entry| entry.message.contains("has no at"))
        );
    }

    #[test]
    fn a_root_index_with_the_wrong_version_is_an_error() {
        let mut findings = Vec::new();
        check_index(
            Path::new("index.md"),
            "---\nokf_version: \"0.1\"\n---\n",
            true,
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .any(|entry| entry.message.contains("okf_version"))
        );
    }

    #[test]
    fn a_sub_index_with_frontmatter_is_an_error() {
        let mut findings = Vec::new();
        check_index(
            Path::new("sub/index.md"),
            "---\nokf_version: \"0.2\"\n---\n",
            false,
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .any(|entry| entry.message.contains("only the bundle-root"))
        );
    }

    #[test]
    fn a_log_with_frontmatter_is_an_error() {
        let mut findings = Vec::new();
        check_log(
            Path::new("log.md"),
            "---\nokf_version: \"0.2\"\n---\n## 2026-10-25\n",
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .any(|entry| entry.message.contains("must not carry frontmatter"))
        );
    }

    #[test]
    fn a_log_that_is_not_newest_first_is_an_error() {
        let mut findings = Vec::new();
        check_log(
            Path::new("log.md"),
            "## 2026-10-01\n## 2026-10-25\n",
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .any(|entry| entry.message.contains("not newest first"))
        );
    }

    #[test]
    fn a_log_with_newest_first_dates_is_clean() {
        let mut findings = Vec::new();
        check_log(
            Path::new("log.md"),
            "## 2026-10-25\n## 2026-10-01\n",
            &mut findings,
        );
        assert!(findings.is_empty());
    }
}
