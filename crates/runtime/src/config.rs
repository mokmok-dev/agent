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

/// The address space a first-party tool gets unless the operator says otherwise.
const DEFAULT_TOOL_MEMORY: u64 = 256 * 1024 * 1024;

/// The largest file a first-party tool gets unless the operator says otherwise.
const DEFAULT_TOOL_FILE: u64 = 8 * 1024 * 1024;

/// A build needs more room than a file reader.
const RUN_TOOL_MEMORY: u64 = 2 * 1024 * 1024 * 1024;
const RUN_TOOL_FILE: u64 = 512 * 1024 * 1024;

/// The most one event may carry, and the cap an override is held to.
const OUTPUT_CAP: usize = 1024 * 1024;

/// The most address space one tool may be given.
const MEMORY_CAP: u64 = 16 * 1024 * 1024 * 1024;

/// The largest file one tool may write.
const FILE_CAP: u64 = 1024 * 1024 * 1024;

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
    /// A configuration the preset rules refuse, in the words the operator reads.
    Message(String),
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
            Self::Message(text) => formatter.write_str(text),
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
    workspace: Option<String>,
    #[serde(default)]
    tools: Vec<RawTool>,
    #[serde(default)]
    tool_limits: std::collections::BTreeMap<String, RawLimits>,
    #[serde(default)]
    sandbox: Option<RawSandbox>,
}

#[derive(Debug, Deserialize)]
struct RawTool {
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    schema: Option<String>,
    #[serde(default)]
    risk: Option<String>,
    #[serde(default)]
    programs: Option<Vec<String>>,
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

#[derive(Debug, Default, Deserialize)]
struct RawLimits {
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_output_bytes: Option<usize>,
    #[serde(default)]
    memory_bytes: Option<u64>,
    #[serde(default)]
    file_bytes: Option<u64>,
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

    let state_dir = PathBuf::from(state_dir);
    let workspace = resolve_workspace(raw.workspace, &state_dir, !raw.tools.is_empty())?;
    let tools = check_tools(
        raw.tools,
        &state_dir,
        &workspace,
        env,
        confined,
        &raw.tool_limits,
    )?;

    Ok(Config {
        state_dir,
        settings: Settings {
            approval_grace: Duration::from_secs(raw.approval_seconds.unwrap_or(600)),
            model,
            directive_version,
            directive,
            workspace,
            tools,
            max_output_tokens,
        },
        base_url,
        api_key,
        timeout: Duration::from_secs(timeout_seconds),
    })
}

/// The directory tools run in, created when it is inside the state directory.
fn resolve_workspace(
    raw: Option<String>,
    state_dir: &Path,
    tools: bool,
) -> Result<PathBuf, ConfigError> {
    let Some(raw) = raw else {
        if tools {
            return Err(ConfigError::Missing("workspace"));
        }
        return Ok(state_dir.to_path_buf());
    };
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(ConfigError::BadPath(path.display().to_string()));
    }
    if !path.exists() {
        if path.starts_with(state_dir) {
            std::fs::create_dir_all(&path)
                .map_err(|_| ConfigError::MissingDirectory(path.display().to_string()))?;
        } else {
            return Err(ConfigError::MissingDirectory(path.display().to_string()));
        }
    }
    if !path.is_dir() {
        return Err(ConfigError::MissingDirectory(path.display().to_string()));
    }
    Ok(path)
}

/// Turns every `[[tools]]` entry into the tool the runtime runs.
struct Deployment<'a> {
    state_dir: &'a Path,
    workspace: &'a Path,
    env: &'a dyn Fn(&str) -> Option<String>,
    confined: bool,
}

