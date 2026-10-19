use std::{
    collections::VecDeque,
    convert::Infallible,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::*;

const IMAGE: &str =
    "ghcr.io/owner/image@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

enum Reply {
    Json(StatusCode, Value),
    Pull(&'static str),
    StallResponse,
    StallBody,
}

#[derive(Default)]
struct Script {
    replies: VecDeque<Reply>,
    requests: Vec<(String, String)>,
}

struct MockDocker {
    directory: PathBuf,
    socket: PathBuf,
    script: Arc<Mutex<Script>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockDocker {
    async fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "tk-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("s");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let script = Arc::new(Mutex::new(Script {
            replies: replies.into_iter().collect(),
            requests: vec![],
        }));
        let app = Router::new()
            .fallback(reply)
            .with_state(Arc::clone(&script));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            directory,
            socket,
            script,
            task,
        }
    }

    fn client(&self) -> Docker {
        Docker::connect_with_local(self.socket.to_str().unwrap(), 5, API_DEFAULT_VERSION).unwrap()
    }

    async fn check(&self) -> Result<(), DockerPreflightError> {
        ensure_ready_with_client(
            self.client(),
            &ExecutionImage::from_reference(IMAGE).unwrap(),
            ExecutionPolicy::REQUEST,
            ExecutionPolicy::IMAGE_PULL,
        )
        .await
    }

    fn requests(&self) -> Vec<(String, String)> {
        self.script.lock().unwrap().requests.clone()
    }
}

impl Drop for MockDocker {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

async fn reply(State(script): State<Arc<Mutex<Script>>>, request: Request<Body>) -> Response {
    let response = {
        let mut script = script.lock().unwrap();
        script.requests.push((
            request.method().to_string(),
            request.uri().path_and_query().unwrap().to_string(),
        ));
        script
            .replies
            .pop_front()
            .expect("unexpected Docker request")
    };
    match response {
        Reply::Json(status, value) => (status, Json(value)).into_response(),
        Reply::Pull(body) => body.into_response(),
        Reply::StallResponse => std::future::pending().await,
        Reply::StallBody => Response::new(Body::from_stream(futures_util::stream::pending::<
            Result<Bytes, Infallible>,
        >())),
    }
}

fn version() -> Reply {
    Reply::Json(StatusCode::OK, json!({"ApiVersion": "1.45"}))
}

fn info(os: &str, architecture: &str) -> Reply {
    Reply::Json(
        StatusCode::OK,
        json!({"OSType": os, "Architecture": architecture}),
    )
}

fn image(architecture: &str) -> Reply {
    Reply::Json(
        StatusCode::OK,
        json!({"Id": "sha256:configuration-digest", "RepoDigests": [IMAGE],
            "Os": "linux", "Architecture": architecture}),
    )
}

fn missing_image() -> Reply {
    Reply::Json(StatusCode::NOT_FOUND, json!({"message": "not present"}))
}

#[tokio::test]
async fn cached_native_images_boot_without_a_pull() {
    for (daemon_architecture, image_architecture) in [("x86_64", "amd64"), ("aarch64", "arm64")] {
        let daemon = MockDocker::new([
            version(),
            info("linux", daemon_architecture),
            image(image_architecture),
        ])
        .await;
        daemon.check().await.unwrap();
        let requests = daemon.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|(method, _)| method == "GET"));
        assert_eq!(requests[0].1, "/version");
        assert_eq!(requests[1].1, "/v1.45/info");
        assert_eq!(requests[2].1, format!("/v1.45/images/{IMAGE}/json"));
    }
}

#[tokio::test]
async fn prepared_client_keeps_the_negotiated_prefix_for_runtime_operations() {
    let daemon = MockDocker::new([
        version(),
        info("linux", "amd64"),
        image("amd64"),
        Reply::Json(StatusCode::OK, json!({"Id": "private-container"})),
    ])
    .await;
    let client = prepare_with_client(
        daemon.client(),
        &ExecutionImage::from_reference(IMAGE).unwrap(),
        ExecutionPolicy::REQUEST,
        ExecutionPolicy::IMAGE_PULL,
    )
    .await
    .unwrap();
    client
        .inspect_container("private-container", None)
        .await
        .unwrap();
    assert_eq!(
        daemon.requests()[3].1,
        "/v1.45/containers/private-container/json"
    );
}

fn inspection(expected: &ContainerCreateBody) -> Value {
    let mut config = serde_json::to_value(expected).unwrap();
    let host = config
        .as_object_mut()
        .unwrap()
        .remove("HostConfig")
        .unwrap();
    json!({"Config": config, "HostConfig": host, "State": {"Running": true}})
}

