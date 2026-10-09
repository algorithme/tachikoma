use std::time::Duration;

use axum::http::Uri;
use bollard::{
    API_DEFAULT_VERSION, Docker, errors::Error, models::ImageInspect,
    query_parameters::CreateImageOptionsBuilder,
};
use futures_util::TryStreamExt;
use tokio::time::timeout;

use super::models::{DockerEndpoint, DockerPreflightError, ExecutionImage, LinuxArchitecture};

const DAEMON_TIMEOUT: Duration = Duration::from_secs(5);
const PULL_TIMEOUT: Duration = Duration::from_secs(600);

/// Verify local Docker and cache the reviewed image before opening the application listener.
pub(crate) async fn ensure_ready(endpoint: &DockerEndpoint) -> Result<(), DockerPreflightError> {
    let image = ExecutionImage::approved()?;
    let docker = connect_local_docker(endpoint)?;
    tracing::info!("Checking local Docker and the approved execution image");
    ensure_ready_with_client(docker, &image, DAEMON_TIMEOUT, PULL_TIMEOUT).await?;
    tracing::info!("Docker and the approved execution image are ready");
    Ok(())
}

fn connect_local_docker(endpoint: &DockerEndpoint) -> Result<Docker, DockerPreflightError> {
    Docker::connect_with_local(
        endpoint.connection_path(),
        DAEMON_TIMEOUT.as_secs(),
        API_DEFAULT_VERSION,
    )
    .map_err(|_| DockerPreflightError::DaemonUnavailable)
}

async fn ensure_ready_with_client(
    docker: Docker,
    image: &ExecutionImage,
    daemon_timeout: Duration,
    pull_timeout: Duration,
) -> Result<(), DockerPreflightError> {
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
    ensure_image_available(&docker, image, architecture, daemon_timeout, pull_timeout).await
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

#[cfg(all(test, unix))]
#[path = "tests.rs"]
mod tests;
