use anyhow::Context;
use async_trait::async_trait;
use executioner_core::{
    ErrorEnvelope, ToolInvocationCompleted, ToolInvocationFailed, ToolInvocationRequest,
    ToolInvocationResult,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

#[async_trait]
pub trait InvocationBroker: Send + Sync {
    async fn claim_next(&self, worker_id: &str) -> anyhow::Result<Option<ClaimedInvocation>>;
    async fn complete(&self, event: ToolInvocationCompleted) -> anyhow::Result<()>;
    async fn fail(&self, event: ToolInvocationFailed) -> anyhow::Result<()>;
}

#[async_trait]
pub trait ToolHostClient: Send + Sync {
    async fn execute(&self, request: ToolInvocationRequest)
        -> anyhow::Result<ToolInvocationResult>;
}

#[derive(Debug, Clone)]
pub struct ClaimedInvocation {
    pub request: ToolInvocationRequest,
    pub attempt_id: String,
    pub lease_token: String,
}

#[derive(Debug, Clone)]
pub struct Worker {
    pub id: String,
    pub idle_sleep: Duration,
}

impl Worker {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            idle_sleep: Duration::from_millis(250),
        }
    }

    pub fn with_idle_sleep(mut self, idle_sleep: Duration) -> Self {
        self.idle_sleep = idle_sleep;
        self
    }

    pub async fn run_once<B, H>(&self, broker: &B, host: &H) -> anyhow::Result<WorkerRunOnce>
    where
        B: InvocationBroker,
        H: ToolHostClient,
    {
        let Some(claim) = broker.claim_next(&self.id).await? else {
            return Ok(WorkerRunOnce::Idle);
        };

        let invocation_id = claim
            .request
            .invocation_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let session_id = claim.request.session_id.clone();

        match host.execute(claim.request).await {
            Ok(result) => {
                broker
                    .complete(ToolInvocationCompleted {
                        event_type: "tool.invocation.completed".to_string(),
                        invocation_id: result.invocation_id.clone(),
                        session_id: result.session_id.clone(),
                        attempt_id: Some(claim.attempt_id),
                        lease_token: Some(claim.lease_token),
                        result,
                        completed_at: format!("{:?}", std::time::SystemTime::now()),
                    })
                    .await?;
                Ok(WorkerRunOnce::Completed)
            }
            Err(err) => {
                broker
                    .fail(ToolInvocationFailed {
                        event_type: "tool.invocation.failed".to_string(),
                        invocation_id,
                        session_id,
                        attempt_id: Some(claim.attempt_id),
                        lease_token: Some(claim.lease_token),
                        error: ErrorEnvelope {
                            code: "host_execute_failed".to_string(),
                            message: err.to_string(),
                            retryable: true,
                        },
                        failed_at: format!("{:?}", std::time::SystemTime::now()),
                    })
                    .await?;
                Ok(WorkerRunOnce::Failed)
            }
        }
    }

    pub async fn run<B, H>(&self, broker: &B, host: &H) -> anyhow::Result<()>
    where
        B: InvocationBroker,
        H: ToolHostClient,
    {
        loop {
            match self.run_once(broker, host).await? {
                WorkerRunOnce::Idle => {
                    tokio::time::sleep(self.idle_sleep).await;
                }
                WorkerRunOnce::Completed | WorkerRunOnce::Failed => {}
            }
        }
    }

    pub async fn run_until_idle<B, H>(
        &self,
        broker: &B,
        host: &H,
        max_idle_ticks: usize,
    ) -> anyhow::Result<WorkerStats>
    where
        B: InvocationBroker,
        H: ToolHostClient,
    {
        let mut stats = WorkerStats::default();
        loop {
            match self.run_once(broker, host).await? {
                WorkerRunOnce::Idle => {
                    stats.idle_ticks += 1;
                    if stats.idle_ticks >= max_idle_ticks {
                        return Ok(stats);
                    }
                    tokio::time::sleep(self.idle_sleep).await;
                }
                WorkerRunOnce::Completed => stats.completed += 1,
                WorkerRunOnce::Failed => stats.failed += 1,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerRunOnce {
    Idle,
    Completed,
    Failed,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorkerStats {
    pub completed: usize,
    pub failed: usize,
    pub idle_ticks: usize,
}

#[derive(Debug, Clone)]
pub struct HttpHostClient {
    base_url: Url,
    client: reqwest::Client,
}

impl HttpHostClient {
    pub fn new(base_url: impl AsRef<str>) -> anyhow::Result<Self> {
        Ok(Self {
            base_url: Url::parse(base_url.as_ref()).context("invalid host base url")?,
            client: reqwest::Client::new(),
        })
    }
}

#[async_trait]
impl ToolHostClient for HttpHostClient {
    async fn execute(
        &self,
        request: ToolInvocationRequest,
    ) -> anyhow::Result<ToolInvocationResult> {
        let url = self
            .base_url
            .join(&format!("sessions/{}/invocations", request.session_id))
            .context("invalid invocation url")?;
        let response = self.client.post(url).json(&request).send().await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("host returned {status}: {text}");
        }
        Ok(response.json::<ToolInvocationResult>().await?)
    }
}

#[derive(Debug, Clone)]
pub struct FileBroker {
    pending_dir: PathBuf,
    claimed_dir: PathBuf,
    completed_dir: PathBuf,
    failed_dir: PathBuf,
}

impl FileBroker {
    pub fn new(queue_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let queue_dir = queue_dir.as_ref();
        let pending_dir = queue_dir.join("pending");
        let claimed_dir = queue_dir.join("claimed");
        let completed_dir = queue_dir.join("completed");
        let failed_dir = queue_dir.join("failed");
        fs::create_dir_all(&pending_dir)?;
        fs::create_dir_all(&claimed_dir)?;
        fs::create_dir_all(&completed_dir)?;
        fs::create_dir_all(&failed_dir)?;
        Ok(Self {
            pending_dir,
            claimed_dir,
            completed_dir,
            failed_dir,
        })
    }

    pub fn enqueue(&self, request: &ToolInvocationRequest) -> anyhow::Result<PathBuf> {
        let invocation_id = request
            .invocation_id
            .clone()
            .unwrap_or_else(|| format!("inv_{}", Uuid::new_v4().simple()));
        let path = self.pending_dir.join(format!("{invocation_id}.json"));
        write_json_atomic(&path, request)?;
        Ok(path)
    }

    pub fn completed_path(&self, invocation_id: &str) -> PathBuf {
        self.completed_dir.join(format!("{invocation_id}.json"))
    }

    pub fn failed_path(&self, invocation_id: &str) -> PathBuf {
        self.failed_dir.join(format!("{invocation_id}.json"))
    }

    pub fn read_completed(
        &self,
        invocation_id: &str,
    ) -> anyhow::Result<Option<ToolInvocationCompleted>> {
        read_json_optional(&self.completed_path(invocation_id))
    }

    pub fn read_failed(&self, invocation_id: &str) -> anyhow::Result<Option<ToolInvocationFailed>> {
        read_json_optional(&self.failed_path(invocation_id))
    }

    fn next_pending(&self) -> anyhow::Result<Option<PathBuf>> {
        let mut entries = fs::read_dir(&self.pending_dir)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .collect::<Vec<_>>();
        entries.sort();
        Ok(entries.into_iter().next())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimEnvelope {
    worker_id: String,
    attempt_id: String,
    lease_token: String,
    claimed_at: String,
    request: ToolInvocationRequest,
}

#[async_trait]
impl InvocationBroker for FileBroker {
    async fn claim_next(&self, worker_id: &str) -> anyhow::Result<Option<ClaimedInvocation>> {
        let Some(path) = self.next_pending()? else {
            return Ok(None);
        };
        let bytes = fs::read(&path)?;
        let request = serde_json::from_slice::<ToolInvocationRequest>(&bytes)
            .with_context(|| format!("invalid invocation file: {}", path.display()))?;
        let invocation_id = request
            .invocation_id
            .clone()
            .unwrap_or_else(|| format!("inv_{}", Uuid::new_v4().simple()));
        let attempt_id = format!("attempt_{}", Uuid::new_v4().simple());
        let lease_token = format!("lease_{}", Uuid::new_v4().simple());
        let claimed_path = self.claimed_dir.join(format!("{invocation_id}.json"));
        let envelope = ClaimEnvelope {
            worker_id: worker_id.to_string(),
            attempt_id: attempt_id.clone(),
            lease_token: lease_token.clone(),
            claimed_at: format!("{:?}", std::time::SystemTime::now()),
            request: request.clone(),
        };
        write_json_atomic(&claimed_path, &envelope)?;
        fs::remove_file(path)?;
        Ok(Some(ClaimedInvocation {
            request,
            attempt_id,
            lease_token,
        }))
    }

    async fn complete(&self, event: ToolInvocationCompleted) -> anyhow::Result<()> {
        let path = self
            .completed_dir
            .join(format!("{}.json", event.invocation_id));
        write_json_atomic(&path, &event)?;
        let claimed_path = self
            .claimed_dir
            .join(format!("{}.json", event.invocation_id));
        let _ = fs::remove_file(claimed_path);
        Ok(())
    }

    async fn fail(&self, event: ToolInvocationFailed) -> anyhow::Result<()> {
        let path = self
            .failed_dir
            .join(format!("{}.json", event.invocation_id));
        write_json_atomic(&path, &event)?;
        let claimed_path = self
            .claimed_dir
            .join(format!("{}.json", event.invocation_id));
        let _ = fs::remove_file(claimed_path);
        Ok(())
    }
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let tmp_path = path.with_extension(format!("json.tmp.{}", Uuid::new_v4().simple()));
    fs::write(&tmp_path, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn read_json_optional<T>(path: &Path) -> anyhow::Result<Option<T>>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use executioner_core::{ToolInvocationResult, ToolResultStatus};
    use serde_json::Map;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct MemoryBroker {
        request: Mutex<Option<ToolInvocationRequest>>,
        completed: Mutex<usize>,
    }

    #[async_trait]
    impl InvocationBroker for Arc<MemoryBroker> {
        async fn claim_next(&self, _worker_id: &str) -> anyhow::Result<Option<ClaimedInvocation>> {
            Ok(self
                .request
                .lock()
                .unwrap()
                .take()
                .map(|request| ClaimedInvocation {
                    request,
                    attempt_id: "attempt".to_string(),
                    lease_token: "lease".to_string(),
                }))
        }

        async fn complete(&self, _event: ToolInvocationCompleted) -> anyhow::Result<()> {
            *self.completed.lock().unwrap() += 1;
            Ok(())
        }

        async fn fail(&self, _event: ToolInvocationFailed) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct EchoHost;

    #[async_trait]
    impl ToolHostClient for EchoHost {
        async fn execute(
            &self,
            request: ToolInvocationRequest,
        ) -> anyhow::Result<ToolInvocationResult> {
            Ok(ToolInvocationResult {
                invocation_id: request.invocation_id.unwrap_or_else(|| "inv".to_string()),
                session_id: request.session_id,
                tool_name: request.tool_name,
                status: ToolResultStatus::Success,
                output: "ok".to_string(),
                error: None,
                summary: None,
                effects: vec![],
                duration_ms: 0,
                metadata: Map::new(),
            })
        }
    }

    #[tokio::test]
    async fn worker_claims_executes_and_completes() {
        let broker = Arc::new(MemoryBroker::default());
        *broker.request.lock().unwrap() = Some(ToolInvocationRequest {
            invocation_id: Some("inv".to_string()),
            session_id: "sess".to_string(),
            tool_name: "Read".to_string(),
            arguments: Map::new(),
            cwd: None,
            timeout_ms: None,
            max_output_bytes: None,
            idempotency_key: None,
            required_capabilities: vec![],
            metadata: Map::new(),
        });

        let worker = Worker::new("worker");
        let result = worker.run_once(&broker, &EchoHost).await.unwrap();

        assert_eq!(result, WorkerRunOnce::Completed);
        assert_eq!(*broker.completed.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn worker_runs_until_idle_after_processing_queue() {
        let broker = Arc::new(MemoryBroker::default());
        *broker.request.lock().unwrap() = Some(ToolInvocationRequest {
            invocation_id: Some("inv".to_string()),
            session_id: "sess".to_string(),
            tool_name: "Read".to_string(),
            arguments: Map::new(),
            cwd: None,
            timeout_ms: None,
            max_output_bytes: None,
            idempotency_key: None,
            required_capabilities: vec![],
            metadata: Map::new(),
        });

        let worker = Worker::new("worker").with_idle_sleep(Duration::from_millis(1));
        let stats = worker.run_until_idle(&broker, &EchoHost, 1).await.unwrap();

        assert_eq!(stats.completed, 1);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.idle_ticks, 1);
    }

    #[tokio::test]
    async fn file_broker_claims_to_claimed_and_completes_to_completed() {
        let temp = tempfile::TempDir::new().unwrap();
        let broker = FileBroker::new(temp.path()).unwrap();
        let request = ToolInvocationRequest {
            invocation_id: Some("inv_file".to_string()),
            session_id: "sess".to_string(),
            tool_name: "Read".to_string(),
            arguments: Map::new(),
            cwd: None,
            timeout_ms: None,
            max_output_bytes: None,
            idempotency_key: None,
            required_capabilities: vec![],
            metadata: Map::new(),
        };

        broker.enqueue(&request).unwrap();
        let claim = broker.claim_next("worker").await.unwrap().unwrap();

        assert_eq!(claim.request.invocation_id.as_deref(), Some("inv_file"));
        assert!(temp.path().join("claimed/inv_file.json").exists());
        assert!(!temp.path().join("pending/inv_file.json").exists());

        broker
            .complete(ToolInvocationCompleted {
                event_type: "tool.invocation.completed".to_string(),
                invocation_id: "inv_file".to_string(),
                session_id: "sess".to_string(),
                attempt_id: Some(claim.attempt_id),
                lease_token: Some(claim.lease_token),
                result: ToolInvocationResult {
                    invocation_id: "inv_file".to_string(),
                    session_id: "sess".to_string(),
                    tool_name: "Read".to_string(),
                    status: ToolResultStatus::Success,
                    output: "ok".to_string(),
                    error: None,
                    summary: None,
                    effects: vec![],
                    duration_ms: 0,
                    metadata: Map::new(),
                },
                completed_at: "now".to_string(),
            })
            .await
            .unwrap();

        assert!(temp.path().join("completed/inv_file.json").exists());
        assert!(!temp.path().join("claimed/inv_file.json").exists());
    }
}
