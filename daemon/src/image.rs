//! The agent image: the template a session is created from.
//!
//! An image is data. A session is an instance. An image carries the program to
//! run, the workspace it acts on, and the policy inputs that turn into a
//! confinement policy. See `docs/session/agent.md`.

use std::path::{Path, PathBuf};

use sandbox::policy::{EnvVar, FsEntry, FsPolicy, HostPort, NetworkPolicy, Policy, ShellPolicy};

use crate::session::SessionId;

/// The template a session is created from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentImage {
    /// The agent program, resolved on the trusted side.
    pub program: PathBuf,
    /// The program's arguments.
    pub args: Vec<String>,
    /// The destinations the agent may reach through the egress proxy. The zero
    /// list reaches nothing.
    pub rules: Vec<HostPort>,
    /// The environment variables the program receives. The host environment is
    /// never inherited; this is the whole allowlist.
    pub env: Vec<EnvVar>,
    /// Whether this image runs the project's agent, which discovers its session
    /// from the flags the daemon passes at launch. An image that is not one runs
    /// exactly the arguments it carries, which is what a script does.
    agent: bool,
}

impl AgentImage {
    /// An image for `program` with no arguments, no egress, and no environment.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            rules: Vec::new(),
            env: Vec::new(),
            agent: false,
        }
    }

    /// An image for the project's agent, running `program`.
    ///
    /// The agent is told where its session is at launch; see
    /// [`AgentImage::session_args`]. [`crate::settings`] builds one of these for a
    /// session whose settings name an endpoint.
    #[must_use]
    pub fn agent(program: impl Into<PathBuf>) -> Self {
        Self {
            agent: true,
            ..Self::new(program)
        }
    }

    /// Add an argument.
    #[must_use]
    pub fn with_arg(
        mut self,
        arg: impl Into<String>,
    ) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Allow the agent to reach `host` on port 443 through the proxy.
    #[must_use]
    pub fn allowing(
        mut self,
        host: impl Into<String>,
    ) -> Self {
        self.rules.push(HostPort::new(host, 443));
        self
    }

    /// Allow the agent to reach `destination` through the proxy.
    ///
    /// A destination carries its port, so an endpoint on a non-default port is a
    /// rule of its own. [`crate::settings`] derives one per declared endpoint.
    #[must_use]
    pub fn allowing_destination(
        mut self,
        destination: HostPort,
    ) -> Self {
        self.rules.push(destination);
        self
    }

    /// Give the agent an environment variable.
    #[must_use]
    pub fn with_env(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.env.push(EnvVar::new(name, value));
        self
    }

    /// Whether this image reaches the network at all.
    #[must_use]
    pub const fn grants_egress(&self) -> bool {
        !self.rules.is_empty()
    }

    /// Build the confinement policy for a session on `workspace`.
    ///
    /// The workspace is bound read-write and is the working directory; the
    /// agent's program directory is granted read-only, so a shell and the tooling
    /// beside the program resolve inside the sandbox. The bus socket is granted
    /// so the agent can subscribe, and `deny` masks everything under
    /// `deny_under`, which is where the authority socket and the daemon's own
    /// state live.
    ///
    /// The policy is validated here. The caller then adds the egress socket if
    /// the image grants egress, because that socket must exist first, and renders
    /// the result.
    ///
    /// # Errors
    ///
    /// Returns [`sandbox::policy::InvalidPolicy`] if the composed policy is not
    /// valid, which would mean a host path is malformed.
    pub fn policy(
        &self,
        workspace: &Path,
        bus_socket: &Path,
        deny_under: &[PathBuf],
    ) -> Result<Policy, sandbox::policy::InvalidPolicy> {
        let mut entries = vec![FsEntry::write(workspace)];

        // The program's directory, read-only: a resolved program and the tools
        // beside it must be reachable, but not writable by the agent.
        if let Some(parent) = self.program.parent() {
            entries.push(FsEntry::read(parent));
        }

        for path in deny_under {
            if path.exists() {
                entries.push(FsEntry::deny(path));
            }
        }

        let policy = Policy {
            fs: FsPolicy {
                entries,
                protected: FsPolicy::default().protected,
            },
            shell: ShellPolicy {
                env: self.env.clone(),
                workdir: workspace.to_path_buf(),
            },
            network: NetworkPolicy {
                unix_sockets: vec![bus_socket.to_path_buf()],
                loopback: false,
                proxy: None,
            },
            ..Policy::default()
        };
        policy.validate()?;
        Ok(policy)
    }

    /// The egress grant for this image: every rule it carries.
    #[must_use]
    pub fn egress_rules(&self) -> Vec<HostPort> {
        self.rules.clone()
    }

    /// The argv a session on `workspace` runs this image with.
    ///
    /// An image of the project's agent is told where its session is: the bus socket
    /// it subscribes on, the id that scopes its input and output, and the workspace
    /// its commands run in. Any other image runs exactly the arguments it carries,
    /// so a script is unaffected.
    #[must_use]
    pub fn session_args(
        &self,
        id: &SessionId,
        bus_socket: &Path,
        workspace: &Path,
    ) -> Vec<String> {
        let mut args = self.args.clone();
        if self.agent {
            args.extend([
                "--socket".to_owned(),
                bus_socket.display().to_string(),
                "--session".to_owned(),
                id.as_str().to_owned(),
                "--workdir".to_owned(),
                workspace.display().to_string(),
            ]);
        }
        args
    }
}