fn check_tools(
    raw: Vec<RawTool>,
    state_dir: &Path,
    workspace: &Path,
    env: &dyn Fn(&str) -> Option<String>,
    confined: bool,
    limits: &std::collections::BTreeMap<String, RawLimits>,
) -> Result<Vec<Tool>, ConfigError> {
    let deployment = Deployment {
        state_dir,
        workspace,
        env,
        confined,
    };
    let mut seen = BTreeSet::new();
    let mut tools = Vec::with_capacity(raw.len());
    for tool in raw {
        let name = match (&tool.preset, &tool.name) {
            (Some(preset), None) => preset.clone(),
            (Some(preset), Some(_)) => {
                return Err(ConfigError::Message(format!(
                    "{preset} is a preset and does not take its own fields"
                )));
            },
            (None, Some(name)) if !name.is_empty() => name.clone(),
            (None, _) => return Err(ConfigError::Missing("tools[].name")),
        };
        if !seen.insert(name.clone()) {
            return Err(ConfigError::DuplicateTool(name));
        }
        let limit = limits.get(&name);
        let preset = tool.preset.clone();
        let built = if let Some(preset) = preset {
            let Some(spec) = agent_tools::registry()
                .iter()
                .find(|spec| spec.name == preset)
            else {
                return Err(ConfigError::Message(format!(
                    "there is no tool named {preset}"
                )));
            };
            preset_tool(&name, spec, tool, &deployment, limit)?
        } else {
            manifest_tool(name, tool, &deployment)?
        };
        tools.push(built);
    }
    Ok(tools)
}

/// Builds one first-party tool from its registry entry.
fn preset_tool(
    name: &str,
    spec: &'static agent_tools::RegisteredTool,
    tool: RawTool,
    deployment: &Deployment<'_>,
    limits: Option<&RawLimits>,
) -> Result<Tool, ConfigError> {
    preset_fields(name, &tool)?;
    let risk = preset_risk(spec, tool.risk.as_deref(), name)?;
    let (timeout_ms, max_output, memory_bytes, file_bytes) = preset_bounds(name, spec, limits)?;
    let programs = preset_programs(spec, tool.programs, name)?;
    let mut argv = vec![
        tool_binary(deployment.env)?.display().to_string(),
        name.to_string(),
    ];
    for program in &programs {
        argv.push(format!("--allow={program}"));
    }
    let write = preset_write(spec, deployment)?;
    let names = preset_env(spec, deployment)?;
    Ok(Tool {
        argv,
        description: spec.description.to_string(),
        env: names,
        file_bytes,
        idempotent: spec.class == agent_tools::Class::Idempotent,
        max_output,
        memory_bytes,
        name: name.to_string(),
        network: Network::None,
        risk,
        confined: deployment.confined,
        schema: serde_json::to_string(&(spec.parameters)()).map_err(|_| {
            ConfigError::Message(format!("the schema of {name} is not a JSON object"))
        })?,
        timeout: Duration::from_millis(timeout_ms),
        write,
    })
}

fn preset_fields(
    name: &str,
    tool: &RawTool,
) -> Result<(), ConfigError> {
    let present = tool.name.is_some()
        || tool.description.is_some()
        || tool.schema.is_some()
        || !tool.argv.is_empty()
        || tool.class.is_some()
        || tool.timeout_ms.is_some()
        || tool.max_output_bytes.is_some()
        || tool.permissions.is_some();
    if present {
        return Err(ConfigError::Message(format!(
            "{name} is a preset and does not take its own fields"
        )));
    }
    Ok(())
}

fn preset_risk(
    spec: &agent_tools::RegisteredTool,
    word: Option<&str>,
    name: &str,
) -> Result<Risk, ConfigError> {
    let Some(word) = word else {
        return Ok(spec.risk);
    };
    let risk = parse_risk(Some(word))?;
    if risk < spec.risk {
        return Err(ConfigError::Message(format!(
            "the risk of {name} cannot be lower than its preset"
        )));
    }
    Ok(risk)
}

