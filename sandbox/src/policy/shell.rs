//! The shell domain: the environment a command receives and where it runs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{InvalidPolicy, path};

/// The command's environment and working directory.
///
/// The host environment is **never** inherited; `env` is the complete set a
/// command receives. The daemon adds the proxy variables when egress is
/// granted, in the executor milestone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellPolicy {
    /// The environment entries the command receives. Nothing is inherited.
    #[serde(default)]
    pub env: Vec<EnvVar>,
    /// The working directory, which must be inside a `write` entry.
    ///
    /// Defaults to `/`, which no policy grants a write to, so a policy that
    /// forgets it is rejected rather than silently run in the wrong place.
    #[serde(default = "default_workdir")]
    pub workdir: PathBuf,
}

impl Default for ShellPolicy {
    fn default() -> Self {
        Self {
            env: Vec::new(),
            workdir: default_workdir(),
        }
    }
}

/// The working directory a shell that omits one gets: the host root, which is
/// writable only if the policy explicitly grants it.
fn default_workdir() -> PathBuf {
    PathBuf::from("/")
}

/// One environment variable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvVar {
    /// The variable name, without a `=` or a NUL byte.
    pub name: String,
    /// The variable value, without a NUL byte.
    pub value: String,
}

impl EnvVar {
    /// A variable named `name` with the given `value`.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

impl ShellPolicy {
    /// Check the working directory and the environment entries.
    ///
    /// The rule that the working directory lies inside a `write` entry is a
    /// property of the whole policy, so [`Policy`](super::Policy) checks it.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidPolicy`] for the first violation found.
    pub fn validate(&self) -> Result<(), InvalidPolicy> {
        path::check_absolute_normalized(&self.workdir)?;
        for var in &self.env {
            validate_env_var(var)?;
        }
        Ok(())
    }
}

/// Check one environment variable's name and value.
fn validate_env_var(var: &EnvVar) -> Result<(), InvalidPolicy> {
    let name_is_invalid = var.name.is_empty() || var.name.contains('=');
    if name_is_invalid {
        return Err(InvalidPolicy::InvalidEnvName {
            name: var.name.clone(),
        });
    }
    if var.name.contains('\0') || var.value.contains('\0') {
        return Err(InvalidPolicy::InvalidEnvValue {
            name: var.name.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Tests for the shell domain: the environment is an allowlist, so each entry
    // is validated, and the working directory must be absolute and normalized.

    use super::*;

    #[test]
    fn the_default_shell_has_no_environment() {
        let shell = ShellPolicy::default();
        assert!(shell.env.is_empty());
        assert!(shell.validate().is_ok());
    }

    #[test]
    fn an_empty_name_is_rejected() {
        let shell = ShellPolicy {
            env: vec![EnvVar::new("", "value")],
            ..ShellPolicy::default()
        };
        assert!(matches!(
            shell.validate(),
            Err(InvalidPolicy::InvalidEnvName { .. })
        ));
    }

    #[test]
    fn a_name_with_an_equals_sign_is_rejected() {
        let shell = ShellPolicy {
            env: vec![EnvVar::new("A=B", "value")],
            ..ShellPolicy::default()
        };
        assert!(matches!(
            shell.validate(),
            Err(InvalidPolicy::InvalidEnvName { .. })
        ));
    }

    #[test]
    fn a_name_or_value_with_a_nul_is_rejected() {
        let name = ShellPolicy {
            env: vec![EnvVar::new("A\0B", "value")],
            ..ShellPolicy::default()
        };
        assert!(matches!(
            name.validate(),
            Err(InvalidPolicy::InvalidEnvValue { .. })
        ));

        let value = ShellPolicy {
            env: vec![EnvVar::new("A", "va\0lue")],
            ..ShellPolicy::default()
        };
        assert!(matches!(
            value.validate(),
            Err(InvalidPolicy::InvalidEnvValue { .. })
        ));
    }

    #[test]
    fn an_equals_sign_in_the_value_is_allowed() {
        let shell = ShellPolicy {
            env: vec![EnvVar::new("FLAGS", "--a=b")],
            ..ShellPolicy::default()
        };
        assert!(shell.validate().is_ok());
    }

    #[test]
    fn a_relative_working_directory_is_rejected() {
        let shell = ShellPolicy {
            env: Vec::new(),
            workdir: PathBuf::from("work"),
        };
        assert!(matches!(
            shell.validate(),
            Err(InvalidPolicy::NotAbsolute { .. })
        ));
    }
}