#[test]
fn container_inspection_rejects_extra_mounts_and_privileges() {
    let expected = container_config(IMAGE, "private-session");
    let inspection = inspection(&expected);
    for (key, value) in [
        ("Binds", json!(["/private-host:/workspace"])),
        (
            "Mounts",
            json!([{"Type": "bind", "Source": "/private-host", "Target": "/workspace"}]),
        ),
        ("VolumesFrom", json!(["foreign-container"])),
        ("CapAdd", json!(["SYS_ADMIN"])),
        ("Privileged", json!(true)),
    ] {
        let mut changed = inspection.clone();
        changed["HostConfig"][key] = value;
        assert_eq!(
            verify_container(&serde_json::from_value(changed).unwrap(), &expected),
            Err(ExecutionError::SetupFailure)
        );
    }
}

#[test]
fn typed_inspection_requires_configured_controls_and_accepts_daemon_defaults() {
    let expected = container_config(IMAGE, "private-session");
    let mut inspection = inspection(&expected);
    inspection["Config"]["Labels"]["image-label"] = json!("preserved");
    inspection["HostConfig"]["LogConfig"]["Config"] = json!({});
    inspection["HostConfig"]["RestartPolicy"]["MaximumRetryCount"] = json!(0);
    inspection["HostConfig"]["CpuShares"] = json!(0);
    assert_eq!(
        verify_container(
            &serde_json::from_value(inspection.clone()).unwrap(),
            &expected
        ),
        Ok(())
    );

    for section in ["Config", "HostConfig", "State"] {
        let mut missing = inspection.clone();
        missing[section] = Value::Null;
        assert_eq!(
            verify_container(&serde_json::from_value(missing).unwrap(), &expected),
            Err(ExecutionError::SetupFailure)
        );
        for field in self::inspection(&expected)[section]
            .as_object()
            .unwrap()
            .keys()
        {
            let mut missing = inspection.clone();
            missing[section].as_object_mut().unwrap().remove(field);
            assert_eq!(
                verify_container(&serde_json::from_value(missing).unwrap(), &expected),
                Err(ExecutionError::SetupFailure),
                "missing {section}.{field} must fail preparation"
            );
        }
    }

    for (section, field, value) in [
        ("Config", "User", json!("0:0")),
        ("Config", "Cmd", json!(["unexpected"])),
        ("Config", "Labels", json!({OWNER_LABEL: "foreign"})),
        ("HostConfig", "Memory", json!(1)),
        ("HostConfig", "MemorySwap", json!(-1)),
        ("HostConfig", "NanoCpus", json!(0)),
        ("HostConfig", "PidsLimit", json!(-1)),
        ("HostConfig", "ShmSize", json!(0)),
        ("HostConfig", "Tmpfs", json!({})),
        ("HostConfig", "RestartPolicy", json!({"Name": "always"})),
        ("HostConfig", "LogConfig", json!({"Type": "json-file"})),
        ("State", "Running", json!(false)),
    ] {
        let mut changed = inspection.clone();
        changed[section][field] = value;
        assert_eq!(
            verify_container(&serde_json::from_value(changed).unwrap(), &expected),
            Err(ExecutionError::SetupFailure),
            "changed {section}.{field} must fail preparation"
        );
    }
}

#[tokio::test]
async fn only_a_missing_image_triggers_one_digest_and_platform_pull() {
    for (architecture, platform) in [("amd64", "linux/amd64"), ("arm64", "linux/arm64")] {
        let daemon = MockDocker::new([
            version(),
            info("linux", architecture),
            missing_image(),
            Reply::Pull("{\"status\":\"Downloading\"}\n{\"status\":\"Download complete\"}\n"),
            image(architecture),
        ])
        .await;
        daemon.check().await.unwrap();
        let requests = daemon.requests();
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[3].0, "POST");
        let url = reqwest::Url::parse(&format!("http://localhost{}", requests[3].1)).unwrap();
        assert_eq!(url.path(), "/v1.45/images/create");
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query.get("fromImage").unwrap(), IMAGE);
        assert_eq!(query.get("platform").unwrap(), platform);
        assert_eq!(requests[2], requests[4]);
    }

    let daemon = MockDocker::new([
        version(),
        info("linux", "amd64"),
        Reply::Json(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"message": "private-daemon-diagnostic"}),
        ),
    ])
    .await;
    let error = daemon.check().await.unwrap_err();
    assert_eq!(error, DockerPreflightError::ImageInspectionFailed);
    assert!(!error.to_string().contains("private-daemon-diagnostic"));
    assert_eq!(daemon.requests().len(), 3);
}

