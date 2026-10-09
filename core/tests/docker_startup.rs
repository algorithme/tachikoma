use std::{
    net::TcpListener,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn missing_docker_reports_setup_instructions_before_binding_http() {
    // An occupied port exposes a regression that binds HTTP before checking Docker.
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();
    let name = format!("tachikoma-missing-{}", uuid::Uuid::new_v4());
    #[cfg(unix)]
    let endpoint = format!("unix:///tmp/{name}.sock");
    #[cfg(windows)]
    let endpoint = format!("npipe:////./pipe/{name}");
    let mut command = Command::new(env!("CARGO_BIN_EXE_tachikoma-core"));
    command
        .env_clear()
        .env("TACHIKOMA_PORT", port.to_string())
        .env("TACHIKOMA_DOCKER_HOST", &endpoint)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("core did not stop after the Docker prerequisite failed");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success());
    assert!(error.contains("Cannot reach local Docker"), "{error}");
    assert!(error.contains("Start Docker Engine or Docker Desktop"));
    assert!(!error.contains(&endpoint));
    assert!(!error.contains("Could not bind"));
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("listening")
    );
}
