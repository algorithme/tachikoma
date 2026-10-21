use std::{collections::HashMap, time::Duration};

use axum::http::Uri;
use bollard::{
    API_DEFAULT_VERSION, Docker,
    errors::Error,
    models::{
        ContainerCreateBody, ContainerInspectResponse, HostConfig, HostConfigCgroupnsModeEnum,
        HostConfigLogConfig, ImageInspect, RestartPolicy, RestartPolicyNameEnum,
    },
    query_parameters::CreateImageOptionsBuilder,
};
use futures_util::TryStreamExt;
use tokio::time::timeout;

use super::models::{
    DockerEndpoint, DockerPreflightError, ExecutionImage, LinuxArchitecture,
    runtime::{ExecutionError, ExecutionPolicy},
};

const LAUNCHER: &str = include_str!("worker.py");
pub(super) const OWNER_LABEL: &str = "io.tachikoma.execution";
pub(super) const OWNER_VALUE: &str = "private-python-v1";

/// Verify local Docker and cache the reviewed image before opening the application listener.
pub(crate) async fn ensure_ready(endpoint: &DockerEndpoint) -> Result<(), DockerPreflightError> {
    prepare(endpoint).await?;
    Ok(())
}

pub(super) struct CheckedDocker {
    pub(super) client: Docker,
    pub(super) image: ExecutionImage,
}

pub(super) async fn prepare(
    endpoint: &DockerEndpoint,
) -> Result<CheckedDocker, DockerPreflightError> {
    let image = ExecutionImage::approved()?;
    let docker = connect_local_docker(endpoint)?;
    tracing::info!("Checking local Docker and the approved execution image");
    let client = prepare_with_client(
        docker,
        &image,
        ExecutionPolicy::REQUEST,
        ExecutionPolicy::IMAGE_PULL,
    )
    .await?;
    tracing::info!("Docker and the approved execution image are ready");
    Ok(CheckedDocker { client, image })
}

fn connect_local_docker(endpoint: &DockerEndpoint) -> Result<Docker, DockerPreflightError> {
    Docker::connect_with_local(
        endpoint.connection_path(),
        ExecutionPolicy::REQUEST.as_secs(),
        API_DEFAULT_VERSION,
    )
    .map_err(|_| DockerPreflightError::DaemonUnavailable)
}

async fn prepare_with_client(
    docker: Docker,
    image: &ExecutionImage,
    daemon_timeout: Duration,
    pull_timeout: Duration,
) -> Result<Docker, DockerPreflightError> {
    let (docker, architecture) = timeout(daemon_timeout, async {
        let docker = docker
            .negotiate_version()
            .await
            .map_err(|error| match error {
                Error::APIVersionParseError {} => DockerPreflightError::IncompatibleApi,
                Error::RequestTimeoutError => DockerPreflightError::TimedOut("connection"),
                _ => DockerPreflightError::DaemonUnavailable,
            })?;
        let version = docker.client_version();
        let prefix = format!("/v{}.{}", version.major_version, version.minor_version);
        // Bollard 0.21.1's URL join discards its negotiated API path prefix.
        // Preserve the local transport URI and restore that prefix on the wire.
        let docker = docker.with_request_modifier(move |mut request| {
            let mut parts = request.uri().clone().into_parts();
            let path = parts
                .path_and_query
                .as_ref()
                .map_or("/", |path| path.as_str());
            parts.path_and_query = Some(
                format!("{prefix}{path}")
                    .parse()
                    .expect("an API version prefix preserves a valid request path"),
            );
            *request.uri_mut() =
                Uri::from_parts(parts).expect("the transport URI remains valid after prefixing");
            request
        });
        let info = docker.info().await.map_err(|error| match error {
            Error::RequestTimeoutError => DockerPreflightError::TimedOut("connection"),
            Error::DockerResponseServerError {
                status_code: 400, ..
            } => DockerPreflightError::IncompatibleApi,
            _ => DockerPreflightError::DaemonUnavailable,
        })?;
        let architecture =
            LinuxArchitecture::from_daemon(info.os_type.as_deref(), info.architecture.as_deref())?;
        Ok::<_, DockerPreflightError>((docker, architecture))
    })
    .await
    .map_err(|_| DockerPreflightError::TimedOut("connection"))??;
    ensure_image_available(&docker, image, architecture, daemon_timeout, pull_timeout).await?;
    Ok(docker)
}

