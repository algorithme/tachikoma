use super::*;

fn code(source: &str) -> CodeExecutionRequest {
    CodeExecutionRequest {
        code: source.to_owned(),
    }
}

async fn live() -> AgentRuntime {
    let config =
        crate::common::config::Config::from_env().expect("valid local Docker configuration");
    AgentRuntime::create(&config.docker)
        .await
        .expect("live Docker prerequisites and controls are required")
}

#[tokio::test]
#[ignore = "requires local Linux Docker and the approved native image"]
async fn docker_arithmetic_streams_and_python_errors() {
    let runtime = live().await;
    println!(
        "Verified Python {} with {:?}",
        runtime.profile().python_version,
        runtime.profile().modules
    );
    let result = runtime
        .execute_code(code("print(sum(i*i for i in range(1, 10001)))"))
        .await
        .unwrap();
    assert_eq!(result.std_out, "333383335000\n");
    assert_eq!(result.std_err, "");
    let result = runtime
        .execute_code(code(
            "import sys; print('Grüezi 🐍'); print('error', file=sys.stderr)",
        ))
        .await
        .unwrap();
    assert_eq!(result.std_out, "Grüezi 🐍\n");
    assert_eq!(result.std_err, "error\n");
    let result = runtime.execute_code(code("")).await.unwrap();
    assert_eq!(
        result,
        CodeExecutionResult {
            std_out: String::new(),
            std_err: String::new()
        }
    );
    for (source, kind) in [("1/0", "ZeroDivisionError"), ("if", "SyntaxError")] {
        let result = runtime.execute_code(code(source)).await.unwrap();
        assert!(result.std_err.starts_with(kind));
        assert!(!result.std_err.contains("Traceback"));
    }
    runtime.close().await.unwrap();
    assert_eq!(
        runtime.execute_code(code("print(1)")).await,
        Err(ExecutionError::Closed)
    );
}

