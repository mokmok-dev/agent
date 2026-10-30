use serde_json::Value;

use super::{SelectError, registry, select, tool_block};
use crate::Allowlist;
use crate::Tool;
use crate::fixture::{self, TempDir};

/// The bytes the provider receives, pinned so a description or schema edit has
/// to be deliberate.
const GOLDEN: &str = r#"[{"type":"function","function":{"name":"edit_file","description":"Replace one exact string in a UTF-8 file in the workspace. Fails as not_found when the string is absent and as ambiguous when it occurs more than once unless all is true.","parameters":{"additionalProperties":false,"properties":{"all":{"description":"Replace every occurrence instead of requiring exactly one.","type":["boolean","null"]},"new":{"description":"The text that takes its place.","type":"string"},"old":{"description":"The exact text to replace. It must occur exactly once unless `all` is true.","type":"string"},"path":{"description":"The file, relative to the workspace.","type":"string"}},"required":["path","old","new"],"type":"object"}}},{"type":"function","function":{"name":"list_dir","description":"List a directory in the workspace, one to three levels deep. Entries are sorted by path; a symlink is reported and never followed.","parameters":{"additionalProperties":false,"properties":{"depth":{"description":"How many levels to list, from 1 to 3. Defaults to 1.","format":"uint8","maximum":255,"minimum":0,"type":["integer","null"]},"path":{"description":"The directory, relative to the workspace. Defaults to the workspace root.","type":["string","null"]}},"type":"object"}}},{"type":"function","function":{"name":"read_file","description":"Read a UTF-8 text file in the workspace. Returns a window of numbered lines and whether more of the file follows; page a long file with start_line and end_line.","parameters":{"additionalProperties":false,"properties":{"end_line":{"description":"The last line to return, counted from one and included.","format":"uint","minimum":0,"type":["integer","null"]},"path":{"description":"The file, relative to the workspace.","type":"string"},"start_line":{"description":"The first line to return, counted from one.","format":"uint","minimum":0,"type":["integer","null"]}},"required":["path"],"type":"object"}}},{"type":"function","function":{"name":"run","description":"Run a program from the allowlist in the workspace with a structured argument list. There is no shell, so a word is passed as written. Returns the exit status, standard output, and standard error.","parameters":{"additionalProperties":false,"properties":{"args":{"description":"Its arguments, each passed as one word.","items":{"type":"string"},"type":["array","null"]},"program":{"description":"The program's name or absolute path. It must be on the allowlist.","type":"string"},"stdin":{"description":"What to write to its standard input.","type":["string","null"]}},"required":["program"],"type":"object"}}},{"type":"function","function":{"name":"search","description":"Search the workspace for a Rust regular expression and return each matching line with its path, line, and column. Binary and oversized files are skipped.","parameters":{"additionalProperties":false,"properties":{"case_sensitive":{"description":"Whether the pattern is case sensitive. Defaults to true.","type":["boolean","null"]},"glob":{"description":"A glob over the workspace-relative path, with `*`, `?`, and `**`.","type":["string","null"]},"max_results":{"description":"The most matches to return. Defaults to 200 and cannot exceed it.","format":"uint","minimum":0,"type":["integer","null"]},"path":{"description":"Where to start, relative to the workspace. Defaults to the workspace root.","type":["string","null"]},"pattern":{"description":"A regular expression in Rust `regex` syntax.","type":"string"}},"required":["pattern"],"type":"object"}}},{"type":"function","function":{"name":"write_file","description":"Write a whole file in the workspace, creating its parents. The file is replaced atomically; prefer edit_file for a change to a file that exists.","parameters":{"additionalProperties":false,"properties":{"content":{"description":"The whole new content.","type":"string"},"path":{"description":"The file, relative to the workspace. Its parents are created.","type":"string"}},"required":["path","content"],"type":"object"}}}]"#;

#[test]
fn every_tool_refuses_an_argument_its_schema_does_not_name() {
    for tool in registry() {
        let dir = TempDir::new();
        let workspace = dir.workspace();
        let allow = Allowlist::default();
        let ctx = fixture::ctx(&workspace, &allow, dir.path());
        let answer = (tool.call)(br#"{"unknown":true}"#, &ctx);
        let value: Value = serde_json::from_slice(&answer).expect("the tool answers with JSON");
        assert_eq!(value["ok"], false, "{}", tool.name);
        assert_eq!(value["error"]["code"], "invalid_args", "{}", tool.name);
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("unknown field"),
            "{} took a key its schema does not name",
            tool.name
        );
        let schema = (tool.parameters)();
        assert_eq!(
            schema["additionalProperties"],
            Value::Bool(false),
            "{} leaves unknown keys open in its schema",
            tool.name
        );
        assert!(schema.get("$schema").is_none(), "{}", tool.name);
        assert!(schema.get("title").is_none(), "{}", tool.name);
    }
}

