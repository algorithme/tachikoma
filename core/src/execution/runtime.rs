//! Supervisor-owned sessions. Dropping a caller never drops cleanup ownership.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};

use bollard::{
    Docker,
    container::{AttachContainerResults, LogOutput},
    errors::Error as DockerError,
    exec::{StartExecOptions, StartExecResults},
    models::{ContainerSummaryStateEnum, ExecConfig},
    query_parameters::{
        AttachContainerOptionsBuilder, CreateContainerOptionsBuilder, ListContainersOptionsBuilder,
        RemoveContainerOptionsBuilder,
    },
};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
    time::{MissedTickBehavior, interval, sleep, timeout},
};
use uuid::Uuid;

use super::{
    docker::{
        self, CheckedDocker, OWNER_LABEL, OWNER_VALUE, command, container_config, verify_container,
    },
    models::{DockerEndpoint, runtime::*},
};

type Output = Pin<Box<dyn Stream<Item = Result<LogOutput, DockerError>> + Send>>;
type Input = Pin<Box<dyn AsyncWrite + Send>>;
type Reply = oneshot::Sender<Result<CodeExecutionResult, ExecutionError>>;

/// One logical runtime. A replacement has a new container and generation.
// Remove these API allowances when the code tool becomes its first caller.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct AgentRuntime {
    docker: CheckedDocker,
    supervisor: Arc<ExecutionSupervisor>,
    operation: Mutex<SessionTicket>,
    closed: watch::Sender<bool>,
    profile: RuntimeProfile,
}

#[cfg_attr(not(test), allow(dead_code))]
impl AgentRuntime {
    /// Reserve a private session and prepare its first restricted worker.
    pub(crate) async fn create(endpoint: &DockerEndpoint) -> Result<Self, ExecutionError> {
        let docker = docker::prepare(endpoint)
            .await
            .map_err(|_| ExecutionError::SetupFailure)?;
        reconcile(&docker.client)
            .await
            .map_err(setup("reconciliation"))?;
        Self::with_supervisor(docker, ExecutionSupervisor::shared()).await
    }

    async fn with_supervisor(
        docker: CheckedDocker,
        supervisor: Arc<ExecutionSupervisor>,
    ) -> Result<Self, ExecutionError> {
        let (closed, _) = watch::channel(false);
        let (ticket, ready) = supervisor.start(&docker, closed.subscribe())?;
        let profile = ready.await.map_err(|_| ExecutionError::SetupFailure)??;
        Ok(Self {
            docker,
            supervisor,
            operation: Mutex::new(ticket),
            closed,
            profile,
        })
    }

    pub(crate) fn profile(&self) -> &RuntimeProfile {
        &self.profile
    }

    /// Execute one request. A later call can replace a removed, lost session.
    pub(crate) async fn execute_code(
        &self,
        request: CodeExecutionRequest,
    ) -> Result<CodeExecutionResult, ExecutionError> {
        if *self.closed.borrow() {
            return Err(ExecutionError::Closed);
        }
        if request.code.len() > ExecutionPolicy::SOURCE {
            return Err(ExecutionError::InvalidInput);
        }
        let mut ticket = self.operation.lock().await;
        if *self.closed.borrow() {
            return Err(ExecutionError::Closed);
        }
        if matches!(
            *ticket.state.borrow(),
            Lifecycle::Quarantined | Lifecycle::Closed
        ) {
            // Only a subsequent explicit call reaches this branch. Never replay a failed call.
            ticket.wait_for_cleanup().await?;
            let (replacement, ready) = self
                .supervisor
                .start(&self.docker, self.closed.subscribe())?;
            *ticket = replacement;
            let profile = ready
                .await
                .map_err(|_| lost(ExecutionOutcome::WorkerLost))??;
            if profile != self.profile {
                ticket.cancel.send_replace(true);
                return Err(ExecutionError::SetupFailure);
            }
        }
        tracing::debug!(session = %ticket.generation, "Private Python execution started");
        let (reply, result) = oneshot::channel();
        ticket
            .commands
            .send((request, reply))
            .await
            .map_err(|_| lost(ExecutionOutcome::WorkerLost))?;
        result
            .await
            .map_err(|_| lost(ExecutionOutcome::Cancelled))?
    }

