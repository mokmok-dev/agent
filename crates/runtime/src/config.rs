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

use agent_core::event::Risk;

use super::{Network, Settings, Tool};

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
    /// A tool declared a name that is not an environment variable name.
    BadEnvName(String),
    /// A tool declared a variable and the process has no such variable.
    EnvMissing(String),
    /// A path a tool may write is not absolute, so it would be resolved against
    /// the launcher's working directory rather than the tool's.
    BadPath(String),
    /// A tool declared network access this build does not know how to give it.
    BadNetwork(String),
    /// A directory a tool may write is not there, so the launcher has nothing to
    /// mount and the tool would fail on its first call.
    MissingDirectory(String),
    /// The sandbox setting is neither a pair of paths nor the word none.
    BadSandbox(String),
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
            Self::BadEnvName(name) => {
                write!(formatter, "{name} is not an environment variable name")
            },
            Self::EnvMissing(name) => {
                write!(formatter, "{name} is declared by a tool but is not set")
            },
            Self::BadPath(path) => write!(formatter, "{path} is not an absolute path"),
            Self::BadNetwork(value) => {
                write!(formatter, "network is none or any, not {value}")
            },
            Self::BadSandbox(value) => {
                write!(
                    formatter,
                    "sandbox names a launcher without both paths: {value}"
                )
            },
            Self::MissingDirectory(path) => {
                write!(
                    formatter,
                    "{path} is writable by a tool but is not a directory"
                )
            },
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
    #[serde(default)]
    approval_seconds: Option<u64>,
    directive_version: Option<String>,
    directive: Option<String>,
    #[serde(default)]
    tools: Vec<RawTool>,
    #[serde(default)]
    sandbox: Option<RawSandbox>,
}

#[derive(Debug, Deserialize)]
struct RawTool {
    name: String,
    schema: String,
    #[serde(default)]
    risk: Option<String>,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
    #[serde(default)]
    permissions: Option<RawPermissions>,
}

