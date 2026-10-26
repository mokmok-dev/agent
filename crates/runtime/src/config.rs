//! The configuration file.
//!
//! Everything is checked here, once, before the store is opened. A missing key
//! or a malformed URL that only surfaces on the first request would look like a
//! provider outage, and the operator would spend the evening reading someone
//! else's status page.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use super::{Settings, Tool};

/// The default deadline for one model call.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 120;

/// A tool that does not say otherwise runs for this long.
const DEFAULT_TOOL_TIMEOUT_MS: u64 = 30_000;

/// A tool that does not say otherwise may write this much into one event.
const DEFAULT_TOOL_MAX_OUTPUT: usize = 64 * 1024;

const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 4096;

/// Why the configuration could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The file could not be read.
    Read { path: PathBuf, detail: String },
    /// The file is not the TOML this daemon reads.
    Parse(String),
    /// A required setting is absent or empty.
    Missing(&'static str),
    /// The base URL is not an HTTP one.
    BadUrl(String),
    /// The named environment variable holds nothing.
    KeyEnvMissing(String),
    /// The key file could not be read.
    KeyFile { path: PathBuf, detail: String },
    /// The key resolved to nothing.
    KeyEmpty,
    /// A duration or a budget of zero would mean "never" or "nothing".
    Zero(&'static str),
    /// Two tools share a name, so the tool block would be ambiguous.
    DuplicateTool(String),
    /// A tool declared a class that is neither of the two this build knows.
    ToolClass(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match self {
            Self::Read { path, detail } => {
                write!(formatter, "cannot read {}: {detail}", path.display())
            },
            Self::Parse(detail) => write!(formatter, "cannot parse the configuration: {detail}"),
            Self::Missing(setting) => write!(formatter, "{setting} is required"),
            Self::BadUrl(url) => write!(formatter, "base_url is not an http or https URL: {url}"),
            Self::KeyEnvMissing(name) => {
                write!(formatter, "the environment variable {name} is not set")
            },
            Self::KeyFile { path, detail } => {
                write!(
                    formatter,
                    "cannot read the key from {}: {detail}",
                    path.display()
                )
            },
            Self::KeyEmpty => write!(formatter, "the API key is empty"),
            Self::Zero(setting) => write!(formatter, "{setting} must be greater than zero"),
            Self::DuplicateTool(name) => write!(formatter, "two tools are named {name}"),
            Self::ToolClass(class) => write!(
                formatter,
                "a tool class is idempotent or non-idempotent, not {class}"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Where the key comes from. The file itself never holds it.
#[derive(Debug, Deserialize)]
struct Raw {
    state_dir: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    api_key_file: Option<String>,
    timeout_seconds: Option<u64>,
    max_output_tokens: Option<u64>,
    directive_version: Option<String>,
    directive: Option<String>,
    #[serde(default)]
    tools: Vec<RawTool>,
}

#[derive(Debug, Deserialize)]
struct RawTool {
    name: String,
    schema: String,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
}

/// A configuration that has been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub state_dir: PathBuf,
    pub settings: Settings,
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
}

/// Reads and checks a configuration file.
///
/// `env` is passed in so a test can supply variables without touching the
/// process environment, which is shared with every other test in the binary.
///
/// # Errors
///
/// Returns [`ConfigError`] when the file cannot be read or parsed, when a
/// required setting is absent, when the URL is not HTTP, or when the key does
/// not resolve.
pub fn load(
    path: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|error| ConfigError::Read {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    parse(&text, env)
}

/// Checks a configuration that is already in memory.
///
/// # Errors
///
/// As [`load`].
pub fn parse(
    text: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Config, ConfigError> {
    let raw: Raw = toml::from_str(text).map_err(|error| ConfigError::Parse(error.to_string()))?;

    let api_key = resolve_key(&raw, env)?;
    let state_dir = required(raw.state_dir, "state_dir")?;
    let model = required(raw.model, "model")?;
    let base_url = required(raw.base_url, "base_url")?;
    let directive = required(raw.directive, "directive")?;
    let directive_version = required(raw.directive_version, "directive_version")?;

    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(ConfigError::BadUrl(base_url));
    }

    let timeout_seconds = raw.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    if timeout_seconds == 0 {
        return Err(ConfigError::Zero("timeout_seconds"));
    }
    let max_output_tokens = raw.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    if max_output_tokens == 0 {
        return Err(ConfigError::Zero("max_output_tokens"));
    }

    let mut seen = BTreeSet::new();
    let mut tools = Vec::with_capacity(raw.tools.len());
    for tool in raw.tools {
        if tool.name.is_empty() {
            return Err(ConfigError::Missing("tools[].name"));
        }
        if !seen.insert(tool.name.clone()) {
            return Err(ConfigError::DuplicateTool(tool.name));
        }
        if tool.argv.is_empty() {
            return Err(ConfigError::Missing("tools[].argv"));
        }
        let idempotent = match tool.class.as_deref() {
            None | Some("idempotent") => true,
            Some("non-idempotent") => false,
            Some(other) => return Err(ConfigError::ToolClass(other.to_string())),
        };
        let timeout_ms = tool.timeout_ms.unwrap_or(DEFAULT_TOOL_TIMEOUT_MS);
        if timeout_ms == 0 {
            return Err(ConfigError::Zero("tools[].timeout_ms"));
        }
        let max_output = tool.max_output_bytes.unwrap_or(DEFAULT_TOOL_MAX_OUTPUT);
        if max_output == 0 {
            return Err(ConfigError::Zero("tools[].max_output_bytes"));
        }
        tools.push(Tool {
            argv: tool.argv,
            idempotent,
            max_output,
            name: tool.name,
            schema: tool.schema,
            timeout: Duration::from_millis(timeout_ms),
        });
    }

    Ok(Config {
        state_dir: PathBuf::from(state_dir),
        settings: Settings {
            model,
            directive_version,
            directive,
            tools,
            max_output_tokens,
        },
        base_url,
        api_key,
        timeout: Duration::from_secs(timeout_seconds),
    })
}

fn required(
    value: Option<String>,
    name: &'static str,
) -> Result<String, ConfigError> {
    match value {
        Some(text) if !text.is_empty() => Ok(text),
        _ => Err(ConfigError::Missing(name)),
    }
}

/// The key, from the environment or from a file, never from the configuration.
fn resolve_key(
    raw: &Raw,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<String>, ConfigError> {
    if let Some(name) = &raw.api_key_env {
        let value = env(name).ok_or_else(|| ConfigError::KeyEnvMissing(name.clone()))?;
        if value.is_empty() {
            return Err(ConfigError::KeyEmpty);
        }
        return Ok(Some(value));
    }
    if let Some(path) = &raw.api_key_file {
        let path = PathBuf::from(path);
        let text = std::fs::read_to_string(&path).map_err(|error| ConfigError::KeyFile {
            path: path.clone(),
            detail: error.to_string(),
        })?;
        let key = text.trim_end_matches(['\n', '\r']).to_string();
        if key.is_empty() {
            return Err(ConfigError::KeyEmpty);
        }
        return Ok(Some(key));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, parse};
    use std::time::Duration;

    const MINIMAL: &str = r#"
state_dir = "/var/lib/agent"
model = "deepseek-chat"
base_url = "https://api.example.test/v1"
directive_version = "2026-10-26"
directive = "You are a careful agent."
timeout_seconds = 30
max_output_tokens = 512

[[tools]]
argv = ["/bin/cat"]
name = "shell"
schema = '{"argv":"string"}'
"#;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// A top-level setting, inserted above the tool table so it stays top level.
    fn with(line: &str) -> String {
        MINIMAL.replace(
            "max_output_tokens = 512",
            &format!("max_output_tokens = 512\n{line}"),
        )
    }

    fn key_env(name: &str) -> Option<String> {
        match name {
            "AGENT_KEY" => Some("sk-test".to_string()),
            _ => None,
        }
    }

    #[test]
    fn a_complete_file_becomes_a_configuration() {
        let config = parse(MINIMAL, &no_env).expect("parses");
        assert_eq!(config.state_dir.to_string_lossy(), "/var/lib/agent");
        assert_eq!(config.settings.model, "deepseek-chat");
        assert_eq!(config.settings.directive_version, "2026-10-26");
        assert_eq!(config.settings.max_output_tokens, 512);
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert_eq!(config.settings.tools.len(), 1);
        assert_eq!(config.api_key, None, "a local provider needs no key");
    }

    #[test]
    fn a_key_comes_from_the_environment_when_named() {
        let text = with("api_key_env = \"AGENT_KEY\"");
        let config = parse(&text, &key_env).expect("parses");
        assert_eq!(config.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn a_named_variable_that_is_not_set_is_a_startup_error() {
        let text = with("api_key_env = \"MISSING_KEY\"");
        assert_eq!(
            parse(&text, &key_env),
            Err(ConfigError::KeyEnvMissing("MISSING_KEY".to_string()))
        );
    }

    #[test]
    fn an_empty_variable_is_not_a_key() {
        let text = with("api_key_env = \"EMPTY\"");
        let empty = |_: &str| Some(String::new());
        assert_eq!(parse(&text, &empty), Err(ConfigError::KeyEmpty));
    }

    #[test]
    fn a_key_file_is_read_with_its_trailing_newline_removed() {
        let path = std::env::temp_dir().join(format!("agent-key-{}", std::process::id()));
        std::fs::write(&path, "sk-file\n").expect("writes");
        let text = with(&format!("api_key_file = {:?}", path.to_string_lossy()));
        let config = parse(&text, &no_env).expect("parses");
        assert_eq!(config.api_key.as_deref(), Some("sk-file"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_key_file_that_is_not_there_is_a_startup_error() {
        let text = with("api_key_file = \"/nonexistent/key\"");
        match parse(&text, &no_env) {
            Err(ConfigError::KeyFile { path, .. }) => {
                assert_eq!(path.to_string_lossy(), "/nonexistent/key");
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn every_required_setting_is_required() {
        for setting in [
            "state_dir",
            "model",
            "base_url",
            "directive",
            "directive_version",
        ] {
            let text = MINIMAL
                .lines()
                .filter(|line| !line.starts_with(&format!("{setting} =")))
                .collect::<Vec<_>>()
                .join("\n");
            match parse(&text, &no_env) {
                Err(ConfigError::Missing(name)) => assert_eq!(name, setting),
                other => panic!("{setting}: unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn a_setting_that_is_present_but_empty_is_treated_as_absent() {
        let text = MINIMAL.replace("model = \"deepseek-chat\"", "model = \"\"");
        assert_eq!(parse(&text, &no_env), Err(ConfigError::Missing("model")));

        let text = MINIMAL.replace(
            "directive = \"You are a careful agent.\"",
            "directive = \"\"",
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Missing("directive"))
        );

        let text = MINIMAL.replace(
            "directive_version = \"2026-10-26\"",
            "directive_version = \"\"",
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Missing("directive_version"))
        );
    }

    #[test]
    fn a_url_that_is_not_http_is_refused() {
        let text = MINIMAL.replace("https://api.example.test/v1", "api.example.test");
        match parse(&text, &no_env) {
            Err(ConfigError::BadUrl(url)) => assert_eq!(url, "api.example.test"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_tool_that_says_nothing_about_its_output_gets_sixty_four_kilobytes() {
        let config = parse(MINIMAL, &no_env).expect("parses");
        assert_eq!(config.settings.tools[0].max_output, 65_536);
        assert_eq!(config.settings.tools[0].timeout.as_millis(), 30_000);
        assert!(config.settings.tools[0].idempotent, "the safe default");
    }

    #[test]
    fn a_tool_class_this_build_does_not_know_is_refused() {
        let text = format!(
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nclass = \"maybe\"\nname = \"other\"\nschema = '{{}}'\n"
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::ToolClass("maybe".to_string()))
        );
    }

    #[test]
    fn a_zero_timeout_is_refused() {
        let text = MINIMAL.replace("timeout_seconds = 30", "timeout_seconds = 0");
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Zero("timeout_seconds"))
        );
    }

    #[test]
    fn a_zero_budget_is_refused() {
        let text = MINIMAL.replace("max_output_tokens = 512", "max_output_tokens = 0");
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Zero("max_output_tokens"))
        );
    }

    #[test]
    fn the_defaults_apply_when_the_optional_settings_are_absent() {
        let text = MINIMAL
            .lines()
            .filter(|line| {
                !line.starts_with("timeout_seconds") && !line.starts_with("max_output_tokens")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let config = parse(&text, &no_env).expect("parses");
        assert_eq!(
            config.timeout,
            Duration::from_secs(super::DEFAULT_TIMEOUT_SECONDS)
        );
        assert_eq!(
            config.settings.max_output_tokens,
            super::DEFAULT_MAX_OUTPUT_TOKENS
        );
    }

    #[test]
    fn two_tools_with_one_name_are_refused() {
        let text = format!(
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nname = \"shell\"\nschema = '{{}}'\n"
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::DuplicateTool("shell".to_string()))
        );
    }

    #[test]
    fn a_tool_with_no_name_is_refused() {
        let text =
            format!("{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nname = \"\"\nschema = '{{}}'\n");
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Missing("tools[].name"))
        );
    }

    #[test]
    fn a_file_that_is_not_toml_is_refused() {
        assert!(matches!(
            parse("this is not toml ][", &no_env),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn every_error_explains_itself() {
        for error in [
            ConfigError::Missing("model"),
            ConfigError::BadUrl("x".to_string()),
            ConfigError::KeyEnvMissing("K".to_string()),
            ConfigError::KeyEmpty,
            ConfigError::Zero("timeout_seconds"),
            ConfigError::DuplicateTool("shell".to_string()),
        ] {
            assert_ne!(error.to_string(), "");
        }
    }
}
