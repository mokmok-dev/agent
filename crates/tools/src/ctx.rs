//! What one call needs beyond its arguments.

use std::ffi::{OsStr, OsString};

use crate::allow::Allowlist;
use crate::path::Workspace;

/// The workspace, the program list, and the environment one call runs with.
pub struct Ctx<'a> {
    workspace: &'a Workspace,
    allow: &'a Allowlist,
    env: Vec<(String, OsString)>,
}

impl<'a> Ctx<'a> {
    /// Reads the named variables from this process, and no others.
    #[must_use]
    pub fn capture(
        workspace: &'a Workspace,
        allow: &'a Allowlist,
        names: &[&str],
    ) -> Self {
        let env = names
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|value| ((*name).to_string(), value)))
            .collect();
        Self {
            workspace,
            allow,
            env,
        }
    }

    /// Builds a context with the environment a test supplies.
    #[must_use]
    pub const fn with_env(
        workspace: &'a Workspace,
        allow: &'a Allowlist,
        env: Vec<(String, OsString)>,
    ) -> Self {
        Self {
            workspace,
            allow,
            env,
        }
    }

    /// The directory every path is resolved against.
    #[must_use]
    pub const fn workspace(&self) -> &'a Workspace {
        self.workspace
    }

    /// The programs this invocation permits.
    #[must_use]
    pub const fn allow(&self) -> &'a Allowlist {
        self.allow
    }

    /// One environment value, when the tool has it.
    #[must_use]
    pub fn value(
        &self,
        name: &str,
    ) -> Option<&OsStr> {
        self.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_os_str())
    }
}