#[derive(Debug, Deserialize)]
struct RawPermissions {
    /// Absent is not the same as empty: an empty list means the tool sees no
    /// environment at all, and a missing list means the operator said nothing.
    #[serde(default)]
    env: Option<Vec<String>>,
    #[serde(default)]
    write: Option<Vec<String>>,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    memory_bytes: Option<u64>,
    #[serde(default)]
    file_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawSandbox {
    /// An operator may say, in as many words, that this host wants no bounds.
    #[serde(default)]
    none: Option<bool>,
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
#[allow(
    clippy::too_many_lines,
    reason = "one pass over the file, so a setting cannot be read after it is used"
)]
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

    // A tool is bounded unless the configuration says, in as many words, that it
    // wants none. Nothing writes that for the operator, and the module never does.
    let confined = !matches!(
        raw.sandbox,
        Some(RawSandbox {
            none: Some(true),
            ..
        })
    );

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
        // No default. What a tool may touch is the operator's statement, and a
        // default would be this program making it for them.
        let risk = match tool.risk.as_deref() {
            Some("read") => Risk::Read,
            Some("write") => Risk::Write,
            Some("network") => Risk::Network,
            Some("destructive") => Risk::Destructive,
            Some(_) | None => return Err(ConfigError::Missing("tools[].risk")),
        };
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
        let Some(permissions) = tool.permissions else {
            return Err(ConfigError::Missing("tools[].permissions.env"));
        };
        let Some(declared) = permissions.env else {
            return Err(ConfigError::Missing("tools[].permissions.env"));
        };
        let mut names = BTreeSet::new();
        for name in declared {
            if name.is_empty() || name.contains(['=', '\0']) {
                return Err(ConfigError::BadEnvName(name));
            }
            if env(&name).is_none() {
                return Err(ConfigError::EnvMissing(name));
            }
            names.insert(name);
        }
        let mut write = Vec::new();
        for path in permissions.write.unwrap_or_default() {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(ConfigError::BadPath(path.display().to_string()));
            }
            // The launcher mounts this directory into the tool's namespace, and a
            // mount of something that is not there fails on the first call rather
            // than here. The operator or the module creates it; this program does
            // not create directories it was only given permission to use.
            if !path.is_dir() {
                return Err(ConfigError::MissingDirectory(path.display().to_string()));
            }
            write.push(path);
        }
        let network = match permissions.network.as_deref() {
            Some("none") => Network::None,
            Some("any") => Network::Any,
            Some(other) => return Err(ConfigError::BadNetwork(other.to_string())),
            None => return Err(ConfigError::Missing("tools[].permissions.network")),
        };
        let Some(memory_bytes) = permissions.memory_bytes.filter(|bytes| *bytes > 0) else {
            return Err(ConfigError::Zero("tools[].permissions.memory_bytes"));
        };
        let Some(file_bytes) = permissions.file_bytes.filter(|bytes| *bytes > 0) else {
            return Err(ConfigError::Zero("tools[].permissions.file_bytes"));
        };
        tools.push(Tool {
            argv: tool.argv,
            env: names.into_iter().collect(),
            file_bytes,
            idempotent,
            max_output,
            memory_bytes,
            name: tool.name,
            network,
            risk,
            confined,
            schema: tool.schema,
            timeout: Duration::from_millis(timeout_ms),
            write,
        });
    }

    Ok(Config {
        state_dir: PathBuf::from(state_dir),
        settings: Settings {
            approval_grace: Duration::from_secs(raw.approval_seconds.unwrap_or(600)),
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
risk = "read"
schema = '{"argv":"string"}'

[tools.permissions]
env = []
network = "none"
memory_bytes = 268435456
file_bytes = 8388608

[sandbox]
none = false
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

    /// The permissions every extra tool in these tests would otherwise repeat.
    const BOUNDS: &str = "network = \"none\"\nmemory_bytes = 268435456\nfile_bytes = 8388608\n";

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
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nclass = \"maybe\"\nname = \"other\"\nrisk = \"read\"\nschema = '{{}}'\n"
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::ToolClass("maybe".to_string()))
        );
    }

    #[test]
    fn a_tool_that_does_not_say_what_it_may_touch_is_refused() {
        let text = format!(
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nname = \"other\"\nschema = '{{}}'\n"
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Missing("tools[].risk"))
        );
    }

    #[test]
    fn a_risk_this_build_does_not_know_is_refused() {
        let text = format!(
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nname = \"other\"\nrisk = \"harmless\"\nschema = '{{}}'\n"
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Missing("tools[].risk"))
        );
    }

    #[test]
    fn a_risk_is_read_into_the_tool() {
        let text = format!(
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nname = \"other\"\nrisk = \"destructive\"\nschema = '{{}}'\n[tools.permissions]\nenv = []\n{BOUNDS}"
        );
        let config = parse(&text, &no_env).expect("parses");
        let other = config
            .settings
            .tools
            .iter()
            .find(|tool| tool.name == "other")
            .expect("the tool");
        assert_eq!(other.risk, agent_core::event::Risk::Destructive);
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
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nname = \"shell\"\nrisk = \"read\"\nschema = '{{}}'\n"
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::DuplicateTool("shell".to_string()))
        );
    }

    #[test]
    fn a_tool_with_no_name_is_refused() {
        let text = format!(
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nname = \"\"\nrisk = \"read\"\nschema = '{{}}'\n"
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Missing("tools[].name"))
        );
    }

    #[test]
    fn a_tool_that_does_not_say_what_it_may_see_is_refused() {
        let text = MINIMAL.replace("[tools.permissions]\nenv = []\n", "");
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Missing("tools[].permissions.env"))
        );

        let text = MINIMAL.replace("env = []\n", "");
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Missing("tools[].permissions.env"))
        );
    }

    #[test]
    fn a_name_that_is_not_an_environment_variable_name_is_refused() {
        let text = MINIMAL.replace("env = []", "env = [\"A=B\"]");
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::BadEnvName("A=B".to_string()))
        );
    }

    #[test]
    fn a_declared_variable_the_process_does_not_have_is_refused_by_name() {
        let text = MINIMAL.replace("env = []", "env = [\"SET\", \"UNSET\"]");
        let env = |name: &str| match name {
            "SET" => Some("s3cr3t".to_string()),
            _ => None,
        };
        match parse(&text, &env) {
            Err(ConfigError::EnvMissing(name)) => {
                assert_eq!(name, "UNSET");
                let message = ConfigError::EnvMissing(name).to_string();
                assert!(message.contains("UNSET"), "{message}");
                assert!(
                    !message.contains("s3cr3t"),
                    "the message carried a value: {message}"
                );
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn two_tools_that_declare_the_same_names_in_any_order_are_the_same() {
        let text = MINIMAL.replace("env = []", "env = [\"B\", \"A\"]")
            + "\n[[tools]]\nargv = [\"/bin/cat\"]\nname = \"other\"\nrisk = \"read\"\nschema = '{}'\n[tools.permissions]\nenv = [\"A\", \"B\", \"A\"]\n"
            + BOUNDS;
        let env = |name: &str| match name {
            "A" | "B" => Some("x".to_string()),
            _ => None,
        };
        let config = parse(&text, &env).expect("parses");
        assert_eq!(config.settings.tools[0].env, vec!["A", "B"]);
        assert_eq!(config.settings.tools[0].env, config.settings.tools[1].env);
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
            ConfigError::BadEnvName("A=B".to_string()),
            ConfigError::EnvMissing("HOME".to_string()),
        ] {
            assert_ne!(error.to_string(), "");
        }
    }

    #[test]
    fn a_tool_is_bounded_unless_the_file_says_in_as_many_words_that_it_is_not() {
        let bounded = parse(MINIMAL, &no_env).expect("parses");
        assert!(bounded.settings.tools[0].confined, "the default is bounded");

        let without = MINIMAL
            .split("[sandbox]")
            .next()
            .expect("a sandbox section")
            .to_string();
        let default = parse(&without, &no_env).expect("a file that says nothing is bounded");
        assert!(default.settings.tools[0].confined);

        let unbounded = MINIMAL.replace("none = false", "none = true");
        let read = parse(&unbounded, &no_env).expect("parses");
        assert!(
            !read.settings.tools[0].confined,
            "the file said none, in as many words"
        );
    }

    #[test]
    fn a_path_a_tool_may_write_has_to_be_absolute() {
        let text = MINIMAL.replace(
            "network = \"none\"",
            "write = [\"relative/dir\"]\nnetwork = \"none\"",
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::BadPath("relative/dir".to_string()))
        );
    }

    #[test]
    fn the_network_is_one_of_two_answers() {
        let text = MINIMAL.replace("network = \"none\"", "network = \"allowlist\"");
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::BadNetwork("allowlist".to_string()))
        );
    }

    #[test]
    fn a_bound_of_zero_would_mean_no_process_at_all() {
        let text = MINIMAL.replace("memory_bytes = 268435456", "memory_bytes = 0");
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Zero("tools[].permissions.memory_bytes"))
        );

        let text = MINIMAL.replace("file_bytes = 8388608", "file_bytes = 0");
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Zero("tools[].permissions.file_bytes"))
        );
    }

    #[test]
    fn every_tool_carries_the_launcher_that_bounds_it() {
        let config = parse(MINIMAL, &no_env).expect("parses");
        let tool = &config.settings.tools[0];
        assert!(tool.confined, "a tool is bounded unless the file says none");
        assert_eq!(tool.network, super::Network::None);
        assert_eq!(tool.memory_bytes, 268_435_456);
        assert_eq!(tool.file_bytes, 8_388_608);
        assert!(
            tool.write.is_empty(),
            "a tool writes nowhere it did not say"
        );
    }

    #[test]
    fn a_directory_a_tool_may_write_has_to_exist_before_the_daemon_starts() {
        let missing = std::env::temp_dir().join("agent-config-not-created");
        let _ = std::fs::remove_dir_all(&missing);
        let text = MINIMAL.replace(
            "network = \"none\"",
            &format!(
                "write = [{:?}]\nnetwork = \"none\"",
                missing.display().to_string()
            ),
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::MissingDirectory(missing.display().to_string()))
        );
    }
}
