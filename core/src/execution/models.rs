use std::{fmt, io};

#[cfg(unix)]
use std::path::Path;

/// A host-selected local Docker socket or named pipe, never a remote context.
pub(crate) struct DockerEndpoint(String);

impl DockerEndpoint {
    pub(crate) fn from_lookup(
        lookup: &mut impl FnMut(&str) -> io::Result<Option<String>>,
    ) -> io::Result<Self> {
        let value = match lookup("TACHIKOMA_DOCKER_HOST")? {
            Some(value) => value,
            None => {
                #[cfg(target_os = "macos")]
                {
                    let home = lookup("HOME")?.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Set HOME or TACHIKOMA_DOCKER_HOST to locate the local Docker socket.",
                        )
                    })?;
                    if !Path::new(&home).is_absolute() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "HOME must be an absolute path to locate the local Docker socket.",
                        ));
                    }
                    format!(
                        "unix://{}/.docker/run/docker.sock",
                        home.trim_end_matches('/')
                    )
                }
                #[cfg(target_os = "linux")]
                {
                    "unix:///var/run/docker.sock".to_owned()
                }
                #[cfg(target_os = "windows")]
                {
                    "npipe:////./pipe/docker_engine".to_owned()
                }
                #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
                {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Docker execution supports Linux, macOS and Windows hosts.",
                    ));
                }
            }
        };
        Self::parse(value)
    }

    fn parse(value: String) -> io::Result<Self> {
        let valid = !value.chars().any(char::is_control) && {
            #[cfg(unix)]
            {
                value
                    .strip_prefix("unix://")
                    .is_some_and(|path| Path::new(path).is_absolute())
            }
            #[cfg(windows)]
            {
                value.strip_prefix("npipe:////./pipe/").is_some_and(|name| {
                    !name.is_empty()
                        && name != "."
                        && name != ".."
                        && !name.contains(['/', '\\', ':'])
                })
            }
            #[cfg(not(any(unix, windows)))]
            {
                false
            }
        };
        if valid {
            Ok(Self(value))
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TACHIKOMA_DOCKER_HOST must name a local absolute Unix socket or a local Windows named pipe; remote Docker connections are unsupported.",
            ))
        }
    }

    pub(super) fn connection_path(&self) -> &str {
        self.0
            .strip_prefix("unix://")
            .or_else(|| self.0.strip_prefix("npipe://"))
            .expect("DockerEndpoint is validated on construction")
    }
}

/// A reviewed registry manifest digest embedded in the application build.
pub(super) struct ExecutionImage(&'static str);

impl ExecutionImage {
    pub(super) fn approved() -> Result<Self, DockerPreflightError> {
        Self::from_reference(include_str!(
            "../../images/base-execution-py3.x/approved-image.ref"
        ))
    }

    pub(super) fn from_reference(value: &'static str) -> Result<Self, DockerPreflightError> {
        let reference = value.trim();
        let valid = reference
            .split_once("@sha256:")
            .is_some_and(|(name, digest)| {
                let Some(repository) = name.strip_prefix("ghcr.io/") else {
                    return false;
                };
                let parts: Vec<_> = repository.split('/').collect();
                parts.len() >= 2
                    && parts.iter().all(|part| {
                        !part.is_empty()
                            && *part != "."
                            && *part != ".."
                            && part.bytes().all(|byte| {
                                byte.is_ascii_lowercase()
                                    || byte.is_ascii_digit()
                                    || b"._-".contains(&byte)
                            })
                    })
                    && digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            });
        if valid {
            Ok(Self(reference))
        } else {
            Err(DockerPreflightError::InvalidImageReference)
        }
    }

    pub(super) fn reference(&self) -> &'static str {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LinuxArchitecture {
    Amd64,
    Arm64,
}

impl LinuxArchitecture {
    pub(super) fn from_daemon(
        os: Option<&str>,
        architecture: Option<&str>,
    ) -> Result<Self, DockerPreflightError> {
        match (os, architecture) {
            (Some("linux"), Some("amd64" | "x86_64")) => Ok(Self::Amd64),
            (Some("linux"), Some("arm64" | "aarch64")) => Ok(Self::Arm64),
            _ => Err(DockerPreflightError::UnsupportedPlatform),
        }
    }

    pub(super) fn platform(self) -> &'static str {
        match self {
            Self::Amd64 => "linux/amd64",
            Self::Arm64 => "linux/arm64",
        }
    }

    pub(super) fn image_architecture(self) -> &'static str {
        match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum DockerPreflightError {
    InvalidImageReference,
    DaemonUnavailable,
    IncompatibleApi,
    UnsupportedPlatform,
    ImageInspectionFailed,
    ImagePullFailed,
    ImageMismatch,
    TimedOut(&'static str),
}

impl fmt::Display for DockerPreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidImageReference => formatter.write_str(
                "The checked-in execution image must be a reviewed GHCR reference pinned by SHA-256 digest.",
            ),
            Self::DaemonUnavailable => formatter.write_str(
                "Cannot reach local Docker. Start Docker Engine or Docker Desktop and check TACHIKOMA_DOCKER_HOST and socket permissions.",
            ),
            Self::IncompatibleApi => formatter.write_str(
                "Could not negotiate a compatible Docker Engine API. Check your Docker installation.",
            ),
            Self::UnsupportedPlatform => formatter.write_str(
                "Docker must run Linux containers on amd64 or arm64. Check Docker Desktop's container mode and daemon architecture.",
            ),
            Self::ImageInspectionFailed => formatter.write_str(
                "Could not inspect the approved execution image in local Docker. Check the daemon and retry startup.",
            ),
            Self::ImagePullFailed => formatter.write_str(
                "Could not download the approved execution image from GHCR. Check Docker's registry connectivity and retry startup.",
            ),
            Self::ImageMismatch => formatter.write_str(
                "The execution image does not match the approved digest and native Linux architecture. Pull the approved image for this daemon's platform and retry startup.",
            ),
            Self::TimedOut(phase) => write!(
                formatter,
                "Docker {phase} timed out. Check the local daemon and registry connectivity, then retry startup."
            ),
        }
    }
}

