//! The child-side egress forwarder. See [`agentd_sandbox::forward`].

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (port, socket) = match agentd_sandbox::forward::parse_args(&args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("[agentd-egress-forward] {error}");
            return std::process::ExitCode::FAILURE;
        },
    };
    match agentd_sandbox::forward::run(port, &socket) {
        Ok(()) => std::process::ExitCode::SUCCESS,
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