fn preset_bounds(
    name: &str,
    spec: &agent_tools::RegisteredTool,
    limits: Option<&RawLimits>,
) -> Result<(u64, usize, u64, u64), ConfigError> {
    let timeout_ms = limits
        .and_then(|limits| limits.timeout_ms)
        .unwrap_or(spec.limits.timeout_ms);
    if timeout_ms == 0 || timeout_ms > agent_tools::TIMEOUT_CAP_MS {
        return Err(ConfigError::Message(format!(
            "timeout_ms is outside the range for {name}"
        )));
    }
    let max_output = limits
        .and_then(|limits| limits.max_output_bytes)
        .unwrap_or(spec.limits.event_bytes);
    if max_output == 0 || max_output > OUTPUT_CAP {
        return Err(ConfigError::Message(format!(
            "max_output_bytes is outside the range for {name}"
        )));
    }
    let (memory_bytes, file_bytes) = preset_limits(name, limits);
    Ok((timeout_ms, max_output, memory_bytes, file_bytes))
}

fn preset_programs(
    spec: &agent_tools::RegisteredTool,
    programs: Option<Vec<String>>,
    name: &str,
) -> Result<Vec<String>, ConfigError> {
    match spec.programs {
        agent_tools::Programs::None => {
            if programs.is_some() {
                return Err(ConfigError::Message(format!(
                    "{name} starts no programs and takes no list"
                )));
            }
            Ok(Vec::new())
        },
        agent_tools::Programs::Listed => {
            let programs = programs.ok_or_else(|| {
                ConfigError::Message(format!("{name} starts programs and needs a program list"))
            })?;
            for program in &programs {
                check_program(program)?;
            }
            Ok(programs)
        },
    }
}

fn preset_write(
    spec: &agent_tools::RegisteredTool,
    deployment: &Deployment<'_>,
) -> Result<Vec<PathBuf>, ConfigError> {
    match spec.scope {
        agent_tools::Scope::Read => Ok(Vec::new()),
        agent_tools::Scope::Workspace => Ok(vec![deployment.workspace.to_path_buf()]),
        agent_tools::Scope::WorkspaceAndScratch => {
            let home = deployment.state_dir.join("tool-home");
            std::fs::create_dir_all(&home)
                .map_err(|_| ConfigError::MissingDirectory(home.display().to_string()))?;
            Ok(vec![
                deployment.workspace.to_path_buf(),
                home,
                PathBuf::from("/tmp"),
            ])
        },
    }
}

fn preset_env(
    spec: &agent_tools::RegisteredTool,
    deployment: &Deployment<'_>,
) -> Result<Vec<String>, ConfigError> {
    let mut names = Vec::with_capacity(spec.env.len());
    for name in spec.env {
        if (deployment.env)(name).is_none() {
            return Err(ConfigError::EnvMissing((*name).to_string()));
        }
        names.push((*name).to_string());
    }
    Ok(names)
}

/// Builds one operator-written tool, with every field its own statement.
fn manifest_tool(
    name: String,
    tool: RawTool,
    deployment: &Deployment<'_>,
) -> Result<Tool, ConfigError> {
    if tool.programs.is_some() {
        return Err(ConfigError::Message(format!(
            "{name} starts no programs and takes no list"
        )));
    }
    let schema = manifest_schema(tool.schema, &name)?;
    if tool.argv.is_empty() {
        return Err(ConfigError::Missing("tools[].argv"));
    }
    let risk = parse_risk(tool.risk.as_deref())?;
    let idempotent = manifest_class(tool.class.as_deref())?;
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
    let permissions = manifest_permissions(permissions, deployment)?;
    Ok(Tool {
        argv: tool.argv,
        description: tool.description.unwrap_or_default(),
        env: permissions.env,
        file_bytes: permissions.file_bytes,
        idempotent,
        max_output,
        memory_bytes: permissions.memory_bytes,
        name,
        network: permissions.network,
        risk,
        confined: deployment.confined,
        schema,
        timeout: Duration::from_millis(timeout_ms),
        write: permissions.write,
    })
}

