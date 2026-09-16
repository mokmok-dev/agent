#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    agentd_sandbox::helper::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("[agentd-sandbox-helper] the Landlock helper is implemented for Linux only");
    std::process::exit(1);
}