#[cfg(test)]
async fn ensure_ready_with_client(
    docker: Docker,
    image: &ExecutionImage,
    daemon_timeout: Duration,
    pull_timeout: Duration,
) -> Result<(), DockerPreflightError> {
    prepare_with_client(docker, image, daemon_timeout, pull_timeout)
        .await
        .map(|_| ())
}

async fn ensure_image_available(
    docker: &Docker,
    image: &ExecutionImage,
    architecture: LinuxArchitecture,
    request_timeout: Duration,
    pull_timeout: Duration,
) -> Result<(), DockerPreflightError> {
    match timeout(request_timeout, docker.inspect_image(image.reference())).await {
        Ok(Ok(inspection)) => return validate_image(&inspection, image, architecture),
        Ok(Err(Error::DockerResponseServerError {
            status_code: 404, ..
        })) => {}
        Ok(Err(Error::RequestTimeoutError)) | Err(_) => {
            return Err(DockerPreflightError::TimedOut("image inspection"));
        }
        Ok(Err(_)) => return Err(DockerPreflightError::ImageInspectionFailed),
    }

    tracing::info!("Downloading the approved execution image from GHCR");
    let options = CreateImageOptionsBuilder::default()
        .from_image(image.reference())
        .platform(architecture.platform())
        .build();
    timeout(pull_timeout, async {
        let mut progress = docker.create_image(Some(options), None, None);
        while let Some(event) = progress
            .try_next()
            .await
            .map_err(|_| DockerPreflightError::ImagePullFailed)?
        {
            if event.error_detail.is_some()
                || (event.status.is_none() && event.id.is_none() && event.progress_detail.is_none())
            {
                return Err(DockerPreflightError::ImagePullFailed);
            }
        }
        Ok(())
    })
    .await
    .map_err(|_| DockerPreflightError::TimedOut("image download"))??;

    let inspection = timeout(request_timeout, docker.inspect_image(image.reference()))
        .await
        .map_err(|_| DockerPreflightError::TimedOut("image inspection"))?
        .map_err(|error| match error {
            Error::RequestTimeoutError => DockerPreflightError::TimedOut("image inspection"),
            _ => DockerPreflightError::ImageInspectionFailed,
        })?;
    validate_image(&inspection, image, architecture)
}

fn validate_image(
    inspection: &ImageInspect,
    image: &ExecutionImage,
    architecture: LinuxArchitecture,
) -> Result<(), DockerPreflightError> {
    if inspection.os.as_deref() != Some("linux")
        || inspection.architecture.as_deref() != Some(architecture.image_architecture())
        || !inspection
            .repo_digests
            .as_ref()
            .is_some_and(|digests| digests.iter().any(|digest| digest == image.reference()))
    {
        return Err(DockerPreflightError::ImageMismatch);
    }
    Ok(())
}

pub(super) fn command(mode: &str) -> Vec<String> {
    vec![
        "/usr/bin/env".into(),
        "-i".into(),
        "PATH=/usr/local/bin:/usr/bin:/bin".into(),
        format!("HOME={}", ExecutionPolicy::WORKER.directory),
        "LANG=C.UTF-8".into(),
        format!("TMPDIR={}", ExecutionPolicy::TEMPORARY.path),
        "/usr/local/bin/python3".into(),
        "-I".into(),
        "-B".into(),
        "-u".into(),
        "-c".into(),
        LAUNCHER.into(),
        mode.into(),
        ExecutionPolicy::launcher_json(),
    ]
}

