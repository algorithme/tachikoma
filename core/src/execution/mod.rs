//! Local Docker and approved-image prerequisites for application startup.

mod docker;
mod models;

pub(crate) use docker::ensure_ready;
pub(crate) use models::DockerEndpoint;