impl std::error::Error for DockerPreflightError {}

// Private contracts for the runtime slice. Remove this allowance with tool integration.
#[allow(dead_code)]
pub(super) mod runtime {
    use std::{fmt, time::Duration};

    use serde::{Deserialize, Serialize};

    pub(crate) struct CodeExecutionRequest {
        pub(crate) code: String,
    }

    #[derive(Serialize, PartialEq, Eq)]
    pub(crate) struct CodeExecutionResult {
        pub(crate) std_out: String,
        pub(crate) std_err: String,
    }

    impl fmt::Debug for CodeExecutionResult {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("CodeExecutionResult { std_out: [REDACTED], std_err: [REDACTED] }")
        }
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub(crate) struct RuntimeProfile {
        pub(crate) python_version: String,
        pub(crate) modules: Vec<String>,
        #[serde(skip)]
        pub(crate) image: String,
    }

    /// A fixed process identity and working directory selected by core.
    #[derive(Clone, Copy, Serialize)]
    pub(crate) struct ExecutionIdentity {
        pub(crate) uid: u32,
        pub(crate) gid: u32,
        pub(crate) directory: &'static str,
    }

    impl ExecutionIdentity {
        pub(crate) fn docker_user(self) -> String {
            format!("{}:{}", self.uid, self.gid)
        }
    }

    /// A writable mount whose byte and inode limits apply to the whole session.
    #[derive(Clone, Copy, Serialize)]
    pub(crate) struct TmpfsPolicy {
        pub(crate) path: &'static str,
        pub(crate) bytes: i64,
        pub(crate) inodes: u64,
    }

    impl TmpfsPolicy {
        pub(crate) fn docker_options(self) -> String {
            format!(
                "rw,nosuid,nodev,noexec,size={},nr_inodes={},uid={},gid={},mode=0700",
                self.bytes,
                self.inodes,
                ExecutionPolicy::WORKER.uid,
                ExecutionPolicy::WORKER.gid,
            )
        }
    }

    /// Fixed host policy shared by container setup and the trusted launcher.
    pub(crate) struct ExecutionPolicy;

    impl ExecutionPolicy {
        pub(crate) const EXECUTION: Duration = Duration::from_secs(30);
        pub(crate) const REQUEST: Duration = Duration::from_secs(5);
        pub(crate) const IMAGE_PULL: Duration = Duration::from_secs(600);
        pub(crate) const HEARTBEAT: Duration = Duration::from_secs(5);
        pub(crate) const KEEPER_LEASE: Duration = Duration::from_secs(60);
        pub(crate) const SOURCE: usize = 64 * 1024;
        pub(crate) const OUTPUT: usize = 64 * 1024;
        pub(crate) const READINESS: usize = 2048;
        pub(crate) const MEMORY: i64 = 128 * 1024 * 1024;
        pub(crate) const SWAP: i64 = 0;
        pub(crate) const CPUS: i64 = 1;
        pub(crate) const SESSIONS: usize = 4;
        pub(crate) const PIDS: i64 = 32;
        pub(crate) const WORKSPACE: TmpfsPolicy = TmpfsPolicy {
            path: "/workspace",
            bytes: 16 * 1024 * 1024,
            inodes: 4096,
        };
        pub(crate) const TEMPORARY: TmpfsPolicy = TmpfsPolicy {
            path: "/tmp",
            bytes: 8 * 1024 * 1024,
            inodes: 1024,
        };
        pub(crate) const SHARED_MEMORY: TmpfsPolicy = TmpfsPolicy {
            path: "/dev/shm",
            bytes: 4 * 1024 * 1024,
            inodes: 256,
        };
        pub(crate) const MOUNTS: [TmpfsPolicy; 3] =
            [Self::WORKSPACE, Self::TEMPORARY, Self::SHARED_MEMORY];
        pub(crate) const KEEPER: ExecutionIdentity = ExecutionIdentity {
            uid: 10000,
            gid: 10000,
            directory: "/",
        };
        pub(crate) const WORKER: ExecutionIdentity = ExecutionIdentity {
            uid: 10001,
            gid: 10001,
            directory: Self::WORKSPACE.path,
        };
        pub(crate) const MODULES: [&str; 7] = [
            "math",
            "decimal",
            "fractions",
            "statistics",
            "itertools",
            "csv",
            "json",
        ];