pub(super) fn container_config(image: &str, name: &str) -> ContainerCreateBody {
    ContainerCreateBody {
        image: Some(image.into()),
        cmd: Some(command("keeper")),
        user: Some(ExecutionPolicy::KEEPER.docker_user()),
        working_dir: Some(ExecutionPolicy::KEEPER.directory.into()),
        open_stdin: Some(true),
        stdin_once: Some(true),
        attach_stdin: Some(true),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        tty: Some(false),
        network_disabled: Some(true),
        labels: Some(HashMap::from([
            (OWNER_LABEL.into(), OWNER_VALUE.into()),
            ("io.tachikoma.session".into(), name.into()),
        ])),
        host_config: Some(HostConfig {
            network_mode: Some("none".into()),
            readonly_rootfs: Some(true),
            cap_drop: Some(vec!["ALL".into()]),
            security_opt: Some(vec!["no-new-privileges".into()]),
            log_config: Some(HostConfigLogConfig {
                typ: Some("none".into()),
                ..Default::default()
            }),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::NO),
                ..Default::default()
            }),
            auto_remove: Some(true),
            privileged: Some(false),
            pid_mode: Some(String::new()),
            cgroupns_mode: Some(HostConfigCgroupnsModeEnum::PRIVATE),
            memory: Some(ExecutionPolicy::MEMORY),
            memory_swap: Some(ExecutionPolicy::MEMORY + ExecutionPolicy::SWAP),
            nano_cpus: Some(ExecutionPolicy::CPUS * 1_000_000_000),
            pids_limit: Some(ExecutionPolicy::PIDS),
            ipc_mode: Some("private".into()),
            shm_size: Some(ExecutionPolicy::SHARED_MEMORY.bytes),
            tmpfs: Some(
                ExecutionPolicy::MOUNTS
                    .iter()
                    .map(|mount| (mount.path.into(), mount.docker_options()))
                    .collect(),
            ),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub(super) fn verify_container(
    actual: &ContainerInspectResponse,
    expected: &ContainerCreateBody,
) -> Result<(), ExecutionError> {
    let config = actual.config.as_ref().ok_or(ExecutionError::SetupFailure)?;
    let host = actual
        .host_config
        .as_ref()
        .ok_or(ExecutionError::SetupFailure)?;
    let required = expected
        .host_config
        .as_ref()
        .ok_or(ExecutionError::SetupFailure)?;
    let labels = expected
        .labels
        .as_ref()
        .ok_or(ExecutionError::SetupFailure)?;
    for (control, matches) in [
        ("Image", config.image == expected.image),
        ("Cmd", config.cmd == expected.cmd),
        ("User", config.user == expected.user),
        ("WorkingDir", config.working_dir == expected.working_dir),
        ("OpenStdin", config.open_stdin == expected.open_stdin),
        ("StdinOnce", config.stdin_once == expected.stdin_once),
        ("AttachStdin", config.attach_stdin == expected.attach_stdin),
        (
            "AttachStdout",
            config.attach_stdout == expected.attach_stdout,
        ),
        (
            "AttachStderr",
            config.attach_stderr == expected.attach_stderr,
        ),
        ("Tty", config.tty == expected.tty),
        (
            "NetworkDisabled",
            config.network_disabled == expected.network_disabled,
        ),
        (
            "Labels",
            config.labels.as_ref().is_some_and(|actual| {
                labels
                    .iter()
                    .all(|(key, value)| actual.get(key) == Some(value))
            }),
        ),
        ("NetworkMode", host.network_mode == required.network_mode),
        (
            "ReadonlyRootfs",
            host.readonly_rootfs == required.readonly_rootfs,
        ),
        ("CapDrop", host.cap_drop == required.cap_drop),
        ("SecurityOpt", host.security_opt == required.security_opt),
        (
            "LogConfig",
            host.log_config.as_ref().and_then(|c| c.typ.as_ref())
                == required.log_config.as_ref().and_then(|c| c.typ.as_ref()),
        ),
        (
            "RestartPolicy",
            host.restart_policy.as_ref().and_then(|p| p.name)
                == required.restart_policy.as_ref().and_then(|p| p.name),
        ),
        ("AutoRemove", host.auto_remove == required.auto_remove),
        ("Privileged", host.privileged == required.privileged),
        ("PidMode", host.pid_mode == required.pid_mode),
        ("CgroupnsMode", host.cgroupns_mode == required.cgroupns_mode),
        ("Memory", host.memory == required.memory),
        ("MemorySwap", host.memory_swap == required.memory_swap),
        ("NanoCpus", host.nano_cpus == required.nano_cpus),
        ("PidsLimit", host.pids_limit == required.pids_limit),
        ("IpcMode", host.ipc_mode == required.ipc_mode),
        ("ShmSize", host.shm_size == required.shm_size),
        ("Tmpfs", host.tmpfs == required.tmpfs),
        ("Binds", host.binds.as_ref().is_none_or(Vec::is_empty)),
        ("Mounts", host.mounts.as_ref().is_none_or(Vec::is_empty)),
        (
            "VolumesFrom",
            host.volumes_from.as_ref().is_none_or(Vec::is_empty),
        ),
        ("Devices", host.devices.as_ref().is_none_or(Vec::is_empty)),
        (
            "DeviceRequests",
            host.device_requests.as_ref().is_none_or(Vec::is_empty),
        ),
        ("CapAdd", host.cap_add.as_ref().is_none_or(Vec::is_empty)),
        (
            "Running",
            actual.state.as_ref().and_then(|state| state.running) == Some(true),
        ),
    ] {
        if !matches {
            tracing::warn!(control, "Container control did not match policy");
            return Err(ExecutionError::SetupFailure);
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "tests.rs"]
mod tests;