struct ManifestPermissions {
    env: Vec<String>,
    write: Vec<PathBuf>,
    network: Network,
    memory_bytes: u64,
    file_bytes: u64,
}

fn manifest_schema(
    schema: Option<String>,
    name: &str,
) -> Result<String, ConfigError> {
    let schema = schema.ok_or(ConfigError::Missing("tools[].schema"))?;
    let value: serde_json::Value = serde_json::from_str(&schema)
        .map_err(|_| ConfigError::Message(format!("the schema of {name} is not a JSON object")))?;
    if !value.is_object() {
        return Err(ConfigError::Message(format!(
            "the schema of {name} is not a JSON object"
        )));
    }
    Ok(schema)
}

fn manifest_class(class: Option<&str>) -> Result<bool, ConfigError> {
    match class {
        Some("idempotent") => Ok(true),
        Some("non-idempotent") => Ok(false),
        Some(other) => Err(ConfigError::ToolClass(other.to_string())),
        None => Err(ConfigError::Missing("tools[].class")),
    }
}

fn manifest_permissions(
    permissions: RawPermissions,
    deployment: &Deployment<'_>,
) -> Result<ManifestPermissions, ConfigError> {
    let Some(declared) = permissions.env else {
        return Err(ConfigError::Missing("tools[].permissions.env"));
    };
    let mut names = BTreeSet::new();
    for name in declared {
        if name.is_empty() || name.contains(['=', '\0']) {
            return Err(ConfigError::BadEnvName(name));
        }
        if (deployment.env)(&name).is_none() {
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
    Ok(ManifestPermissions {
        env: names.into_iter().collect(),
        write,
        network,
        memory_bytes,
        file_bytes,
    })
}

fn parse_risk(value: Option<&str>) -> Result<Risk, ConfigError> {
    match value {
        Some("read") => Ok(Risk::Read),
        Some("write") => Ok(Risk::Write),
        Some("network") => Ok(Risk::Network),
        Some("destructive") => Ok(Risk::Destructive),
        Some(_) | None => Err(ConfigError::Missing("tools[].risk")),
    }
}

fn check_program(program: &str) -> Result<(), ConfigError> {
    if program.is_empty()
        || program.contains(['=', '\0'])
        || (program.contains('/') && !Path::new(program).is_absolute())
    {
        return Err(ConfigError::Message(format!(
            "{program} is not a program name or an absolute path"
        )));
    }
    Ok(())
}

fn preset_limits(
    name: &str,
    limits: Option<&RawLimits>,
) -> (u64, u64) {
    let memory = limits
        .and_then(|limits| limits.memory_bytes)
        .unwrap_or(if name == "run" {
            RUN_TOOL_MEMORY
        } else {
            DEFAULT_TOOL_MEMORY
        });
    let file = limits
        .and_then(|limits| limits.file_bytes)
        .unwrap_or(if name == "run" {
            RUN_TOOL_FILE
        } else {
            DEFAULT_TOOL_FILE
        });
    (memory.min(MEMORY_CAP), file.min(FILE_CAP))
}

/// The `agent-tool` binary, beside the daemon or named by the environment.
fn tool_binary(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf, ConfigError> {
    if let Some(named) = env("MOKMOK_TOOL") {
        return Ok(PathBuf::from(named));
    }
    let exe = std::env::current_exe().map_err(|_| {
        ConfigError::Message(String::from(
            "agent-tool is not beside the daemon and MOKMOK_TOOL names nothing",
        ))
    })?;
    let beside = exe.with_file_name("agent-tool");
    if beside.is_file() {
        Ok(beside)
    } else {
        Err(ConfigError::Message(String::from(
            "agent-tool is not beside the daemon and MOKMOK_TOOL names nothing",
        )))
    }
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
workspace = "/tmp"

[[tools]]
argv = ["/bin/cat"]
class = "idempotent"
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

    fn preset_env(name: &str) -> Option<String> {
        match name {
            "MOKMOK_TOOL" => Some("/bin/true".to_string()),
            "PATH" | "HOME" | "TMPDIR" | "LANG" => Some("/tmp".to_string()),
            _ => std::env::var(name).ok(),
        }
    }

    #[test]
    fn a_preset_becomes_the_tool_the_registry_declares() {
        let text = format!("{MINIMAL}\n[[tools]]\npreset = \"write_file\"\n");
        let config = parse(&text, &preset_env).expect("parses");
        let tool = config
            .settings
            .tools
            .iter()
            .find(|tool| tool.name == "write_file")
            .expect("the preset");
        assert!(!tool.description.is_empty(), "the model needs a sentence");
        assert!(tool.schema.starts_with('{'), "{}", tool.schema);
        assert_eq!(
            tool.argv,
            vec!["/bin/true".to_string(), "write_file".to_string()]
        );
        assert_eq!(tool.risk, agent_core::event::Risk::Write);
        assert!(tool.idempotent);
        assert_eq!(tool.write, vec![std::path::PathBuf::from("/tmp")]);
    }

    #[test]
    fn a_run_preset_carries_its_programs_and_its_scratch_roots() {
        let state = std::env::temp_dir().join(format!("agent-preset-{}", std::process::id()));
        std::fs::create_dir_all(&state).expect("creates the state directory");
        let text = MINIMAL.replace(
            "state_dir = \"/var/lib/agent\"",
            &format!("state_dir = {:?}", state.display().to_string()),
        ) + "\n[[tools]]\npreset = \"run\"\nprograms = [\"cargo\", \"git\"]\n";
        let config = parse(&text, &preset_env).expect("parses");
        let run = config
            .settings
            .tools
            .iter()
            .find(|tool| tool.name == "run")
            .expect("the preset");
        assert_eq!(run.argv[1], "run");
        assert!(run.argv.contains(&"--allow=cargo".to_string()), "{run:?}");
        assert!(run.argv.contains(&"--allow=git".to_string()), "{run:?}");
        assert!(run.write.contains(&state.join("tool-home")), "{run:?}");
        assert!(run.write.contains(&std::path::PathBuf::from("/tmp")));
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_run_preset_without_programs_is_refused() {
        let text = format!("{MINIMAL}\n[[tools]]\npreset = \"run\"\n");
        assert_eq!(
            parse(&text, &preset_env),
            Err(ConfigError::Message(
                "run starts programs and needs a program list".to_string()
            ))
        );
    }

    #[test]
    fn a_preset_takes_no_schema_of_its_own() {
        let text = format!("{MINIMAL}\n[[tools]]\npreset = \"read_file\"\nschema = '{{}}'\n");
        assert_eq!(
            parse(&text, &preset_env),
            Err(ConfigError::Message(
                "read_file is a preset and does not take its own fields".to_string()
            ))
        );
    }

    #[test]
    fn a_preset_risk_cannot_be_lowered() {
        let text = format!(
            "{MINIMAL}\n[[tools]]\npreset = \"run\"\nprograms = [\"cargo\"]\nrisk = \"read\"\n"
        );
        assert_eq!(
            parse(&text, &preset_env),
            Err(ConfigError::Message(
                "the risk of run cannot be lower than its preset".to_string()
            ))
        );
    }

    #[test]
    fn a_relative_program_is_refused() {
        let text = format!("{MINIMAL}\n[[tools]]\npreset = \"run\"\nprograms = [\"bin/tool\"]\n");
        assert_eq!(
            parse(&text, &preset_env),
            Err(ConfigError::Message(
                "bin/tool is not a program name or an absolute path".to_string()
            ))
        );
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
        assert!(config.settings.tools[0].idempotent, "the fixture states it");
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
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nclass = \"idempotent\"\nname = \"other\"\nschema = '{{}}'\n"
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Missing("tools[].risk"))
        );
    }

    #[test]
    fn a_risk_this_build_does_not_know_is_refused() {
        let text = format!(
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nclass = \"idempotent\"\nname = \"other\"\nrisk = \"harmless\"\nschema = '{{}}'\n"
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::Missing("tools[].risk"))
        );
    }

    #[test]
    fn a_risk_is_read_into_the_tool() {
        let text = format!(
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nclass = \"idempotent\"\nname = \"other\"\nrisk = \"destructive\"\nschema = '{{}}'\n[tools.permissions]\nenv = []\n{BOUNDS}"
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
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nclass = \"idempotent\"\nname = \"shell\"\nrisk = \"read\"\nschema = '{{}}'\n"
        );
        assert_eq!(
            parse(&text, &no_env),
            Err(ConfigError::DuplicateTool("shell".to_string()))
        );
    }

    #[test]
    fn a_tool_with_no_name_is_refused() {
        let text = format!(
            "{MINIMAL}\n[[tools]]\nargv = [\"/bin/cat\"]\nclass = \"idempotent\"\nname = \"\"\nrisk = \"read\"\nschema = '{{}}'\n"
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
            + "\n[[tools]]\nargv = [\"/bin/cat\"]\nclass = \"idempotent\"\nname = \"other\"\nrisk = \"read\"\nschema = '{}'\n[tools.permissions]\nenv = [\"A\", \"B\", \"A\"]\n"
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
            ConfigError::Message("x".to_string()),
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

    fn named<'a>(
        config: &'a super::Config,
        name: &str,
    ) -> &'a crate::Tool {
        config
            .settings
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .expect("the tool is in the configuration")
    }

    fn preset(name: &str) -> String {
        format!("{MINIMAL}\n[[tools]]\npreset = \"{name}\"\n")
    }

    /// A configuration whose state directory is one a test may write in.
    fn writable(name: &str) -> (String, std::path::PathBuf) {
        let state =
            std::env::temp_dir().join(format!("agent-config-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).expect("creates the state directory");
        let text = MINIMAL.replace(
            "state_dir = \"/var/lib/agent\"",
            &format!("state_dir = {:?}", state.display().to_string()),
        );
        (text, state)
    }

    #[test]
    fn a_manifest_tool_gets_the_bounds_the_cap_table_names() {
        let config = parse(MINIMAL, &no_env).expect("parses");
        let tool = &config.settings.tools[0];
        assert_eq!(tool.memory_bytes, 268_435_456);
        assert_eq!(tool.file_bytes, 8_388_608);
        assert_eq!(tool.max_output, 65_536);
        assert_eq!(tool.timeout, Duration::from_secs(30));
    }

    #[test]
    fn a_build_gets_more_room_than_a_reader() {
        let (base, state) = writable("run-bounds");
        let text = format!(
            "{base}\n[[tools]]\npreset = \"run\"\nprograms = [\"sh\"]\n[[tools]]\npreset = \"read_file\"\n"
        );
        let config = parse(&text, &preset_env).expect("parses");
        let run = named(&config, "run");
        assert_eq!(run.memory_bytes, 2_147_483_648);
        assert_eq!(run.file_bytes, 536_870_912);
        let read = named(&config, "read_file");
        assert_eq!(read.memory_bytes, 268_435_456);
        assert_eq!(read.file_bytes, 8_388_608);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_bound_over_its_cap_is_held_to_the_cap() {
        let text = format!(
            "{}[tool_limits.read_file]\nmemory_bytes = 1099511627776\nfile_bytes = 1099511627776\n",
            preset("read_file")
        );
        let config = parse(&text, &preset_env).expect("parses");
        let tool = named(&config, "read_file");
        assert_eq!(tool.memory_bytes, 17_179_869_184);
        assert_eq!(tool.file_bytes, 1_073_741_824);
    }

    #[test]
    fn an_event_cap_at_the_hard_limit_is_allowed_and_one_byte_over_is_not() {
        let allowed = format!(
            "{}[tool_limits.read_file]\nmax_output_bytes = 1048576\n",
            preset("read_file")
        );
        let config = parse(&allowed, &preset_env).expect("the cap itself is allowed");
        assert_eq!(named(&config, "read_file").max_output, 1_048_576);

        let over = format!(
            "{}[tool_limits.read_file]\nmax_output_bytes = 1048577\n",
            preset("read_file")
        );
        assert!(parse(&over, &preset_env).is_err(), "one byte over the cap");
    }

    #[test]
    fn a_timeout_at_the_cap_is_allowed_and_the_edges_are_not() {
        let allowed = format!(
            "{}[tool_limits.read_file]\ntimeout_ms = 3600000\n",
            preset("read_file")
        );
        let config = parse(&allowed, &preset_env).expect("the cap itself is allowed");
        assert_eq!(
            named(&config, "read_file").timeout,
            Duration::from_millis(3_600_000)
        );

        for timeout in ["0", "3600001"] {
            let text = format!(
                "{}[tool_limits.read_file]\ntimeout_ms = {timeout}\n",
                preset("read_file")
            );
            assert!(parse(&text, &preset_env).is_err(), "{timeout} was allowed");
        }
    }

    #[test]
    fn a_preset_refuses_every_field_it_does_not_take() {
        let expected = Err(ConfigError::Message(
            "read_file is a preset and does not take its own fields".to_string(),
        ));
        for extra in [
            "description = \"mine\"",
            "class = \"idempotent\"",
            "timeout_ms = 1000",
            "max_output_bytes = 1024",
            "[tools.permissions]\nenv = []\nnetwork = \"none\"\nmemory_bytes = 268435456\nfile_bytes = 8388608",
        ] {
            let text = format!("{MINIMAL}\n[[tools]]\npreset = \"read_file\"\n{extra}\n");
            assert_eq!(parse(&text, &preset_env), expected.clone(), "{extra}");
        }
    }

    #[test]
    fn a_preset_risk_may_be_kept_or_raised() {
        let (base, state) = writable("run-risk");
        let same =
            format!("{base}\n[[tools]]\npreset = \"run\"\nprograms = [\"sh\"]\nrisk = \"write\"\n");
        let config = parse(&same, &preset_env).expect("the preset's own risk is not a lowering");
        assert_eq!(named(&config, "run").risk, agent_core::event::Risk::Write);

        let raised = format!(
            "{base}\n[[tools]]\npreset = \"run\"\nprograms = [\"sh\"]\nrisk = \"destructive\"\n"
        );
        let config = parse(&raised, &preset_env).expect("a higher risk is allowed");
        assert_eq!(
            named(&config, "run").risk,
            agent_core::event::Risk::Destructive
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_preset_carries_the_environment_names_its_definition_declares() {
        let (base, state) = writable("run-env");
        let text = format!("{base}\n[[tools]]\npreset = \"run\"\nprograms = [\"sh\"]\n");
        let config = parse(&text, &preset_env).expect("parses");
        assert_eq!(
            named(&config, "run").env,
            ["PATH", "HOME", "TMPDIR", "LANG"]
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn a_manifest_tool_keeps_the_schema_it_was_given() {
        let config = parse(MINIMAL, &no_env).expect("parses");
        assert_eq!(config.settings.tools[0].schema, r#"{"argv":"string"}"#);
    }

    #[test]
    fn a_program_name_with_an_equals_sign_or_no_text_is_refused() {
        for program in ["", "a=b", "/bin/a=b"] {
            let text =
                format!("{MINIMAL}\n[[tools]]\npreset = \"run\"\nprograms = [{program:?}]\n");
            assert!(
                parse(&text, &preset_env).is_err(),
                "{program:?} was taken as a program name"
            );
        }
    }
}