#[cfg(test)]
mod tests {
    // Tests for the image and the policy it builds. No host, no spawn.

    use super::*;

    fn image() -> AgentImage {
        AgentImage::new("/usr/bin/agent-agent")
    }

    #[test]
    fn a_bare_image_grants_no_egress() {
        assert!(!image().grants_egress());
        assert!(image().egress_rules().is_empty());
    }

    #[test]
    fn an_allowed_host_is_port_443() {
        let image = image().allowing("api.example.com");
        assert!(image.grants_egress());
        assert_eq!(
            image.egress_rules(),
            vec![HostPort::new("api.example.com", 443)]
        );
    }

    #[test]
    fn an_allowed_destination_keeps_its_port() {
        // A model endpoint on a non-default port is a different destination to
        // the proxy, so the port the settings derived must survive the image.
        let image = image().allowing_destination(HostPort::new("models.internal", 8443));
        assert_eq!(
            image.egress_rules(),
            vec![HostPort::new("models.internal", 8443)]
        );
    }

    #[test]
    fn an_agent_image_is_told_which_session_it_acts_for() {
        // The agent discovers its session from these flags, which the daemon passes
        // because the image is fixed before a session exists.
        let image = AgentImage::agent("/usr/bin/agent-agent");
        let id = SessionId::new("s-1").expect("valid id");
        assert_eq!(
            image.session_args(&id, Path::new("/run/bus.sock"), Path::new("/work/run-1")),
            [
                "--socket",
                "/run/bus.sock",
                "--session",
                "s-1",
                "--workdir",
                "/work/run-1"
            ]
        );
    }

    #[test]
    fn an_agent_image_keeps_the_arguments_it_carries() {
        let image = AgentImage::agent("/usr/bin/agent-agent")
            .with_arg("--from-seq")
            .with_arg("7");
        let id = SessionId::new("s-1").expect("valid id");
        let args = image.session_args(&id, Path::new("/run/bus.sock"), Path::new("/work/run-1"));
        assert_eq!(&args[..2], ["--from-seq", "7"]);
        assert_eq!(
            args.len(),
            8,
            "its own arguments come first, then the session's"
        );
    }

    #[test]
    fn a_script_image_runs_only_what_it_carries() {
        let image = AgentImage::new("/bin/sh")
            .with_arg("-c")
            .with_arg("echo hi");
        let id = SessionId::new("s-1").expect("valid id");
        assert_eq!(
            image.session_args(&id, Path::new("/run/bus.sock"), Path::new("/work/run-1")),
            ["-c", "echo hi"],
            "an image that is not the agent is not told about a session"
        );
    }

    #[test]
    fn the_policy_binds_the_workspace_read_write() {
        let image = image();
        let workspace = Path::new("/work/run-1");
        let policy = image
            .policy(workspace, Path::new("/run/bus.sock"), &[])
            .expect("valid");
        assert_eq!(
            policy.fs.access_for(workspace),
            sandbox::policy::Access::Write
        );
        assert_eq!(policy.shell.workdir, workspace);
    }

    #[test]
    fn the_policy_grants_the_bus_socket() {
        let image = image();
        let policy = image
            .policy(Path::new("/work/run-1"), Path::new("/run/bus.sock"), &[])
            .expect("valid");
        assert_eq!(
            policy.network.unix_sockets,
            vec![PathBuf::from("/run/bus.sock")]
        );
    }

    #[test]
    fn a_deny_under_path_is_masked() {
        // The authority socket's directory is masked when it exists.
        let image = image();
        let dir = std::env::temp_dir().join(format!("daemon-image-deny-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates the dir");
        let denied = dir.join("authority");
        std::fs::create_dir_all(&denied).expect("creates the denied dir");

        let policy = image
            .policy(
                Path::new("/work/run-1"),
                Path::new("/run/bus.sock"),
                std::slice::from_ref(&denied),
            )
            .expect("valid");
        assert_eq!(policy.fs.access_for(&denied), sandbox::policy::Access::Deny);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deny_under_path_that_does_not_exist_is_skipped() {
        // A missing path cannot be masked, and would make the policy invalid, so
        // it is left out rather than failing the session.
        let image = image();
        let missing = PathBuf::from("/nonexistent-authority-socket-dir");
        assert!(
            image
                .policy(
                    Path::new("/work/run-1"),
                    Path::new("/run/bus.sock"),
                    &[missing]
                )
                .is_ok()
        );
    }

    #[test]
    fn the_program_directory_is_read_only() {
        let image = AgentImage::new("/opt/agent/bin/agent-agent");
        let policy = image
            .policy(Path::new("/work/run-1"), Path::new("/run/bus.sock"), &[])
            .expect("valid");
        assert_eq!(
            policy
                .fs
                .access_for(Path::new("/opt/agent/bin/agent-agent")),
            sandbox::policy::Access::Read
        );
    }

    #[test]
    fn the_environment_is_the_image_allowlist() {
        let image = image().with_env("PATH", "/usr/bin");
        let policy = image
            .policy(Path::new("/work/run-1"), Path::new("/run/bus.sock"), &[])
            .expect("valid");
        assert_eq!(policy.shell.env, vec![EnvVar::new("PATH", "/usr/bin")]);
    }
}