#[tokio::test]
#[ignore = "requires local Linux Docker and the approved native image"]
async fn docker_fresh_workers_retained_files_isolation_and_serial_calls() {
    let runtime = live().await;
    runtime.execute_code(code("import math, signal\nx = 123\nmath.pi = 0\nsignal.signal(signal.SIGUSR1, lambda *a: None)\nopen('kept', 'w').write('session data')")).await.unwrap();
    let result = runtime.execute_code(code("import math, signal\nassert 'x' not in globals()\nassert math.pi > 3\nassert signal.getsignal(signal.SIGUSR1) == signal.SIG_DFL\nprint(open('kept').read())")).await.unwrap();
    assert_eq!(result.std_out, "session data\n");
    assert_eq!(result.std_err, "");
    runtime
        .execute_code(code("open('after-error', 'w').write('saved')\n1/0"))
        .await
        .unwrap();
    let other = live().await;
    let result = other
        .execute_code(code(
            "import os\nprint(os.path.exists('kept'), os.path.exists('after-error'))",
        ))
        .await
        .unwrap();
    assert_eq!(result.std_out, "False False\n");
    other.close().await.unwrap();
    runtime.execute_code(code("for name in ['json', 'ctypes', 'sitecustomize', 'usercustomize', 'math']:\n open(name + '.py', 'w').write('raise Exception(\"poisoned\")')")).await.unwrap();
    let result = runtime.execute_code(code("import json, ctypes, math\nprint(json.dumps(math.sqrt(4)))\nprint(open('after-error').read())")).await.unwrap();
    assert_eq!(result.std_out, "2.0\nsaved\n");
    assert_eq!(result.std_err, "");
    let source = "import os, time\nassert not os.path.exists('active')\nopen('active','w').close()\ntime.sleep(.1)\nos.unlink('active')\nprint('done')";
    let (first, second) = tokio::join!(
        runtime.execute_code(code(source)),
        runtime.execute_code(code(source))
    );
    for result in [first.unwrap(), second.unwrap()] {
        assert_eq!(result.std_out, "done\n");
        assert_eq!(result.std_err, "");
    }
    let host = std::env::temp_dir().join(format!("tachikoma-host-{}", Uuid::new_v4()));
    std::fs::write(&host, b"unchanged").unwrap();
    let source = format!(
        "open({}, 'w').write('changed')",
        serde_json::to_string(host.to_str().unwrap()).unwrap()
    );
    assert!(
        !runtime
            .execute_code(code(&source))
            .await
            .unwrap()
            .std_err
            .is_empty()
    );
    assert_eq!(std::fs::read(&host).unwrap(), b"unchanged");
    std::fs::remove_file(host).unwrap();
    runtime.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires local Linux Docker and the approved native image"]
async fn docker_process_restrictions_and_threads() {
    let runtime = live().await;
    let source = r#"
import ctypes, errno, os, platform, signal, subprocess, threading
for action in [os.fork, lambda: os.execv('/usr/local/bin/python3', ['python3']),
               lambda: subprocess.run(['/bin/true']), lambda: os.kill(1, signal.SIGCONT),
               lambda: open('/proc/1/mem', 'rb')]:
    try:
        action()
        raise AssertionError('forbidden operation succeeded')
    except PermissionError:
        pass
libc = ctypes.CDLL(None, use_errno=True)
for number in [424, 425, 426, 427, 434, 438, 440, 448]:
    assert libc.syscall(number, 0, 0, 0, 0, 0, 0) == -1
    assert ctypes.get_errno() in (errno.EPERM, errno.ENOSYS)
assert libc.syscall(435, 0, 0) == -1 and ctypes.get_errno() == errno.ENOSYS
numbers = [101, 310, 311, 312, 272, 308] if platform.machine() == 'x86_64' else [117, 270, 271, 272, 97, 268]
for number in numbers:
    assert libc.syscall(number, 1, 0, 0, 0, 0, 0) == -1
    assert ctypes.get_errno() in (errno.EPERM, errno.ENOSYS)
values = []
threads = [threading.Thread(target=lambda: values.append(1)) for _ in range(8)]
for thread in threads: thread.start()
for thread in threads: thread.join()
assert sum(values) == 8
print('restricted')
"#;
    let result = runtime.execute_code(code(source)).await.unwrap();
    assert_eq!(result.std_out, "restricted\n");
    assert_eq!(result.std_err, "");
    runtime.close().await.unwrap();
}

async fn assert_removed(runtime: &AgentRuntime, name: &str) {
    runtime
        .operation
        .lock()
        .await
        .wait_for_cleanup()
        .await
        .unwrap();
    assert!(matches!(
        runtime.docker.client.inspect_container(name, None).await,
        Err(DockerError::DockerResponseServerError {
            status_code: 404,
            ..
        })
    ));
}

async fn session_name(runtime: &AgentRuntime) -> String {
    format!(
        "tachikoma-python-{}",
        runtime.operation.lock().await.generation
    )
}

#[tokio::test]
#[ignore = "requires local Linux Docker and the approved native image"]
async fn docker_output_source_and_resource_limits() {
    let runtime = live().await;
    let generation = runtime.operation.lock().await.generation;
    assert_eq!(
        runtime
            .execute_code(code(&" ".repeat(ExecutionPolicy::SOURCE + 1)))
            .await,
        Err(ExecutionError::InvalidInput)
    );
    assert_eq!(runtime.operation.lock().await.generation, generation);
    let result = runtime
        .execute_code(code(
            "import os\nos.write(1, b'\\xff\\n'); os.write(2, b'\\xfe')",
        ))
        .await
        .unwrap();
    assert_eq!(result.std_out, "�\n");
    assert_eq!(result.std_err, "�");
    for source in [
        "import os\nwhile True: os.write(1, b'x'*4096)",
        "import os\nwhile True: os.write(2, b'x'*4096)",
        "print('x'*1000000, end='')",
        "x = bytearray(256*1024*1024)",
        "import threading, time\nfor i in range(100): threading.Thread(target=lambda: time.sleep(60), daemon=True).start()",
        "open('/workspace/full', 'wb').write(b'x'*(17*1024*1024))",
        "open('/tmp/full', 'wb').write(b'x'*(9*1024*1024))",
        "open('/dev/shm/full', 'wb').write(b'x'*(5*1024*1024))",
        "for i in range(4100): open('/workspace/f'+str(i), 'w').close()",
        "for i in range(1030): open('/tmp/f'+str(i), 'w').close()",
        "for i in range(260): open('/dev/shm/f'+str(i), 'w').close()",
    ] {
        // This later explicit call starts an empty generation after prior cleanup.
        runtime
            .execute_code(code("open('old-session','w').close()"))
            .await
            .unwrap();
        let name = session_name(&runtime).await;
        assert!(
            matches!(
                runtime.execute_code(code(source)).await,
                Err(ExecutionError::SessionLost(_))
            ),
            "resource failure must quarantine"
        );
        assert_removed(&runtime, &name).await;
        let result = runtime
            .execute_code(code("import os\nprint(os.path.exists('old-session'))"))
            .await
            .unwrap();
        assert_eq!(result.std_out, "False\n");
    }
    runtime.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires local Linux Docker; includes the full 30-second deadline"]
async fn docker_timeout_crash_cancellation_and_close() {
    let runtime = Arc::new(live().await);
    let name = session_name(&runtime).await;
    let start = std::time::Instant::now();
    assert_eq!(
        runtime
            .execute_code(code("import time; time.sleep(60)"))
            .await,
        Err(lost(ExecutionOutcome::Timeout))
    );
    assert!(start.elapsed() < Duration::from_secs(36));
    assert_removed(&runtime, &name).await;
    runtime.execute_code(code("")).await.unwrap();
    let name = session_name(&runtime).await;
    assert_eq!(
        runtime
            .execute_code(code("import ctypes; ctypes.string_at(0)"))
            .await,
        Err(lost(ExecutionOutcome::WorkerLost))
    );
    assert_removed(&runtime, &name).await;
    runtime.execute_code(code("")).await.unwrap();
    let name = session_name(&runtime).await;
    let caller = Arc::clone(&runtime);
    let task = tokio::spawn(async move {
        caller
            .execute_code(code("import time; time.sleep(60)"))
            .await
    });
    sleep(Duration::from_millis(200)).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_removed(&runtime, &name).await;
    runtime.execute_code(code("")).await.unwrap();
    let name = session_name(&runtime).await;
    let caller = Arc::clone(&runtime);
    let task = tokio::spawn(async move {
        caller
            .execute_code(code("import time; time.sleep(60)"))
            .await
    });
    sleep(Duration::from_millis(200)).await;
    runtime.close().await.unwrap();
    assert!(task.await.unwrap().is_err());
    assert_removed(&runtime, &name).await;
    assert_eq!(
        runtime.execute_code(code("")).await,
        Err(ExecutionError::Closed)
    );
}

#[tokio::test]
#[ignore = "requires local Linux Docker"]
async fn docker_drop_and_process_wide_admission() {
    let mut runtimes = Vec::new();
    for _ in 0..4 {
        runtimes.push(live().await);
    }
    let config = crate::common::config::Config::from_env().unwrap();
    assert!(matches!(
        AgentRuntime::create(&config.docker).await,
        Err(ExecutionError::AdmissionFull)
    ));
    let supervisor = Arc::clone(&runtimes[0].supervisor);
    drop(runtimes.pop());
    timeout(Duration::from_secs(6), async {
        while supervisor.admission.available_permits() == 0 {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let replacement = live().await;
    replacement.close().await.unwrap();
    for runtime in runtimes {
        runtime.close().await.unwrap();
    }
    assert_eq!(supervisor.admission.available_permits(), 4);
}

#[tokio::test]
#[ignore = "requires local Linux Docker; includes the full 60-second keeper lease"]
async fn docker_keeper_eof_and_lease_expiry() {
    let config = crate::common::config::Config::from_env().unwrap();
    let checked = docker::prepare(&config.docker).await.unwrap();
    for eof in [true, false] {
        let (state, _) = watch::channel(Lifecycle::Preparing);
        let mut session = Session {
            client: checked.client.clone(),
            image: checked.image.reference().into(),
            name: format!("tachikoma-python-{}", Uuid::new_v4()),
            creation_attempted: false,
            container_id: None,
            state,
        };
        let (mut keeper, worker) = session.prepare().await.unwrap();
        if eof {
            keeper.input.shutdown().await.unwrap();
        }
        // Keep both attachments alive. The lease test sends no heartbeat.
        timeout(Duration::from_secs(65), async {
            loop {
                if matches!(
                    checked.client.inspect_container(&session.name, None).await,
                    Err(DockerError::DockerResponseServerError {
                        status_code: 404,
                        ..
                    })
                ) {
                    break;
                }
                sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .unwrap();
        drop((keeper, worker));
        session.remove().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires local Linux Docker"]
async fn docker_missing_kernel_controls_reject_readiness() {
    let config = crate::common::config::Config::from_env().unwrap();
    let checked = docker::prepare(&config.docker).await.unwrap();
    for missing_seccomp in [true, false] {
        let name = format!("tachikoma-python-{}", Uuid::new_v4());
        let mut body = container_config(checked.image.reference(), &name);
        if missing_seccomp {
            body.host_config.as_mut().unwrap().security_opt = Some(vec![
                "no-new-privileges".into(),
                "seccomp=unconfined".into(),
            ]);
        } else {
            body.host_config.as_mut().unwrap().memory_swap = Some(256 * 1024 * 1024);
        }
        let created = checked
            .client
            .create_container(
                Some(CreateContainerOptionsBuilder::default().name(&name).build()),
                body,
            )
            .await
            .unwrap();
        let keeper = checked
            .client
            .attach_container(
                &name,
                Some(
                    AttachContainerOptionsBuilder::default()
                        .stdin(true)
                        .stdout(true)
                        .stderr(true)
                        .stream(true)
                        .build(),
                ),
            )
            .await
            .unwrap();
        checked.client.start_container(&name, None).await.unwrap();
        let (state, _) = watch::channel(Lifecycle::Preparing);
        let session = Session {
            client: checked.client.clone(),
            image: checked.image.reference().into(),
            name,
            creation_attempted: true,
            container_id: Some(created.id),
            state,
        };
        assert!(matches!(
            timeout(ExecutionPolicy::REQUEST, session.prepare_worker())
                .await
                .unwrap(),
            Err(ExecutionError::SetupFailure)
        ));
        session.remove().await.unwrap();
        drop(keeper);
    }
}

#[tokio::test]
#[ignore = "requires native Linux AMD64 or ARM64 Docker"]
async fn docker_alternate_syscall_abis() {
    let runtime = live().await;
    let arch = runtime
        .execute_code(code("import platform; print(platform.machine())"))
        .await
        .unwrap()
        .std_out;
    if arch.trim() == "x86_64" {
        for source in [
            "import ctypes\nctypes.CDLL(None).syscall(0x40000027)",
            "import ctypes, mmap\nm = mmap.mmap(-1, 4096, prot=7)\nm.write(bytes.fromhex('b814000000cd80c3'))\nctypes.CFUNCTYPE(ctypes.c_int)(ctypes.addressof(ctypes.c_char.from_buffer(m)))()",
        ] {
            assert_eq!(
                runtime.execute_code(code(source)).await,
                Err(lost(ExecutionOutcome::WorkerLost))
            );
            runtime
                .operation
                .lock()
                .await
                .wait_for_cleanup()
                .await
                .unwrap();
        }
    } else {
        assert_eq!(arch.trim(), "aarch64");
    }
    runtime.close().await.unwrap();
}

#[cfg(unix)]
mod fixtures {
    use super::*;
    use serde_json::{Value, json};
    use std::{collections::VecDeque, path::PathBuf};
    use tokio::{
        io::AsyncReadExt,
        net::{UnixListener, UnixStream},
        task::JoinSet,
    };

    enum Response {
        Json(u16, Value),
        Inspection(Option<&'static str>),
        Keeper,
        Worker {
            before: Vec<(u8, Vec<u8>)>,
            after: Vec<(u8, Vec<u8>)>,
            stall: bool,
        },
        Stall,
    }

    struct Script {
        replies: VecDeque<Response>,
        calls: Vec<(String, Value)>,
        sources: Vec<Vec<u8>>,
        created: Value,
    }

    struct Fixture {
        directory: PathBuf,
        socket: PathBuf,
        script: Arc<StdMutex<Script>>,
        server: JoinHandle<()>,
    }

    impl Fixture {
        async fn new(replies: impl IntoIterator<Item = Response>) -> Self {
            let directory = std::env::temp_dir()
                .join(format!("tk-{}", &Uuid::new_v4().simple().to_string()[..12]));
            std::fs::create_dir(&directory).unwrap();
            let socket = directory.join("s");
            let listener = UnixListener::bind(&socket).unwrap();
            let script = Arc::new(StdMutex::new(Script {
                replies: replies.into_iter().collect(),
                calls: vec![],
                sources: vec![],
                created: Value::Null,
            }));
            let state = Arc::clone(&script);
            let server = tokio::spawn(async move {
                let mut connections = JoinSet::new();
                loop {
                    tokio::select! {
                        connection = listener.accept() => {
                            let (stream, _) = connection.unwrap();
                            connections.spawn(serve(stream, Arc::clone(&state)));
                        }
                        Some(result) = connections.join_next() => { result.unwrap(); }
                    }
                }
            });
            Self {
                directory,
                socket,
                script,
                server,
            }
        }

        fn checked(&self) -> CheckedDocker {
            CheckedDocker {
                client: Docker::connect_with_local(
                    self.socket.to_str().unwrap(),
                    5,
                    bollard::API_DEFAULT_VERSION,
                )
                .unwrap(),
                image: super::super::super::models::ExecutionImage::approved().unwrap(),
            }
        }

        async fn runtime(&self) -> AgentRuntime {
            AgentRuntime::with_supervisor(self.checked(), Arc::new(ExecutionSupervisor::new()))
                .await
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
            std::fs::remove_file(&self.socket).unwrap();
            std::fs::remove_dir(&self.directory).unwrap();
        }
    }

    async fn frame(stream: &mut UnixStream, kind: u8, bytes: &[u8]) {
        let mut header = vec![kind, 0, 0, 0];
        header.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        // A caller can cancel while the fixture is writing output.
        if stream.write_all(&header).await.is_ok() {
            let _ = stream.write_all(bytes).await;
        }
    }

    async fn serve(mut stream: UnixStream, script: Arc<StdMutex<Script>>) {
        let mut received = Vec::new();
        let header_end = loop {
            let byte = match stream.read_u8().await {
                Ok(byte) => byte,
                Err(_) => return,
            };
            received.push(byte);
            assert!(received.len() < 8192);
            if received.ends_with(b"\r\n\r\n") {
                break received.len();
            }
        };
        let header = String::from_utf8(received).unwrap();
        let length = header
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        assert!(length < 128 * 1024);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        let path = header[..header_end]
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_owned();
        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let response = {
            let mut script = script.lock().unwrap();
            if path.starts_with("/containers/create") {
                script.created = body.clone();
            }
            script.calls.push((path, body));
            script
                .replies
                .pop_front()
                .expect("unexpected Docker request")
        };
        match response {
            Response::Json(status, body) => {
                let body = if status == 204 {
                    String::new()
                } else {
                    body.to_string()
                };
                let message = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(message.as_bytes()).await;
            }
            Response::Inspection(missing) => {
                let mut config = script.lock().unwrap().created.clone();
                let host = config
                    .as_object_mut()
                    .unwrap()
                    .remove("HostConfig")
                    .unwrap();
                let mut inspection = json!({"Id": "container", "Config": config, "HostConfig": host, "State": {"Running": true}});
                if let Some(key) = missing {
                    inspection["HostConfig"][key] = Value::Null;
                }
                let body = inspection.to_string();
                let message = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(message.as_bytes()).await;
            }
            Response::Keeper => {
                stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n").await.unwrap();
                let mut heartbeats = Vec::new();
                let _ = stream.read_to_end(&mut heartbeats).await;
                assert!(heartbeats.iter().all(|byte| *byte == b'H'));
            }
            Response::Worker {
                before,
                after,
                stall,
            } => {
                stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n").await.unwrap();
                for (kind, bytes) in before {
                    frame(&mut stream, kind, &bytes).await;
                }
                let mut source = Vec::new();
                if stream.read_to_end(&mut source).await.is_err() {
                    return;
                }
                script.lock().unwrap().sources.push(source);
                for (kind, bytes) in after {
                    frame(&mut stream, kind, &bytes).await;
                }
                if stall {
                    std::future::pending::<()>().await;
                }
            }
            Response::Stall => std::future::pending::<()>().await,
        }
    }

    fn ready() -> (u8, Vec<u8>) {
        (
            1,
            format!(
                "TK_READY {}\n",
                json!({"profile": {"python_version": "3.13.0", "modules": ExecutionPolicy::MODULES},
            "counters": {"memory": {"max": "0", "oom": "0"}, "pids": {"max": "0"}}})
            )
            .into_bytes(),
        )
    }

    fn worker(after: Vec<(u8, Vec<u8>)>) -> Response {
        Response::Worker {
            before: vec![ready()],
            after,
            stall: false,
        }
    }

    fn boot(worker: Response) -> Vec<Response> {
        vec![
            Response::Json(201, json!({"Id": "container", "Warnings": []})),
            Response::Keeper,
            Response::Json(204, Value::Null),
            Response::Inspection(None),
            Response::Json(201, json!({"Id": "worker"})),
            worker,
        ]
    }

    fn replacement() -> [Response; 3] {
        [
            Response::Json(200, json!({"Running": false, "ExitCode": 0})),
            Response::Json(201, json!({"Id": "next-worker"})),
            worker(vec![]),
        ]
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        timeout(Duration::from_secs(7), async {
            while !condition() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn socket_transport_drains_streams_and_confirms_exit_before_replacement() {
        let mut replies = boot(worker(vec![
            (1, b"hello\n\xff".to_vec()),
            (2, b"error\n".to_vec()),
        ]));
        replies.extend(replacement());
        replies.push(Response::Json(204, Value::Null));
        let daemon = Fixture::new(replies).await;
        let runtime = daemon.runtime().await;
        let result = runtime.execute_code(code("private-source")).await.unwrap();
        assert_eq!(result.std_out, "hello\n�");
        assert_eq!(result.std_err, "error\n");
        runtime.close().await.unwrap();
        let script = daemon.script.lock().unwrap();
        assert_eq!(script.sources[0], b"private-source");
        assert_eq!(script.calls[6].0, "/exec/worker/json");
        assert!(script.calls[7].0.ends_with("/exec"));
        assert_eq!(script.calls[7].1["User"], "10001:10001");
        assert!(
            script
                .calls
                .iter()
                .all(|(_, body)| !body.to_string().contains("private-source"))
        );
        assert_eq!(runtime.supervisor.admission.available_permits(), 4);
    }

    #[tokio::test]
    async fn missing_docker_controls_prevent_worker_creation() {
        for key in [
            "Memory",
            "MemorySwap",
            "NanoCpus",
            "PidsLimit",
            "Tmpfs",
            "ReadonlyRootfs",
            "SecurityOpt",
            "NetworkMode",
            "CapDrop",
            "LogConfig",
            "AutoRemove",
        ] {
            let mut replies = boot(worker(vec![]));
            replies.truncate(4);
            replies[3] = Response::Inspection(Some(key));
            replies.push(Response::Json(204, Value::Null));
            let daemon = Fixture::new(replies).await;
            let supervisor = Arc::new(ExecutionSupervisor::new());
            assert!(matches!(
                AgentRuntime::with_supervisor(daemon.checked(), Arc::clone(&supervisor)).await,
                Err(ExecutionError::SetupFailure)
            ));
            wait_until(|| supervisor.admission.available_permits() == 4).await;
            let script = daemon.script.lock().unwrap();
            assert!(script.sources.is_empty());
            assert!(!script.calls.iter().any(|(path, _)| path.ends_with("/exec")));
        }
    }

    #[tokio::test]
    async fn malformed_or_oversized_readiness_prevents_source_transfer() {
        for bytes in [b"not-ready\n".to_vec(), vec![b'x'; 2049]] {
            let mut replies = boot(Response::Worker {
                before: vec![(1, bytes)],
                after: vec![],
                stall: false,
            });
            replies.push(Response::Json(204, Value::Null));
            let daemon = Fixture::new(replies).await;
            let supervisor = Arc::new(ExecutionSupervisor::new());
            assert!(matches!(
                AgentRuntime::with_supervisor(daemon.checked(), Arc::clone(&supervisor)).await,
                Err(ExecutionError::SetupFailure)
            ));
            wait_until(|| supervisor.admission.available_permits() == 4).await;
            assert!(
                daemon
                    .script
                    .lock()
                    .unwrap()
                    .sources
                    .iter()
                    .all(Vec::is_empty)
            );
        }
    }

    #[tokio::test]
    async fn output_quota_applies_to_frames_during_source_transfer() {
        for descriptor in [1, 2] {
            let mut replies = boot(Response::Worker {
                before: vec![
                    ready(),
                    (descriptor, vec![b'x'; ExecutionPolicy::OUTPUT + 1]),
                ],
                after: vec![],
                stall: false,
            });
            replies.push(Response::Json(204, Value::Null));
            let daemon = Fixture::new(replies).await;
            let runtime = daemon.runtime().await;
            assert_eq!(
                runtime
                    .execute_code(code(&"x".repeat(ExecutionPolicy::SOURCE)))
                    .await,
                Err(lost(ExecutionOutcome::OutputLimit))
            );
            runtime
                .operation
                .lock()
                .await
                .wait_for_cleanup()
                .await
                .unwrap();
            assert_eq!(runtime.supervisor.admission.available_permits(), 4);
        }
    }

    #[tokio::test]
    async fn cancelled_requests_keep_supervised_cleanup_and_reservations() {
        let mut replies = boot(Response::Worker {
            before: vec![ready()],
            after: vec![],
            stall: true,
        });
        replies.extend([
            Response::Json(500, json!({"message": "private-daemon-error"})),
            Response::Json(204, Value::Null),
        ]);
        let daemon = Fixture::new(replies).await;
        let runtime = Arc::new(daemon.runtime().await);
        let caller = Arc::clone(&runtime);
        let task = tokio::spawn(async move { caller.execute_code(code("private-source")).await });
        wait_until(|| !daemon.script.lock().unwrap().sources.is_empty()).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(
            runtime.close().await,
            Err(lost(ExecutionOutcome::CleanupFailed))
        );
        assert_eq!(runtime.supervisor.admission.available_permits(), 3);
        let held: Vec<_> = (0..3)
            .map(|_| {
                Arc::clone(&runtime.supervisor.admission)
                    .try_acquire_owned()
                    .unwrap()
            })
            .collect();
        let (_, closed) = watch::channel(false);
        assert!(matches!(
            runtime.supervisor.start(&daemon.checked(), closed),
            Err(ExecutionError::AdmissionFull)
        ));
        wait_until(|| runtime.supervisor.admission.available_permits() == 1).await;
        drop(held);
        runtime.close().await.unwrap();
        assert_eq!(runtime.supervisor.admission.available_permits(), 4);
        assert_eq!(daemon.script.lock().unwrap().sources.len(), 1);
    }

    #[tokio::test]
    async fn connection_loss_quarantines_instead_of_returning_partial_output() {
        let mut replies = boot(worker(vec![(1, b"partial".to_vec())]));
        replies.extend([
            Response::Json(500, json!({"message": "private-daemon-error"})),
            Response::Json(204, Value::Null),
        ]);
        let daemon = Fixture::new(replies).await;
        let runtime = daemon.runtime().await;
        let error = runtime
            .execute_code(code("private-source"))
            .await
            .unwrap_err();
        assert_eq!(error, lost(ExecutionOutcome::WorkerLost));
        assert!(!format!("{error:?}: {error}").contains("private-daemon-error"));
        runtime.close().await.unwrap();
    }

    #[tokio::test]
    async fn ambiguous_creation_cannot_release_admission_on_a_404() {
        let daemon = Fixture::new([
            Response::Stall,
            Response::Json(404, json!({"message": "absent"})),
            Response::Inspection(None),
            Response::Json(204, Value::Null),
        ])
        .await;
        let supervisor = Arc::new(ExecutionSupervisor::new());
        let (_closed, receiver) = watch::channel(false);
        let (mut ticket, ready) = supervisor.start(&daemon.checked(), receiver).unwrap();
        // Keep the runtime cancellation sender alive while the create request stalls.
        drop(ready);
        wait_until(|| !daemon.script.lock().unwrap().calls.is_empty()).await;
        ticket.cancel.send_replace(true);
        assert_eq!(
            ticket.wait_for_cleanup().await,
            Err(lost(ExecutionOutcome::CleanupFailed))
        );
        assert_eq!(supervisor.admission.available_permits(), 3);
        wait_until(|| supervisor.admission.available_permits() == 4).await;
    }

    #[tokio::test]
    async fn cancelled_creation_never_removes_a_foreign_container_with_the_same_name() {
        let daemon = Fixture::new([
            Response::Stall,
            Response::Json(
                200,
                json!({"Id": "foreign", "Config": {"Labels": {"other": "owner"}}}),
            ),
            Response::Inspection(None),
            Response::Json(204, Value::Null),
        ])
        .await;
        let supervisor = Arc::new(ExecutionSupervisor::new());
        let (_closed, receiver) = watch::channel(false);
        let (mut ticket, _ready) = supervisor.start(&daemon.checked(), receiver).unwrap();
        wait_until(|| !daemon.script.lock().unwrap().calls.is_empty()).await;
        ticket.cancel.send_replace(true);
        assert_eq!(
            ticket.wait_for_cleanup().await,
            Err(lost(ExecutionOutcome::CleanupFailed))
        );
        wait_until(|| supervisor.admission.available_permits() == 4).await;
        let script = daemon.script.lock().unwrap();
        assert!(
            script
                .calls
                .last()
                .unwrap()
                .0
                .starts_with("/containers/container?")
        );
        assert!(
            !script
                .calls
                .iter()
                .any(|(path, _)| path.contains("foreign"))
        );
    }

    #[tokio::test]
    async fn reconciliation_preserves_live_sessions_and_unrelated_containers() {
        let daemon = Fixture::new([
            Response::Json(200, json!([
                {"Id": "stopped-owned", "State": "exited", "Labels": {OWNER_LABEL: OWNER_VALUE}},
                {"Id": "live-owned", "State": "running", "Labels": {OWNER_LABEL: OWNER_VALUE}},
                {"Id": "unrelated", "State": "exited", "Labels": {"other": "owner"}}
            ])), Response::Json(204, Value::Null),
        ]).await;
        reconcile(&daemon.checked().client).await.unwrap();
        let script = daemon.script.lock().unwrap();
        assert_eq!(script.calls.len(), 2);
        assert!(script.calls[1].0.starts_with("/containers/stopped-owned?"));
        assert!(script.calls[1].0.contains("force=false"));
    }
}