#[test]
fn the_registry_names_six_tools_in_name_order() {
    let names: Vec<&str> = registry().iter().map(|tool| tool.name).collect();
    assert_eq!(
        names,
        [
            "edit_file",
            "list_dir",
            "read_file",
            "run",
            "search",
            "write_file"
        ]
    );
}

#[test]
fn selecting_names_returns_them_and_nothing_else() {
    let requested = vec!["search".to_string(), "read_file".to_string()];
    let selected = select(Some(&requested)).expect("both exist");
    let names: Vec<&str> = selected.iter().map(|tool| tool.name).collect();
    assert_eq!(names, ["search", "read_file"]);
}

#[test]
fn selecting_nothing_returns_the_whole_registry() {
    assert_eq!(select(None).expect("always").len(), registry().len());
}

#[test]
fn an_unknown_name_is_refused() {
    let named = vec!["fetch".to_string()];
    assert_eq!(
        select(Some(&named)).err(),
        Some(SelectError::Unknown("fetch".to_string()))
    );
}

#[test]
fn only_run_takes_a_program_list() {
    for tool in registry() {
        let expected = if tool.name == "run" {
            crate::Programs::Listed
        } else {
            crate::Programs::None
        };
        assert_eq!(tool.programs, expected, "{}", tool.name);
    }
}

#[test]
fn the_tool_block_is_sorted_compact_and_shaped_for_the_provider() {
    let tools = select(None).expect("always");
    let block = tool_block(&tools);
    let parsed: Value = serde_json::from_str(&block).expect("the block is JSON");
    let entries = parsed.as_array().expect("the block is an array");
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| {
            assert_eq!(entry["type"], "function");
            entry["function"]["name"].as_str().expect("a name")
        })
        .collect();
    assert_eq!(
        names,
        [
            "edit_file",
            "list_dir",
            "read_file",
            "run",
            "search",
            "write_file"
        ]
    );
    assert!(!block.contains('\n'), "the block carries whitespace");
}

#[test]
fn the_tool_block_is_the_exact_bytes_the_provider_receives() {
    let tools = select(None).expect("always");
    assert_eq!(tool_block(&tools), GOLDEN);
}

#[test]
fn an_answer_that_is_not_json_is_invalid_args() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = Allowlist::default();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    for tool in registry() {
        let answer = (tool.call)(b"not json", &ctx);
        let value: Value = serde_json::from_slice(&answer).expect("the tool answers with JSON");
        assert_eq!(value["error"]["code"], "invalid_args", "{}", tool.name);
    }
}

#[test]
fn an_outside_path_is_reported_with_its_code() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = Allowlist::default();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let tool = registry()
        .iter()
        .find(|tool| tool.name == "read_file")
        .expect("read_file exists");
    let answer = (tool.call)(br#"{"path":"../secret"}"#, &ctx);
    let value: Value = serde_json::from_slice(&answer).expect("the tool answers with JSON");
    assert_eq!(value["error"]["code"], "outside_workspace");
}

#[test]
fn an_unknown_name_reads_as_the_sentence_it_is() {
    assert_eq!(
        SelectError::Unknown("fetch".to_string()).to_string(),
        "there is no tool named fetch"
    );
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
struct EchoArgs {
    text: String,
}

struct Echo;

impl crate::Tool for Echo {
    type Args = EchoArgs;
    type Output = String;

    const NAME: &'static str = "echo";
    const DESCRIPTION: &'static str = "Answers with the text it was given.";
    const CLASS: crate::Class = crate::Class::Idempotent;
    const RISK: agent_core::event::Risk = agent_core::event::Risk::Read;
    const SCOPE: crate::Scope = crate::Scope::Read;
    const LIMITS: crate::Limits = crate::Limits {
        timeout_ms: 1_000,
        event_bytes: 64,
    };
    const ENV: &'static [&'static str] = &[];

    fn run(
        args: EchoArgs,
        _ctx: &crate::Ctx<'_>,
    ) -> Result<String, crate::ToolError> {
        Ok(args.text)
    }
}

#[test]
fn an_answer_at_the_cap_is_delivered_and_one_byte_over_is_refused() {
    let dir = TempDir::new();
    let workspace = dir.workspace();
    let allow = Allowlist::default();
    let ctx = fixture::ctx(&workspace, &allow, dir.path());
    let cap = Echo::LIMITS.event_bytes;
    let base = crate::envelope::success(&String::new()).len();
    let exact = "x".repeat(cap - base);
    let answer = super::call::<Echo>(format!(r#"{{"text":"{exact}"}}"#).as_bytes(), &ctx);
    let value: Value = serde_json::from_slice(&answer).expect("one object");
    assert_eq!(
        value["ok"], true,
        "an answer of exactly the cap was refused"
    );

    let over = "x".repeat(cap - base + 1);
    let answer = super::call::<Echo>(format!(r#"{{"text":"{over}"}}"#).as_bytes(), &ctx);
    let value: Value = serde_json::from_slice(&answer).expect("one object");
    assert_eq!(value["error"]["code"], "too_large");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("over its 64 byte cap"),
        "{value}"
    );
}
