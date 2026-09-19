//! The child-side egress forwarder. See [`agentd_sandbox::forward`].

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let invocation = match agentd_sandbox::forward::parse_args(&args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("[agentd-egress-forward] {error}");
            return std::process::ExitCode::FAILURE;
        },
    };
    let result = if invocation.command.is_empty() {
        agentd_sandbox::forward::run(invocation.port, &invocation.socket).map(|()| 0)
    } else {
        agentd_sandbox::forward::run_command(
            invocation.port,
            &invocation.socket,
            &invocation.command,
        )
    };
    match result {
        Ok(code) => std::process::ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("[agentd-egress-forward] {error}");
            std::process::ExitCode::FAILURE
        },
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("[agentd-egress-forward] the forwarder is implemented for unix only");
    std::process::exit(1);
}