        /// Serialize only host-selected values for the fixed launcher command.
        pub(crate) fn launcher_json() -> String {
            #[derive(Serialize)]
            struct LauncherPolicy {
                keeper: ExecutionIdentity,
                worker: ExecutionIdentity,
                mounts: [TmpfsPolicy; 3],
                source_bytes: usize,
                memory_bytes: i64,
                swap_bytes: i64,
                cpus: i64,
                pids: i64,
                lease_seconds: u64,
                modules: [&'static str; 7],
            }

            serde_json::to_string(&LauncherPolicy {
                keeper: Self::KEEPER,
                worker: Self::WORKER,
                mounts: Self::MOUNTS,
                source_bytes: Self::SOURCE,
                memory_bytes: Self::MEMORY,
                swap_bytes: Self::SWAP,
                cpus: Self::CPUS,
                pids: Self::PIDS,
                lease_seconds: Self::KEEPER_LEASE.as_secs(),
                modules: Self::MODULES,
            })
            .expect("fixed launcher policy contains only JSON-safe fields")
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Lifecycle {
        Preparing,
        Available,
        Busy,
        Quarantined,
        Closed,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum ExecutionOutcome {
        Timeout,
        OutputLimit,
        ResourceLimit,
        WorkerLost,
        Cancelled,
        CleanupFailed,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum ExecutionError {
        InvalidInput,
        SetupFailure,
        AdmissionFull,
        SessionLost(ExecutionOutcome),
        Closed,
    }

    impl fmt::Display for ExecutionError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(match self {
                Self::InvalidInput => "Python source exceeds the 64 KiB limit.",
                Self::SetupFailure => {
                    "The private Python runtime could not establish its required controls."
                }
                Self::AdmissionFull => {
                    "All four private execution session reservations are in use."
                }
                Self::SessionLost(_) => {
                    "The execution session was lost. Its source will not be replayed."
                }
                Self::Closed => "The private Python runtime is closed.",
            })
        }
    }

    impl std::error::Error for ExecutionError {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_serialization_and_diagnostics_preserve_the_boundary() {
        use super::runtime::*;
        use serde_json::json;
        let result = CodeExecutionResult {
            std_out: "private-output\n".into(),
            std_err: String::new(),
        };
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({"std_out": "private-output\n", "std_err": ""})
        );
        assert!(!format!("{result:?}").contains("private-output"));
        for error in [
            ExecutionError::InvalidInput,
            ExecutionError::SetupFailure,
            ExecutionError::SessionLost(ExecutionOutcome::WorkerLost),
            ExecutionError::Closed,
        ] {
            assert!(!format!("{error:?}: {error}").contains("private-output"));
        }
    }

    #[test]
    fn endpoint_defaults_and_explicit_local_override() {
        let default = DockerEndpoint::from_lookup(&mut |key| {
            Ok((key == "HOME").then(|| "/home/test-user".to_owned()))
        })
        .unwrap();
        #[cfg(target_os = "macos")]
        assert_eq!(
            default.connection_path(),
            "/home/test-user/.docker/run/docker.sock"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(default.connection_path(), "/var/run/docker.sock");
        #[cfg(target_os = "windows")]
        assert_eq!(default.connection_path(), "//./pipe/docker_engine");

        #[cfg(unix)]
        let configured = "unix:///run/user/1000/docker.sock";
        #[cfg(windows)]
        let configured = "npipe:////./pipe/dockerDesktopLinuxEngine";
        let endpoint = DockerEndpoint::from_lookup(&mut |key| {
            Ok((key == "TACHIKOMA_DOCKER_HOST").then(|| configured.to_owned()))
        })
        .unwrap();
        assert_eq!(endpoint.0, configured);
    }

    #[test]
    fn remote_or_malformed_endpoints_are_rejected_without_echoing_values() {
        for endpoint in [
            "",
            "tcp://private-host:2375",
            "http://127.0.0.1:2375",
            "https://private-host:2376",
            "ssh://private-user@private-host",
            "unix://relative-path",
            "npipe:////private-host/pipe/docker_engine",
            "unix:///private-path\n",
        ] {
            let error = DockerEndpoint::parse(endpoint.to_owned()).err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            if !endpoint.is_empty() {
                assert!(!error.to_string().contains(endpoint));
            }
        }
    }

    #[test]
    fn approved_reference_requires_a_ghcr_digest_and_no_mutable_tag() {
        for reference in [
            "ghcr.io/owner/image:latest",
            "ghcr.io/owner/image@sha256:123",
            "docker.io/owner/image@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "ghcr.io/owner/image:tag@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "ghcr.io//image@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(matches!(
                ExecutionImage::from_reference(reference),
                Err(DockerPreflightError::InvalidImageReference)
            ));
        }
        assert!(ExecutionImage::from_reference(
            "ghcr.io/owner/image@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
        ).is_ok());
    }
}