    /// Cancel owned work and confirm removal, or report pending cleanup.
    pub(crate) async fn close(&self) -> Result<(), ExecutionError> {
        self.closed.send_replace(true);
        let mut ticket = self.operation.lock().await;
        ticket.cancel.send_replace(true);
        ticket.wait_for_cleanup().await
    }
}

impl Drop for AgentRuntime {
    fn drop(&mut self) {
        self.closed.send_replace(true);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cleanup {
    Pending,
    Retrying,
    Removed,
}

struct SessionTicket {
    generation: Uuid,
    commands: mpsc::Sender<(CodeExecutionRequest, Reply)>,
    cancel: watch::Sender<bool>,
    state: watch::Receiver<Lifecycle>,
    cleanup: watch::Receiver<Cleanup>,
}

impl SessionTicket {
    async fn wait_for_cleanup(&mut self) -> Result<(), ExecutionError> {
        loop {
            match *self.cleanup.borrow_and_update() {
                Cleanup::Removed => return Ok(()),
                Cleanup::Retrying => return Err(lost(ExecutionOutcome::CleanupFailed)),
                Cleanup::Pending => {}
            }
            self.cleanup
                .changed()
                .await
                .map_err(|_| lost(ExecutionOutcome::CleanupFailed))?;
        }
    }
}

impl Drop for SessionTicket {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}

struct TaskRecord {
    // Store the reservation outside the task. A stopped Tokio executor must not
    // silently release admission while Docker still owns an uncertain container.
    _reservation: OwnedSemaphorePermit,
    _task: JoinHandle<()>,
}

struct ExecutionSupervisor {
    admission: Arc<Semaphore>,
    tasks: StdMutex<HashMap<Uuid, TaskRecord>>,
}

impl ExecutionSupervisor {
    fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<ExecutionSupervisor>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| Arc::new(Self::new())))
    }

    fn new() -> Self {
        Self {
            admission: Arc::new(Semaphore::new(ExecutionPolicy::SESSIONS)),
            tasks: StdMutex::new(HashMap::new()),
        }
    }

    fn start(
        self: &Arc<Self>,
        docker: &CheckedDocker,
        mut closed: watch::Receiver<bool>,
    ) -> Result<
        (
            SessionTicket,
            oneshot::Receiver<Result<RuntimeProfile, ExecutionError>>,
        ),
        ExecutionError,
    > {
        let reservation = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| ExecutionError::AdmissionFull)?;
        let generation = Uuid::new_v4();
        let (commands, receiver) = mpsc::channel(1);
        let (cancel, mut cancelled) = watch::channel(false);
        let (state, state_rx) = watch::channel(Lifecycle::Preparing);
        let (cleanup, cleanup_rx) = watch::channel(Cleanup::Pending);
        let (ready, ready_rx) = oneshot::channel();
        let mut session = Session {
            client: docker.client.clone(),
            image: docker.image.reference().to_owned(),
            name: format!("tachikoma-python-{generation}"),
            container_id: None,
            creation_attempted: false,
            state,
        };
        let supervisor = Arc::clone(self);
        let mut tasks = self.tasks.lock().unwrap();
        let task = tokio::spawn(async move {
            let mut ready = Some(ready);
            let result = tokio::select! {
                biased;
                _ = wait_cancelled(&mut cancelled) => Err(lost(ExecutionOutcome::Cancelled)),
                _ = wait_cancelled(&mut closed) => Err(ExecutionError::Closed),
                result = session.serve(receiver, &mut ready) => result,
            };
            session.state.send_replace(Lifecycle::Quarantined);
            if let Some(ready) = ready.take() {
                let _ = ready.send(Err(result.err().unwrap_or(ExecutionError::SetupFailure)));
            }
            // This task remains supervisor-owned after handle/request cancellation.
            // An ambiguous create followed by 404 is NOT proof of removal.
            loop {
                if session.remove().await.is_ok() {
                    supervisor.tasks.lock().unwrap().remove(&generation);
                    cleanup.send_replace(Cleanup::Removed);
                    if *closed.borrow() {
                        session.state.send_replace(Lifecycle::Closed);
                    }
                    break;
                }
                cleanup.send_replace(Cleanup::Retrying);
                tracing::warn!("Private execution cleanup is pending; reservation retained");
                sleep(ExecutionPolicy::REQUEST).await;
            }
        });
        tasks.insert(
            generation,
            TaskRecord {
                _reservation: reservation,
                _task: task,
            },
        );
        Ok((
            SessionTicket {
                generation,
                commands,
                cancel,
                state: state_rx,
                cleanup: cleanup_rx,
            },
            ready_rx,
        ))
    }
}