#[tokio::test]
async fn unsupported_daemons_fail_before_touching_images() {
    for (os, architecture) in [("windows", "amd64"), ("linux", "s390x"), ("linux", "")] {
        let daemon = MockDocker::new([version(), info(os, architecture)]).await;
        assert_eq!(
            daemon.check().await,
            Err(DockerPreflightError::UnsupportedPlatform)
        );
        assert_eq!(daemon.requests().len(), 2);
    }
    let daemon = MockDocker::new([Reply::Json(StatusCode::OK, json!({}))]).await;
    assert_eq!(
        daemon.check().await,
        Err(DockerPreflightError::IncompatibleApi)
    );
    assert_eq!(daemon.requests().len(), 1);

    let daemon = MockDocker::new([
        Reply::Json(
            StatusCode::OK,
            json!({"ApiVersion": "1.99", "MinAPIVersion": "1.98"}),
        ),
        Reply::Json(
            StatusCode::BAD_REQUEST,
            json!({"message": "client version is too old"}),
        ),
    ])
    .await;
    assert_eq!(
        daemon.check().await,
        Err(DockerPreflightError::IncompatibleApi)
    );
    assert_eq!(
        daemon.requests()[1].1,
        format!(
            "/v{}.{}/info",
            API_DEFAULT_VERSION.major_version, API_DEFAULT_VERSION.minor_version
        )
    );
}

#[tokio::test]
async fn daemon_failure_and_timeout_stop_preflight() {
    let daemon = MockDocker::new([Reply::Json(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({"message": "private-daemon-diagnostic"}),
    )])
    .await;
    let error = daemon.check().await.unwrap_err();
    assert_eq!(error, DockerPreflightError::DaemonUnavailable);
    assert!(!error.to_string().contains("private-daemon-diagnostic"));
    assert_eq!(daemon.requests().len(), 1);

    let stalled = MockDocker::new([Reply::StallResponse]).await;
    assert_eq!(
        ensure_ready_with_client(
            stalled.client(),
            &ExecutionImage::from_reference(IMAGE).unwrap(),
            Duration::from_millis(50),
            ExecutionPolicy::IMAGE_PULL,
        )
        .await,
        Err(DockerPreflightError::TimedOut("connection"))
    );
    assert!(stalled.requests().len() <= 1);
}

#[tokio::test]
async fn streamed_pull_errors_and_stalled_bodies_cannot_report_readiness() {
    for body in [
        "{\"errorDetail\":{\"message\":\"private-registry-error\"}}\n",
        "{\"error\":\"private-registry-error\"}\n",
    ] {
        let daemon = MockDocker::new([
            version(),
            info("linux", "amd64"),
            missing_image(),
            Reply::Pull(body),
        ])
        .await;
        let error = daemon.check().await.unwrap_err();
        assert_eq!(error, DockerPreflightError::ImagePullFailed);
        assert!(!error.to_string().contains("private-registry-error"));
        assert_eq!(daemon.requests().len(), 4);
    }

    let stalled = MockDocker::new([
        version(),
        info("linux", "amd64"),
        missing_image(),
        Reply::StallBody,
    ])
    .await;
    assert_eq!(
        ensure_ready_with_client(
            stalled.client(),
            &ExecutionImage::from_reference(IMAGE).unwrap(),
            ExecutionPolicy::REQUEST,
            Duration::from_millis(50),
        )
        .await,
        Err(DockerPreflightError::TimedOut("image download"))
    );
    assert_eq!(stalled.requests().len(), 4);
}

#[tokio::test]
async fn image_digest_and_platform_are_checked_after_a_pull() {
    for inspection in [
        json!({"RepoDigests": [], "Os": "linux", "Architecture": "amd64"}),
        json!({"RepoDigests": [IMAGE], "Os": "windows", "Architecture": "amd64"}),
        json!({"RepoDigests": [IMAGE], "Os": "linux", "Architecture": "arm64"}),
    ] {
        let daemon = MockDocker::new([
            version(),
            info("linux", "amd64"),
            missing_image(),
            Reply::Pull("{\"status\":\"Downloaded\"}\n"),
            Reply::Json(StatusCode::OK, inspection),
        ])
        .await;
        assert_eq!(
            daemon.check().await,
            Err(DockerPreflightError::ImageMismatch)
        );
        assert_eq!(daemon.requests().len(), 5);
    }
}
