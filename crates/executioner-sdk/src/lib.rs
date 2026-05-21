use anyhow::{bail, Context};
use async_trait::async_trait;
use executioner_core::{
    CreateSessionRequest, CreateSessionResponse, EffectOperation, ExecutionPolicy, HostState,
    NetworkPolicy, ProcessPolicy, Session, SessionState, ToolInvocationCompleted,
    ToolInvocationFailed, ToolInvocationRequest, ToolInvocationResult, ToolResultStatus,
    WorkspaceMode, WorkspaceSpec,
};
use executioner_worker::{ClaimedInvocation, FileBroker, InvocationBroker, ToolHostClient, Worker};
use reqwest::Url;
use serde::Serialize;
use serde_json::{Map, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::task::JoinHandle;
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, SdkError>;

#[derive(Debug, Error)]
pub enum SdkError {
    #[error("invalid environment config: {0}")]
    Config(String),
    #[error("host error: {0}")]
    Host(String),
    #[error("broker error: {0}")]
    Broker(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("worker error: {0}")]
    Worker(String),
    #[error("tool invocation failed: {message}")]
    InvocationFailed { code: String, message: String },
    #[error("timed out waiting for tool invocation result after {timeout:?}: {invocation_id}")]
    Timeout {
        invocation_id: String,
        timeout: Duration,
    },
    #[error("expected JSON object arguments")]
    ExpectedJsonObject,
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct EnvironmentConfig {
    pub backend: BackendConfig,
    pub host: HostConfig,
    pub worker: WorkerConfig,
    pub workspace: WorkspaceConfig,
    pub policy: PolicyConfig,
    pub lifecycle: LifecycleConfig,
    pub submit_timeout: Duration,
}

impl EnvironmentConfig {
    pub fn builder() -> EnvironmentConfigBuilder {
        EnvironmentConfigBuilder::default()
    }

    pub fn local_file(queue_dir: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            backend: BackendConfig::File {
                queue_dir: queue_dir.into(),
            },
            host: HostConfig::InProcess {
                state_dir: state_dir.into(),
            },
            worker: WorkerConfig::InProcess {
                id: "executioner-sdk-worker".to_string(),
                idle_sleep: Duration::from_millis(10),
            },
            workspace: WorkspaceConfig::New,
            policy: PolicyConfig::default(),
            lifecycle: LifecycleConfig::default(),
            submit_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Default)]
pub struct EnvironmentConfigBuilder {
    backend: Option<BackendConfig>,
    host: Option<HostConfig>,
    worker: Option<WorkerConfig>,
    workspace: Option<WorkspaceConfig>,
    policy: Option<PolicyConfig>,
    lifecycle: Option<LifecycleConfig>,
    submit_timeout: Option<Duration>,
}

impl EnvironmentConfigBuilder {
    pub fn file_backend(mut self, queue_dir: impl Into<PathBuf>) -> Self {
        self.backend = Some(BackendConfig::File {
            queue_dir: queue_dir.into(),
        });
        self
    }

    pub fn in_process_host(mut self, state_dir: impl Into<PathBuf>) -> Self {
        self.host = Some(HostConfig::InProcess {
            state_dir: state_dir.into(),
        });
        self
    }

    pub fn http_host(mut self, base_url: impl Into<String>) -> Self {
        self.host = Some(HostConfig::ConnectHttp {
            base_url: base_url.into(),
        });
        self
    }

    pub fn in_process_worker(mut self, id: impl Into<String>) -> Self {
        self.worker = Some(WorkerConfig::InProcess {
            id: id.into(),
            idle_sleep: Duration::from_millis(10),
        });
        self
    }

    pub fn in_process_worker_with_sleep(
        mut self,
        id: impl Into<String>,
        idle_sleep: Duration,
    ) -> Self {
        self.worker = Some(WorkerConfig::InProcess {
            id: id.into(),
            idle_sleep,
        });
        self
    }

    pub fn managed_worker(mut self, id: impl Into<String>) -> Self {
        self.worker = Some(WorkerConfig::Managed {
            id: id.into(),
            idle_sleep: Duration::from_millis(10),
        });
        self
    }

    pub fn managed_worker_with_sleep(
        mut self,
        id: impl Into<String>,
        idle_sleep: Duration,
    ) -> Self {
        self.worker = Some(WorkerConfig::Managed {
            id: id.into(),
            idle_sleep,
        });
        self
    }

    pub fn external_worker(mut self) -> Self {
        self.worker = Some(WorkerConfig::External);
        self
    }

    pub fn new_workspace(mut self) -> Self {
        self.workspace = Some(WorkspaceConfig::New);
        self
    }

    pub fn existing_workspace(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace = Some(WorkspaceConfig::Existing { root: root.into() });
        self
    }

    pub fn policy(mut self, policy: PolicyConfig) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn lifecycle(mut self, lifecycle: LifecycleConfig) -> Self {
        self.lifecycle = Some(lifecycle);
        self
    }

    pub fn submit_timeout(mut self, timeout: Duration) -> Self {
        self.submit_timeout = Some(timeout);
        self
    }

    pub fn build(self) -> Result<EnvironmentConfig> {
        Ok(EnvironmentConfig {
            backend: self
                .backend
                .ok_or_else(|| SdkError::Config("backend is required".to_string()))?,
            host: self
                .host
                .ok_or_else(|| SdkError::Config("host is required".to_string()))?,
            worker: self.worker.unwrap_or_else(|| WorkerConfig::InProcess {
                id: "executioner-sdk-worker".to_string(),
                idle_sleep: Duration::from_millis(10),
            }),
            workspace: self.workspace.unwrap_or(WorkspaceConfig::New),
            policy: self.policy.unwrap_or_default(),
            lifecycle: self.lifecycle.unwrap_or_default(),
            submit_timeout: self.submit_timeout.unwrap_or(Duration::from_secs(30)),
        })
    }
}

#[derive(Debug, Clone)]
pub struct WorkerRuntimeConfig {
    pub backend: BackendConfig,
    pub host: HostConfig,
    pub id: String,
    pub idle_sleep: Duration,
}

impl WorkerRuntimeConfig {
    pub fn builder() -> WorkerRuntimeConfigBuilder {
        WorkerRuntimeConfigBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct WorkerRuntimeConfigBuilder {
    backend: Option<BackendConfig>,
    host: Option<HostConfig>,
    id: Option<String>,
    idle_sleep: Option<Duration>,
}

impl WorkerRuntimeConfigBuilder {
    pub fn file_backend(mut self, queue_dir: impl Into<PathBuf>) -> Self {
        self.backend = Some(BackendConfig::File {
            queue_dir: queue_dir.into(),
        });
        self
    }

    pub fn http_host(mut self, base_url: impl Into<String>) -> Self {
        self.host = Some(HostConfig::ConnectHttp {
            base_url: base_url.into(),
        });
        self
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn idle_sleep(mut self, idle_sleep: Duration) -> Self {
        self.idle_sleep = Some(idle_sleep);
        self
    }

    pub fn build(self) -> Result<WorkerRuntimeConfig> {
        Ok(WorkerRuntimeConfig {
            backend: self
                .backend
                .ok_or_else(|| SdkError::Config("worker backend is required".to_string()))?,
            host: self
                .host
                .ok_or_else(|| SdkError::Config("worker host is required".to_string()))?,
            id: self.id.unwrap_or_else(|| "executioner-worker".to_string()),
            idle_sleep: self.idle_sleep.unwrap_or(Duration::from_millis(250)),
        })
    }
}

#[derive(Debug, Clone)]
pub enum BackendConfig {
    File { queue_dir: PathBuf },
}

#[derive(Debug, Clone)]
pub enum HostConfig {
    InProcess { state_dir: PathBuf },
    ConnectHttp { base_url: String },
}

#[derive(Debug, Clone)]
pub enum WorkerConfig {
    InProcess { id: String, idle_sleep: Duration },
    Managed { id: String, idle_sleep: Duration },
    External,
}

#[derive(Debug, Clone)]
pub enum WorkspaceConfig {
    New,
    Existing { root: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyConfig {
    pub allow_exec: bool,
    pub network_enabled: bool,
    pub read_roots: Vec<String>,
    pub write_roots: Vec<String>,
    pub max_duration_ms: Option<u64>,
    pub max_output_bytes: Option<usize>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            allow_exec: false,
            network_enabled: false,
            read_roots: vec!["/workspace".to_string()],
            write_roots: vec!["/workspace".to_string()],
            max_duration_ms: Some(300_000),
            max_output_bytes: Some(100_000),
        }
    }
}

impl PolicyConfig {
    pub fn allow_exec(mut self, allow_exec: bool) -> Self {
        self.allow_exec = allow_exec;
        self
    }

    pub fn network_enabled(mut self, network_enabled: bool) -> Self {
        self.network_enabled = network_enabled;
        self
    }
}

#[derive(Debug, Clone)]
pub struct LifecycleConfig {
    pub close_behavior: CloseBehavior,
    pub queue_cleanup: QueueCleanup,
    pub ttl_ms: Option<u64>,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            close_behavior: CloseBehavior::DestroySession,
            queue_cleanup: QueueCleanup::Preserve,
            ttl_ms: None,
        }
    }
}

impl LifecycleConfig {
    pub fn close_session() -> Self {
        Self {
            close_behavior: CloseBehavior::CloseSession,
            ..Self::default()
        }
    }

    pub fn destroy_session() -> Self {
        Self::default()
    }

    pub fn delete_queue_on_close(mut self) -> Self {
        self.queue_cleanup = QueueCleanup::DeleteOnClose;
        self
    }

    pub fn ttl_ms(mut self, ttl_ms: u64) -> Self {
        self.ttl_ms = Some(ttl_ms);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseBehavior {
    CloseSession,
    DestroySession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueCleanup {
    Preserve,
    DeleteOnClose,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub tool_name: String,
    pub arguments: Map<String, Value>,
    pub cwd: Option<String>,
    pub invocation_id: Option<String>,
    pub timeout_ms: Option<u64>,
    pub max_output_bytes: Option<usize>,
    pub metadata: Map<String, Value>,
}

impl ToolCall {
    pub fn new(tool_name: impl Into<String>, arguments: Map<String, Value>) -> Self {
        Self {
            tool_name: tool_name.into(),
            arguments,
            cwd: Some("/workspace".to_string()),
            invocation_id: None,
            timeout_ms: None,
            max_output_bytes: None,
            metadata: Map::new(),
        }
    }

    pub fn json(tool_name: impl Into<String>, arguments: Value) -> Result<Self> {
        Ok(Self::new(tool_name, json_object(arguments)?))
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn no_cwd(mut self) -> Self {
        self.cwd = None;
        self
    }

    pub fn invocation_id(mut self, invocation_id: impl Into<String>) -> Self {
        self.invocation_id = Some(invocation_id.into());
        self
    }

    pub fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    pub fn max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = Some(max_output_bytes);
        self
    }

    pub fn metadata(mut self, key: impl Into<String>, value: Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub state: SessionStatus,
    pub workspace: WorkspaceInfo,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub metadata: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Ready,
    Closing,
    Closed,
    Destroyed,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    pub root: String,
    pub logical_root: String,
    pub mode: WorkspaceKind,
    pub fresh: bool,
    pub managed: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    New,
    Existing,
    Snapshot,
    Template,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubmitResult {
    pub invocation_id: String,
    pub tool_name: String,
    pub status: ToolStatus,
    pub output: String,
    pub error: Option<String>,
    pub summary: Option<String>,
    pub effects: Vec<StateEffect>,
    pub duration_ms: u64,
    pub metadata: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Success,
    Error,
    Timeout,
    Cancelled,
    PolicyDenied,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StateEffect {
    pub id: String,
    pub invocation_id: String,
    pub kind: String,
    pub resource_type: String,
    pub uri: String,
    pub operation: EffectKind,
    pub summary: Option<String>,
    pub reversible: bool,
    pub occurred_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    Read,
    Create,
    Update,
    Delete,
    Execute,
}

#[derive(Debug)]
pub struct ExecutionerEnvironment {
    session: SessionInfo,
    backend: Arc<BackendClient>,
    queue_dir: Option<PathBuf>,
    host: Arc<HostBackend>,
    worker: WorkerDriver,
    lifecycle: LifecycleConfig,
    submit_timeout: Duration,
}

#[derive(Debug)]
pub struct ExecutionerWorker {
    task: ManagedWorker,
}

impl ExecutionerWorker {
    pub fn builder() -> WorkerRuntimeConfigBuilder {
        WorkerRuntimeConfig::builder()
    }

    pub fn start(config: WorkerRuntimeConfig) -> Result<Self> {
        let backend = Arc::new(BackendClient::from_config(config.backend)?);
        let host = Arc::new(HostBackend::from_config(config.host)?);
        Ok(Self {
            task: ManagedWorker::spawn(config.id, config.idle_sleep, backend, host),
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.task.shutdown().await
    }
}

impl ExecutionerEnvironment {
    pub fn builder() -> EnvironmentConfigBuilder {
        EnvironmentConfig::builder()
    }

    pub async fn create(config: EnvironmentConfig) -> Result<Self> {
        let backend = Arc::new(BackendClient::from_config(config.backend)?);
        let queue_dir = backend.queue_dir();

        let mut host = HostBackend::from_config(config.host)?;
        let session = host
            .create_session(CreateSessionRequest {
                session_id: None,
                workspace: config.workspace.into_spec(),
                policy: config.policy.into_execution_policy(),
                ttl_ms: config.lifecycle.ttl_ms,
                metadata: Map::new(),
            })
            .await?
            .session;
        let host = Arc::new(host);
        let worker =
            WorkerDriver::from_config(config.worker, Arc::clone(&backend), Arc::clone(&host));

        Ok(Self {
            session: session.into(),
            backend,
            queue_dir,
            host,
            worker,
            lifecycle: config.lifecycle,
            submit_timeout: config.submit_timeout,
        })
    }

    pub fn session(&self) -> &SessionInfo {
        &self.session
    }

    pub async fn submit(&self, call: ToolCall) -> Result<SubmitResult> {
        let invocation_id = call
            .invocation_id
            .unwrap_or_else(|| format!("inv_{}", Uuid::new_v4().simple()));
        let request = ToolInvocationRequest {
            invocation_id: Some(invocation_id.clone()),
            session_id: self.session.id.clone(),
            tool_name: call.tool_name,
            arguments: call.arguments,
            cwd: call.cwd,
            timeout_ms: call.timeout_ms,
            max_output_bytes: call.max_output_bytes,
            idempotency_key: None,
            required_capabilities: vec![],
            metadata: call.metadata,
        };

        self.backend
            .enqueue(&request)
            .map_err(|err| SdkError::Broker(err.to_string()))?;
        self.wait_for_result(&invocation_id).await
    }

    pub async fn close(&self) -> Result<SessionInfo> {
        self.worker.shutdown().await?;

        let session = match self.lifecycle.close_behavior {
            CloseBehavior::DestroySession => self.host.destroy_session(&self.session.id).await?,
            CloseBehavior::CloseSession => self.host.close_session(&self.session.id).await?,
        };

        if self.lifecycle.queue_cleanup == QueueCleanup::DeleteOnClose {
            if let Some(queue_dir) = &self.queue_dir {
                match fs::remove_dir_all(queue_dir) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(SdkError::Io(err)),
                }
            }
        }

        Ok(session.into())
    }

    async fn wait_for_result(&self, invocation_id: &str) -> Result<SubmitResult> {
        let started_at = Instant::now();
        loop {
            if let Some(completed) = self
                .backend
                .read_completed(invocation_id)
                .map_err(|err| SdkError::Broker(err.to_string()))?
            {
                return Ok(completed.result.into());
            }

            if let Some(failed) = self
                .backend
                .read_failed(invocation_id)
                .map_err(|err| SdkError::Broker(err.to_string()))?
            {
                return Err(SdkError::InvocationFailed {
                    code: failed.error.code,
                    message: failed.error.message,
                });
            }

            if started_at.elapsed() >= self.submit_timeout {
                return Err(SdkError::Timeout {
                    invocation_id: invocation_id.to_string(),
                    timeout: self.submit_timeout,
                });
            }

            match &self.worker {
                WorkerDriver::InProcess(worker) => {
                    worker
                        .run_once(&*self.backend, &*self.host)
                        .await
                        .map_err(|err| SdkError::Broker(err.to_string()))?;
                }
                WorkerDriver::Managed(_) | WorkerDriver::External => {}
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[derive(Debug)]
enum WorkerDriver {
    InProcess(Worker),
    Managed(ManagedWorker),
    External,
}

impl WorkerDriver {
    fn from_config(
        config: WorkerConfig,
        backend: Arc<BackendClient>,
        host: Arc<HostBackend>,
    ) -> Self {
        match config {
            WorkerConfig::InProcess { id, idle_sleep } => {
                Self::InProcess(Worker::new(id).with_idle_sleep(idle_sleep))
            }
            WorkerConfig::Managed { id, idle_sleep } => {
                Self::Managed(ManagedWorker::spawn(id, idle_sleep, backend, host))
            }
            WorkerConfig::External => Self::External,
        }
    }

    async fn shutdown(&self) -> Result<()> {
        match self {
            Self::Managed(worker) => worker.shutdown().await,
            Self::InProcess(_) | Self::External => Ok(()),
        }
    }
}

#[derive(Debug)]
struct ManagedWorker {
    task: Mutex<Option<JoinHandle<anyhow::Result<()>>>>,
}

impl ManagedWorker {
    fn spawn(
        id: String,
        idle_sleep: Duration,
        backend: Arc<BackendClient>,
        host: Arc<HostBackend>,
    ) -> Self {
        let worker = Worker::new(id).with_idle_sleep(idle_sleep);
        let task = tokio::spawn(async move { worker.run(&*backend, &*host).await });
        Self {
            task: Mutex::new(Some(task)),
        }
    }

    async fn shutdown(&self) -> Result<()> {
        let task = self
            .task
            .lock()
            .map_err(|_| SdkError::Worker("managed worker lock poisoned".to_string()))?
            .take();
        if let Some(task) = task {
            task.abort();
            match task.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(err)) => Err(SdkError::Worker(err.to_string())),
                Err(err) if err.is_cancelled() => Ok(()),
                Err(err) => Err(SdkError::Worker(err.to_string())),
            }
        } else {
            Ok(())
        }
    }
}

impl Drop for ManagedWorker {
    fn drop(&mut self) {
        if let Ok(mut task) = self.task.lock() {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
    }
}

#[derive(Debug)]
enum BackendClient {
    File(FileBackendClient),
}

impl BackendClient {
    fn from_config(config: BackendConfig) -> Result<Self> {
        match config {
            BackendConfig::File { queue_dir } => Ok(Self::File(FileBackendClient {
                queue_dir: queue_dir.clone(),
                broker: FileBroker::new(queue_dir)
                    .map_err(|err| SdkError::Broker(err.to_string()))?,
            })),
        }
    }

    fn queue_dir(&self) -> Option<PathBuf> {
        match self {
            Self::File(file) => Some(file.queue_dir.clone()),
        }
    }

    fn enqueue(&self, request: &ToolInvocationRequest) -> anyhow::Result<PathBuf> {
        match self {
            Self::File(file) => file.broker.enqueue(request),
        }
    }

    fn read_completed(
        &self,
        invocation_id: &str,
    ) -> anyhow::Result<Option<ToolInvocationCompleted>> {
        match self {
            Self::File(file) => file.broker.read_completed(invocation_id),
        }
    }

    fn read_failed(&self, invocation_id: &str) -> anyhow::Result<Option<ToolInvocationFailed>> {
        match self {
            Self::File(file) => file.broker.read_failed(invocation_id),
        }
    }
}

#[derive(Debug)]
struct FileBackendClient {
    queue_dir: PathBuf,
    broker: FileBroker,
}

#[async_trait]
impl InvocationBroker for BackendClient {
    async fn claim_next(&self, worker_id: &str) -> anyhow::Result<Option<ClaimedInvocation>> {
        match self {
            Self::File(file) => file.broker.claim_next(worker_id).await,
        }
    }

    async fn complete(&self, event: ToolInvocationCompleted) -> anyhow::Result<()> {
        match self {
            Self::File(file) => file.broker.complete(event).await,
        }
    }

    async fn fail(&self, event: ToolInvocationFailed) -> anyhow::Result<()> {
        match self {
            Self::File(file) => file.broker.fail(event).await,
        }
    }
}

#[derive(Debug)]
enum HostBackend {
    InProcess(HostState),
    Http(HttpHostBackend),
}

impl HostBackend {
    fn from_config(config: HostConfig) -> Result<Self> {
        match config {
            HostConfig::InProcess { state_dir } => HostState::new(state_dir)
                .map(Self::InProcess)
                .map_err(|err| SdkError::Host(err.to_string())),
            HostConfig::ConnectHttp { base_url } => Ok(Self::Http(HttpHostBackend::new(base_url)?)),
        }
    }

    async fn create_session(
        &mut self,
        request: CreateSessionRequest,
    ) -> Result<CreateSessionResponse> {
        match self {
            Self::InProcess(state) => state
                .create_session(request)
                .map_err(|err| SdkError::Host(err.to_string())),
            Self::Http(host) => host.create_session(request).await,
        }
    }

    async fn close_session(&self, session_id: &str) -> Result<Session> {
        match self {
            Self::InProcess(state) => state
                .close_session(session_id)
                .map_err(|err| SdkError::Host(err.to_string())),
            Self::Http(host) => host.close_session(session_id).await,
        }
    }

    async fn destroy_session(&self, session_id: &str) -> Result<Session> {
        match self {
            Self::InProcess(state) => state
                .destroy_session(session_id)
                .map_err(|err| SdkError::Host(err.to_string())),
            Self::Http(host) => host.destroy_session(session_id).await,
        }
    }
}

#[async_trait]
impl ToolHostClient for HostBackend {
    async fn execute(
        &self,
        request: ToolInvocationRequest,
    ) -> anyhow::Result<ToolInvocationResult> {
        match self {
            Self::InProcess(state) => Ok(state.execute_invocation(request)?),
            Self::Http(host) => host.execute(request).await,
        }
    }
}

#[derive(Debug)]
struct HttpHostBackend {
    base_url: Url,
    client: reqwest::Client,
}

impl HttpHostBackend {
    fn new(base_url: impl AsRef<str>) -> Result<Self> {
        Ok(Self {
            base_url: Url::parse(base_url.as_ref())
                .map_err(|err| SdkError::Config(format!("invalid host base url: {err}")))?,
            client: reqwest::Client::new(),
        })
    }

    async fn create_session(&self, request: CreateSessionRequest) -> Result<CreateSessionResponse> {
        self.post_json("sessions", &request).await
    }

    async fn close_session(&self, session_id: &str) -> Result<Session> {
        self.post_json(&format!("sessions/{session_id}/close"), &Value::Null)
            .await
    }

    async fn destroy_session(&self, session_id: &str) -> Result<Session> {
        let url = self
            .base_url
            .join(&format!("sessions/{session_id}"))
            .map_err(|err| SdkError::Config(format!("invalid session destroy url: {err}")))?;
        let response = self
            .client
            .delete(url)
            .send()
            .await
            .map_err(|err| SdkError::Transport(err.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(SdkError::Host(format!("host returned {status}: {text}")));
        }
        response
            .json::<Session>()
            .await
            .map_err(|err| SdkError::Transport(err.to_string()))
    }

    async fn execute(
        &self,
        request: ToolInvocationRequest,
    ) -> anyhow::Result<ToolInvocationResult> {
        self.post_json_anyhow(
            &format!("sessions/{}/invocations", request.session_id),
            &request,
        )
        .await
    }

    async fn post_json<T, B>(&self, path: &str, body: &B) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
        B: serde::Serialize + ?Sized,
    {
        self.post_json_anyhow(path, body)
            .await
            .map_err(|err| SdkError::Transport(err.to_string()))
    }

    async fn post_json_anyhow<T, B>(&self, path: &str, body: &B) -> anyhow::Result<T>
    where
        T: serde::de::DeserializeOwned,
        B: serde::Serialize + ?Sized,
    {
        let url = self.base_url.join(path).context("invalid host url")?;
        let response = self.client.post(url).json(body).send().await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!("host returned {status}: {text}");
        }
        Ok(response.json::<T>().await?)
    }
}

impl PolicyConfig {
    fn into_execution_policy(self) -> ExecutionPolicy {
        ExecutionPolicy {
            read_roots: self.read_roots,
            write_roots: self.write_roots,
            process: ProcessPolicy {
                allow_exec: self.allow_exec,
                allowed_commands: vec![],
                denied_commands: vec![],
                max_processes: None,
            },
            network: NetworkPolicy {
                enabled: self.network_enabled,
                allow_hosts: vec![],
                deny_hosts: vec![],
            },
            max_duration_ms: self.max_duration_ms,
            max_output_bytes: self.max_output_bytes,
            ..ExecutionPolicy::default()
        }
    }
}

impl WorkspaceConfig {
    fn into_spec(self) -> WorkspaceSpec {
        match self {
            Self::New => WorkspaceSpec {
                mode: WorkspaceMode::New,
                root: None,
                snapshot_ref: None,
                template_ref: None,
                mount_as_workspace: true,
            },
            Self::Existing { root } => WorkspaceSpec {
                mode: WorkspaceMode::Existing,
                root: Some(root.to_string_lossy().into_owned()),
                snapshot_ref: None,
                template_ref: None,
                mount_as_workspace: true,
            },
        }
    }
}

impl From<Session> for SessionInfo {
    fn from(session: Session) -> Self {
        Self {
            id: session.id,
            state: session.state.into(),
            workspace: WorkspaceInfo {
                root: session.workspace.root,
                logical_root: session.workspace.logical_root,
                mode: session.workspace.mode.into(),
                fresh: session.workspace.fresh,
                managed: session.workspace.managed,
            },
            created_at: session.created_at,
            expires_at: session.expires_at,
            metadata: session.metadata,
        }
    }
}

impl From<SessionState> for SessionStatus {
    fn from(state: SessionState) -> Self {
        match state {
            SessionState::Starting => Self::Starting,
            SessionState::Ready => Self::Ready,
            SessionState::Closing => Self::Closing,
            SessionState::Closed => Self::Closed,
            SessionState::Destroyed => Self::Destroyed,
            SessionState::Failed => Self::Failed,
        }
    }
}

impl From<WorkspaceMode> for WorkspaceKind {
    fn from(mode: WorkspaceMode) -> Self {
        match mode {
            WorkspaceMode::New => Self::New,
            WorkspaceMode::Existing => Self::Existing,
            WorkspaceMode::Snapshot => Self::Snapshot,
            WorkspaceMode::Template => Self::Template,
        }
    }
}

impl From<ToolInvocationResult> for SubmitResult {
    fn from(result: ToolInvocationResult) -> Self {
        Self {
            invocation_id: result.invocation_id,
            tool_name: result.tool_name,
            status: result.status.into(),
            output: result.output,
            error: result.error,
            summary: result.summary,
            effects: result.effects.into_iter().map(Into::into).collect(),
            duration_ms: result.duration_ms,
            metadata: result.metadata,
        }
    }
}

impl From<ToolResultStatus> for ToolStatus {
    fn from(status: ToolResultStatus) -> Self {
        match status {
            ToolResultStatus::Success => Self::Success,
            ToolResultStatus::Error => Self::Error,
            ToolResultStatus::Timeout => Self::Timeout,
            ToolResultStatus::Cancelled => Self::Cancelled,
            ToolResultStatus::PolicyDenied => Self::PolicyDenied,
        }
    }
}

impl From<executioner_core::Effect> for StateEffect {
    fn from(effect: executioner_core::Effect) -> Self {
        Self {
            id: effect.id,
            invocation_id: effect.invocation_id,
            kind: effect.kind,
            resource_type: effect.resource.resource_type,
            uri: effect.resource.uri,
            operation: effect.operation.into(),
            summary: effect.summary,
            reversible: effect.reversible,
            occurred_at: effect.occurred_at,
        }
    }
}

impl From<EffectOperation> for EffectKind {
    fn from(operation: EffectOperation) -> Self {
        match operation {
            EffectOperation::Read => Self::Read,
            EffectOperation::Create => Self::Create,
            EffectOperation::Update => Self::Update,
            EffectOperation::Delete => Self::Delete,
            EffectOperation::Execute => Self::Execute,
        }
    }
}

pub fn json_object(value: Value) -> Result<Map<String, Value>> {
    value
        .as_object()
        .cloned()
        .ok_or(SdkError::ExpectedJsonObject)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[tokio::test]
    async fn local_file_environment_writes_and_reads() {
        let temp = tempfile::TempDir::new().unwrap();
        let env = ExecutionerEnvironment::create(EnvironmentConfig::local_file(
            temp.path().join("queue"),
            temp.path().join("state"),
        ))
        .await
        .unwrap();

        let write = env
            .submit(
                ToolCall::json(
                    "Write",
                    json!({ "path": "hello.txt", "content": "hello from sdk" }),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(write.status, ToolStatus::Success);
        assert_eq!(write.effects.len(), 1);

        let read = env
            .submit(ToolCall::json("Read", json!({ "path": "hello.txt" })).unwrap())
            .await
            .unwrap();

        assert_eq!(read.output, "hello from sdk");
        let workspace_root = PathBuf::from(&env.session().workspace.root);
        assert!(workspace_root.exists());

        let closed = env.close().await.unwrap();
        assert_eq!(closed.state, SessionStatus::Destroyed);
        assert!(!workspace_root.exists());
    }

    #[tokio::test]
    async fn builder_constructs_local_file_environment() {
        let temp = tempfile::TempDir::new().unwrap();
        let config = ExecutionerEnvironment::builder()
            .file_backend(temp.path().join("queue"))
            .in_process_host(temp.path().join("state"))
            .in_process_worker("worker")
            .new_workspace()
            .submit_timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let env = ExecutionerEnvironment::create(config).await.unwrap();
        assert_eq!(env.session().state, SessionStatus::Ready);
        env.close().await.unwrap();
    }

    #[tokio::test]
    async fn managed_worker_processes_queue_without_inline_submit_execution() {
        let temp = tempfile::TempDir::new().unwrap();
        let config = ExecutionerEnvironment::builder()
            .file_backend(temp.path().join("queue"))
            .in_process_host(temp.path().join("state"))
            .managed_worker_with_sleep("managed-worker", Duration::from_millis(1))
            .new_workspace()
            .submit_timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let env = ExecutionerEnvironment::create(config).await.unwrap();
        let write = env
            .submit(
                ToolCall::json(
                    "Write",
                    json!({ "path": "managed.txt", "content": "background worker" }),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(write.status, ToolStatus::Success);

        let read = env
            .submit(ToolCall::json("Read", json!({ "path": "managed.txt" })).unwrap())
            .await
            .unwrap();

        assert_eq!(read.output, "background worker");
        env.close().await.unwrap();
    }

    #[tokio::test]
    async fn external_worker_runtime_processes_environment_submissions_over_transport() {
        let temp = tempfile::TempDir::new().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = HostState::new(temp.path().join("host-state")).unwrap();
        let app = executioner_host::HostServer::new(state).router();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let host_url = format!("http://{addr}/");
        let queue = temp.path().join("queue");

        let worker = ExecutionerWorker::start(
            ExecutionerWorker::builder()
                .file_backend(queue.clone())
                .http_host(host_url.clone())
                .id("external-worker")
                .idle_sleep(Duration::from_millis(1))
                .build()
                .unwrap(),
        )
        .unwrap();

        let env = ExecutionerEnvironment::create(
            ExecutionerEnvironment::builder()
                .file_backend(queue)
                .http_host(host_url)
                .external_worker()
                .new_workspace()
                .submit_timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        )
        .await
        .unwrap();

        let write = env
            .submit(
                ToolCall::json(
                    "Write",
                    json!({ "path": "external.txt", "content": "transport worker" }),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(write.status, ToolStatus::Success);
        let read = env
            .submit(ToolCall::json("Read", json!({ "path": "external.txt" })).unwrap())
            .await
            .unwrap();
        assert_eq!(read.output, "transport worker");

        env.close().await.unwrap();
        worker.shutdown().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn existing_workspace_is_preserved_after_destroy() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();

        let config = EnvironmentConfig::builder()
            .file_backend(temp.path().join("queue"))
            .in_process_host(temp.path().join("state"))
            .existing_workspace(workspace.clone())
            .build()
            .unwrap();

        let env = ExecutionerEnvironment::create(config).await.unwrap();
        env.submit(
            ToolCall::json(
                "Write",
                json!({ "path": "kept.txt", "content": "preserve me" }),
            )
            .unwrap(),
        )
        .await
        .unwrap();

        env.close().await.unwrap();

        assert!(workspace.exists());
        assert_eq!(
            fs::read_to_string(workspace.join("kept.txt")).unwrap(),
            "preserve me"
        );
    }

    #[tokio::test]
    async fn lifecycle_can_delete_queue_on_close() {
        let temp = tempfile::TempDir::new().unwrap();
        let queue = temp.path().join("queue");
        let lifecycle = LifecycleConfig::destroy_session().delete_queue_on_close();
        let config = EnvironmentConfig::builder()
            .file_backend(queue.clone())
            .in_process_host(temp.path().join("state"))
            .lifecycle(lifecycle)
            .build()
            .unwrap();

        let env = ExecutionerEnvironment::create(config).await.unwrap();
        assert!(queue.exists());
        env.close().await.unwrap();
        assert!(!queue.exists());
    }

    #[test]
    fn json_arguments_must_be_objects() {
        let err = ToolCall::json("Read", json!(["not", "an", "object"])).unwrap_err();
        assert!(matches!(err, SdkError::ExpectedJsonObject));
    }
}