struct Session {
    client: Docker,
    image: String,
    name: String,
    container_id: Option<String>,
    creation_attempted: bool,
    state: watch::Sender<Lifecycle>,
}

impl Session {
    async fn serve(
        &mut self,
        mut commands: mpsc::Receiver<(CodeExecutionRequest, Reply)>,
        ready: &mut Option<oneshot::Sender<Result<RuntimeProfile, ExecutionError>>>,
    ) -> Result<(), ExecutionError> {
        let (keeper, mut worker) = timeout(ExecutionPolicy::REQUEST, self.prepare())
            .await
            .map_err(|_| ExecutionError::SetupFailure)??;
        let baseline = worker.readiness.counters.clone();
        self.state.send_replace(Lifecycle::Available);
        ready
            .take()
            .unwrap()
            .send(Ok(worker.readiness.profile.clone()))
            .map_err(|_| lost(ExecutionOutcome::Cancelled))?;
        let operations = async {
            while let Some((request, mut reply)) = commands.recv().await {
                self.state.send_replace(Lifecycle::Busy);
                let execution = async {
                    let result = timeout(
                        ExecutionPolicy::EXECUTION,
                        worker.execute(&self.client, request),
                    )
                    .await
                    .map_err(|_| lost(ExecutionOutcome::Timeout))??;
                    let replacement = timeout(ExecutionPolicy::REQUEST, self.prepare_worker())
                        .await
                        .map_err(|_| lost(ExecutionOutcome::WorkerLost))?
                        .map_err(|_| lost(ExecutionOutcome::WorkerLost))?;
                    if replacement.readiness.counters != baseline {
                        return Err(lost(ExecutionOutcome::ResourceLimit));
                    }
                    Ok((result, replacement))
                };
                let result = tokio::select! {
                    _ = reply.closed() => Err(lost(ExecutionOutcome::Cancelled)),
                    result = execution => result,
                };
                match result {
                    Ok((result, replacement)) => {
                        worker = replacement;
                        self.state.send_replace(Lifecycle::Available);
                        if reply.send(Ok(result)).is_err() {
                            return Err(lost(ExecutionOutcome::Cancelled));
                        }
                    }
                    Err(error) => {
                        self.state.send_replace(Lifecycle::Quarantined);
                        let _ = reply.send(Err(error));
                        return Err(error);
                    }
                }
            }
            Ok(())
        };
        tokio::select! {
            result = heartbeat(keeper) => result,
            result = operations => result,
        }
    }

    async fn prepare(
        &mut self,
    ) -> Result<(AttachContainerResults, PreparedWorker), ExecutionError> {
        let body = container_config(&self.image, &self.name);
        self.creation_attempted = true;
        let response = request(
            self.client.create_container(
                Some(
                    CreateContainerOptionsBuilder::default()
                        .name(&self.name)
                        .build(),
                ),
                body.clone(),
            ),
        )
        .await
        .map_err(setup("container creation"))?;
        self.container_id = Some(response.id);
        if !response.warnings.is_empty() {
            return Err(ExecutionError::SetupFailure);
        }
        let id = self
            .container_id
            .as_deref()
            .ok_or(ExecutionError::SetupFailure)?;
        let keeper = request(
            self.client.attach_container(
                id,
                Some(
                    AttachContainerOptionsBuilder::default()
                        .stdin(true)
                        .stdout(true)
                        .stderr(true)
                        .stream(true)
                        .build(),
                ),
            ),
        )
        .await
        .map_err(setup("keeper attachment"))?;
        request(self.client.start_container(id, None))
            .await
            .map_err(setup("container start"))?;
        let inspection = request(self.client.inspect_container(id, None))
            .await
            .map_err(setup("container inspection"))?;
        verify_container(&inspection, &body).map_err(setup("container controls"))?;
        let worker = self
            .prepare_worker()
            .await
            .map_err(setup("worker preparation"))?;
        Ok((keeper, worker))
    }

