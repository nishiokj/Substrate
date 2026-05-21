use anyhow::Context;
use clap::{Parser, Subcommand};
use executioner_core::{
    CreateSessionRequest, ExecutionPolicy, NetworkPolicy, ProcessPolicy, ToolInvocationRequest,
    WorkspaceMode, WorkspaceSpec,
};
use executioner_host::serve;
use executioner_sdk::{ExecutionerEnvironment, ToolCall};
use executioner_worker::{FileBroker, HttpHostClient, Worker};
use serde_json::{Map, Value};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(name = "executioner")]
#[command(about = "Standalone agent tool execution substrate")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Host {
        #[arg(long, default_value = "127.0.0.1:8765")]
        addr: SocketAddr,
        #[arg(long, default_value = "/tmp/executioner")]
        state_dir: PathBuf,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Invoke {
        #[arg(long, default_value = "http://127.0.0.1:8765/")]
        host_url: String,
        #[arg(long)]
        session_id: String,
        #[arg(long)]
        tool: String,
        #[arg(long)]
        args_json: String,
        #[arg(long)]
        cwd: Option<String>,
    },
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    Create {
        #[arg(long, default_value = "http://127.0.0.1:8765/")]
        host_url: String,
        #[arg(long, default_value = "new")]
        mode: String,
        #[arg(long)]
        root: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum WorkerCommand {
    Run {
        #[arg(long, default_value = "worker")]
        id: String,
        #[arg(long, default_value = "http://127.0.0.1:8765/")]
        host_url: String,
        #[arg(long)]
        queue_dir: PathBuf,
        #[arg(long, default_value_t = 250)]
        idle_sleep_ms: u64,
    },
    RunOnce {
        #[arg(long, default_value = "worker")]
        id: String,
        #[arg(long, default_value = "http://127.0.0.1:8765/")]
        host_url: String,
        #[arg(long)]
        queue_dir: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum EnvCommand {
    Smoke {
        #[arg(long, default_value = "/tmp/executioner-env-queue")]
        queue_dir: PathBuf,
        #[arg(long, default_value = "/tmp/executioner-env-state")]
        state_dir: PathBuf,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Host { addr, state_dir } => {
            let state = executioner_core::HostState::new(state_dir)?;
            serve(state, addr).await?;
        }
        Command::Session { command } => match command {
            SessionCommand::Create {
                host_url,
                mode,
                root,
            } => {
                let request = CreateSessionRequest {
                    session_id: None,
                    workspace: WorkspaceSpec {
                        mode: parse_workspace_mode(&mode)?,
                        root,
                        snapshot_ref: None,
                        template_ref: None,
                        mount_as_workspace: true,
                    },
                    policy: default_policy(),
                    ttl_ms: None,
                    metadata: Map::new(),
                };
                let response: Value = reqwest::Client::new()
                    .post(format!("{}sessions", normalize_url(&host_url)))
                    .json(&request)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                println!("{}", serde_json::to_string_pretty(&response)?);
            }
        },
        Command::Invoke {
            host_url,
            session_id,
            tool,
            args_json,
            cwd,
        } => {
            let arguments: Map<String, Value> =
                serde_json::from_str(&args_json).context("--args-json must be a JSON object")?;
            let request = ToolInvocationRequest {
                invocation_id: None,
                session_id: session_id.clone(),
                tool_name: tool,
                arguments,
                cwd,
                timeout_ms: None,
                max_output_bytes: None,
                idempotency_key: None,
                required_capabilities: vec![],
                metadata: Map::new(),
            };
            let response: Value = reqwest::Client::new()
                .post(format!(
                    "{}sessions/{}/invocations",
                    normalize_url(&host_url),
                    session_id
                ))
                .json(&request)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        Command::Worker { command } => match command {
            WorkerCommand::Run {
                id,
                host_url,
                queue_dir,
                idle_sleep_ms,
            } => {
                let worker = Worker::new(id).with_idle_sleep(Duration::from_millis(idle_sleep_ms));
                let broker = FileBroker::new(queue_dir)?;
                let host = HttpHostClient::new(host_url)?;
                worker.run(&broker, &host).await?;
            }
            WorkerCommand::RunOnce {
                id,
                host_url,
                queue_dir,
            } => {
                let worker = Worker::new(id);
                let broker = FileBroker::new(queue_dir)?;
                let host = HttpHostClient::new(host_url)?;
                let result = worker.run_once(&broker, &host).await?;
                println!("{result:?}");
            }
        },
        Command::Env { command } => match command {
            EnvCommand::Smoke {
                queue_dir,
                state_dir,
                workspace,
            } => {
                let mut builder = ExecutionerEnvironment::builder()
                    .file_backend(queue_dir)
                    .in_process_host(state_dir)
                    .managed_worker_with_sleep("env-smoke-worker", Duration::from_millis(1));
                builder = if let Some(root) = workspace {
                    builder.existing_workspace(root)
                } else {
                    builder.new_workspace()
                };

                let env = ExecutionerEnvironment::create(builder.build()?).await?;
                env.submit(ToolCall::new(
                    "Write",
                    object(serde_json::json!({
                        "path": "executioner-smoke.txt",
                        "content": "hello from executioner sdk"
                    }))?,
                ))
                .await?;
                let result = env
                    .submit(ToolCall::new(
                        "Read",
                        object(serde_json::json!({ "path": "executioner-smoke.txt" }))?,
                    ))
                    .await?;
                let session = env.close().await?;

                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "session": session,
                        "result": result,
                    }))?
                );
            }
        },
    }
    Ok(())
}

fn default_policy() -> ExecutionPolicy {
    ExecutionPolicy {
        read_roots: vec!["/workspace".to_string()],
        write_roots: vec!["/workspace".to_string()],
        process: ProcessPolicy {
            allow_exec: false,
            allowed_commands: vec![],
            denied_commands: vec![],
            max_processes: None,
        },
        network: NetworkPolicy {
            enabled: false,
            allow_hosts: vec![],
            deny_hosts: vec![],
        },
        ..ExecutionPolicy::default()
    }
}

fn parse_workspace_mode(mode: &str) -> anyhow::Result<WorkspaceMode> {
    match mode {
        "new" => Ok(WorkspaceMode::New),
        "existing" => Ok(WorkspaceMode::Existing),
        other => anyhow::bail!("unsupported workspace mode: {other}"),
    }
}

fn normalize_url(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

fn object(value: Value) -> anyhow::Result<Map<String, Value>> {
    value.as_object().cloned().context("expected a JSON object")
}