    async fn prepare_worker(&self) -> Result<PreparedWorker, ExecutionError> {
        let exec = request(
            self.client.create_exec(
                self.container_id
                    .as_deref()
                    .ok_or(ExecutionError::SetupFailure)?,
                ExecConfig {
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(false),
                    privileged: Some(false),
                    user: Some(ExecutionPolicy::WORKER.docker_user()),
                    working_dir: Some(ExecutionPolicy::WORKER.directory.into()),
                    cmd: Some(command("worker")),
                    ..Default::default()
                },
            ),
        )
        .await?;
        let StartExecResults::Attached { mut output, input } = request(self.client.start_exec(
            &exec.id,
            Some(StartExecOptions {
                detach: false,
                tty: false,
                output_capacity: Some(4096),
            }),
        ))
        .await?
        else {
            return Err(ExecutionError::SetupFailure);
        };
        let mut message = Vec::new();
        loop {
            let Some(Ok(LogOutput::StdOut { message: bytes })) = output.next().await else {
                return Err(ExecutionError::SetupFailure);
            };
            if message.len() + bytes.len() > ExecutionPolicy::READINESS {
                return Err(ExecutionError::SetupFailure);
            }
            message.extend_from_slice(&bytes);
            if message.ends_with(b"\n") {
                break;
            }
        }
        let mut readiness: Readiness = serde_json::from_slice(
            message
                .strip_prefix(b"TK_READY ")
                .ok_or(ExecutionError::SetupFailure)?,
        )
        .map_err(|_| ExecutionError::SetupFailure)?;
        if readiness.profile.modules != ExecutionPolicy::MODULES
            || readiness.profile.python_version.split('.').count() != 3
            || !readiness
                .profile
                .python_version
                .bytes()
                .all(|c| c.is_ascii_digit() || c == b'.')
            || readiness.counters.len() != 2
            || !readiness.counters.contains_key("memory")
            || !readiness.counters.contains_key("pids")
        {
            return Err(ExecutionError::SetupFailure);
        }
        readiness.profile.image.clone_from(&self.image);
        Ok(PreparedWorker {
            id: exec.id,
            input,
            output,
            readiness,
        })
    }

    async fn remove(&self) -> Result<(), ExecutionError> {
        if !self.creation_attempted {
            return Ok(());
        }
        let id = match &self.container_id {
            Some(id) => id.clone(),
            None => {
                // A timed-out create has no returned ID. Confirm ownership before
                // deleting by immutable ID; a name alone can identify another owner.
                let inspected = request(self.client.inspect_container(&self.name, None))
                    .await
                    .map_err(|_| lost(ExecutionOutcome::CleanupFailed))?;
                let labels = inspected
                    .config
                    .and_then(|config| config.labels)
                    .unwrap_or_default();
                if labels.get(OWNER_LABEL).map(String::as_str) != Some(OWNER_VALUE)
                    || labels.get("io.tachikoma.session") != Some(&self.name)
                {
                    return Err(lost(ExecutionOutcome::CleanupFailed));
                }
                inspected.id.ok_or(lost(ExecutionOutcome::CleanupFailed))?
            }
        };
        let result = timeout(
            ExecutionPolicy::REQUEST,
            self.client.remove_container(
                &id,
                Some(
                    RemoveContainerOptionsBuilder::default()
                        .force(true)
                        .v(true)
                        .build(),
                ),
            ),
        )
        .await;
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(DockerError::DockerResponseServerError {
                status_code: 404, ..
            })) => Ok(()),
            _ => Err(lost(ExecutionOutcome::CleanupFailed)),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Readiness {
    profile: RuntimeProfile,
    counters: HashMap<String, HashMap<String, String>>,
}

struct PreparedWorker {
    id: String,
    input: Input,
    output: Output,
    readiness: Readiness,
}

impl PreparedWorker {
    async fn execute(
        mut self,
        docker: &Docker,
        request: CodeExecutionRequest,
    ) -> Result<CodeExecutionResult, ExecutionError> {
        let transfer = async {
            self.input
                .write_all(request.code.as_bytes())
                .await
                .map_err(|_| lost(ExecutionOutcome::WorkerLost))?;
            self.input
                .shutdown()
                .await
                .map_err(|_| lost(ExecutionOutcome::WorkerLost))
        };
        let (_, result) = tokio::try_join!(transfer, collect(&mut self.output))?;
        loop {
            let inspection = request_during_execution(docker.inspect_exec(&self.id)).await?;
            match (inspection.running, inspection.exit_code) {
                (Some(false), Some(0 | 1)) => return Ok(result),
                (Some(false), Some(70 | 137)) => return Err(lost(ExecutionOutcome::ResourceLimit)),
                (Some(false), _) => return Err(lost(ExecutionOutcome::WorkerLost)),
                (Some(true), _) => sleep(Duration::from_millis(20)).await,
                _ => return Err(lost(ExecutionOutcome::WorkerLost)),
            }
        }
    }
}

async fn collect(output: &mut Output) -> Result<CodeExecutionResult, ExecutionError> {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    while let Some(frame) = output.next().await {
        let (buffer, bytes) = match frame.map_err(|_| lost(ExecutionOutcome::WorkerLost))? {
            LogOutput::StdOut { message } => (&mut stdout, message),
            LogOutput::StdErr { message } => (&mut stderr, message),
            _ => return Err(lost(ExecutionOutcome::WorkerLost)),
        };
        // Bollard output_capacity only sizes its initial decoder buffer.
        if bytes.len() > ExecutionPolicy::OUTPUT - buffer.len() {
            return Err(lost(ExecutionOutcome::OutputLimit));
        }
        buffer.extend_from_slice(&bytes);
    }
    Ok(CodeExecutionResult {
        std_out: String::from_utf8_lossy(&stdout).into_owned(),
        std_err: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

async fn heartbeat(mut keeper: AttachContainerResults) -> Result<(), ExecutionError> {
    let mut ticks = interval(ExecutionPolicy::HEARTBEAT);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticks.tick() => {
                timeout(ExecutionPolicy::REQUEST, keeper.input.write_all(b"H")).await
                    .map_err(|_| lost(ExecutionOutcome::WorkerLost))?
                    .map_err(|_| lost(ExecutionOutcome::WorkerLost))?;
            },
            _ = keeper.output.next() => return Err(lost(ExecutionOutcome::WorkerLost)),
        }
    }
}

async fn request<T>(
    future: impl Future<Output = Result<T, DockerError>>,
) -> Result<T, ExecutionError> {
    timeout(ExecutionPolicy::REQUEST, future)
        .await
        .map_err(|_| ExecutionError::SetupFailure)?
        .map_err(|_| ExecutionError::SetupFailure)
}

async fn request_during_execution<T>(
    future: impl Future<Output = Result<T, DockerError>>,
) -> Result<T, ExecutionError> {
    request(future)
        .await
        .map_err(|_| lost(ExecutionOutcome::WorkerLost))
}

fn lost(outcome: ExecutionOutcome) -> ExecutionError {
    ExecutionError::SessionLost(outcome)
}

async fn wait_cancelled(receiver: &mut watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}

fn setup(phase: &'static str) -> impl FnOnce(ExecutionError) -> ExecutionError {
    move |error| {
        tracing::warn!(phase, "Private execution preparation failed");
        error
    }
}

async fn reconcile(docker: &Docker) -> Result<(), ExecutionError> {
    let filters = HashMap::from([(
        "label".to_owned(),
        vec![format!("{OWNER_LABEL}={OWNER_VALUE}")],
    )]);
    let containers = request(
        docker.list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build(),
        )),
    )
    .await?;
    for container in containers {
        let Some(id) = container.id else {
            continue;
        };
        if !matches!(
            container.state,
            Some(ContainerSummaryStateEnum::EXITED | ContainerSummaryStateEnum::DEAD)
        ) || !container
            .labels
            .as_ref()
            .is_some_and(|labels| labels.get(OWNER_LABEL).is_some_and(|v| v == OWNER_VALUE))
        {
            continue;
        }
        // force=false protects a container that became live since listing.
        let removed = timeout(
            ExecutionPolicy::REQUEST,
            docker.remove_container(
                &id,
                Some(
                    RemoveContainerOptionsBuilder::default()
                        .force(false)
                        .v(true)
                        .build(),
                ),
            ),
        )
        .await;
        match removed {
            Ok(Ok(()))
            | Ok(Err(DockerError::DockerResponseServerError {
                status_code: 404 | 409,
                ..
            })) => {}
            _ => return Err(ExecutionError::SetupFailure),
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
