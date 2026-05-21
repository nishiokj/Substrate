use crate::effects::now_string;
use crate::error::{ExecutionerError, Result};
use crate::protocol::{
    CreateSessionRequest, CreateSessionResponse, Session, SessionState, ToolInvocationRequest,
    ToolInvocationResult, WorkspaceBinding, WorkspaceMode,
};
use crate::tools::{bash, edit_file, glob_files, grep_files, read_file, write_file};
use serde_json::Map;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct HostState {
    inner: Arc<Mutex<HostInner>>,
}

#[derive(Debug)]
struct HostInner {
    base_dir: PathBuf,
    sessions: HashMap<String, SessionRecord>,
    effects: HashMap<String, Vec<crate::protocol::Effect>>,
}

#[derive(Debug, Clone)]
struct SessionRecord {
    session: Session,
    expires_at: Option<SystemTime>,
}

impl HostState {
    pub fn new(base_dir: impl AsRef<Path>) -> Result<Self> {
        let base_dir = base_dir.as_ref().to_path_buf();
        fs::create_dir_all(&base_dir)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(HostInner {
                base_dir,
                sessions: HashMap::new(),
                effects: HashMap::new(),
            })),
        })
    }

    pub fn create_session(&self, request: CreateSessionRequest) -> Result<CreateSessionResponse> {
        let mut inner = self.lock()?;
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| format!("sess_{}", Uuid::new_v4().simple()));

        inner.purge_expired_sessions()?;
        if inner.sessions.contains_key(&session_id) {
            return Err(ExecutionerError::InvalidRequest(format!(
                "session already exists: {session_id}"
            )));
        }

        let (root, fresh, managed) = match request.workspace.mode {
            WorkspaceMode::New => {
                let root = inner.base_dir.join(&session_id).join("workspace");
                fs::create_dir_all(&root)?;
                (root.canonicalize()?, true, true)
            }
            WorkspaceMode::Existing => {
                let root = request.workspace.root.as_ref().ok_or_else(|| {
                    ExecutionerError::InvalidRequest(
                        "workspace.root is required for existing sessions".to_string(),
                    )
                })?;
                let root = PathBuf::from(root);
                if !root.is_dir() {
                    return Err(ExecutionerError::InvalidRequest(format!(
                        "workspace root is not a directory: {}",
                        root.display()
                    )));
                }
                (root.canonicalize()?, false, false)
            }
            WorkspaceMode::Snapshot | WorkspaceMode::Template => {
                return Err(ExecutionerError::InvalidRequest(
                    "snapshot/template workspaces are protocol states but not implemented yet"
                        .to_string(),
                ));
            }
        };

        let created_at = now_string();
        let expires_at_time = request
            .ttl_ms
            .map(|ttl_ms| SystemTime::now() + Duration::from_millis(ttl_ms));
        let session = Session {
            id: session_id.clone(),
            state: SessionState::Ready,
            workspace: WorkspaceBinding {
                root: root.to_string_lossy().into_owned(),
                logical_root: "/workspace".to_string(),
                mode: request.workspace.mode,
                fresh,
                managed,
            },
            policy: request.policy,
            metadata: request.metadata,
            created_at,
            expires_at: expires_at_time.map(|expires_at| format!("{expires_at:?}")),
        };

        inner.sessions.insert(
            session_id,
            SessionRecord {
                session: session.clone(),
                expires_at: expires_at_time,
            },
        );

        Ok(CreateSessionResponse { session })
    }

    pub fn get_session(&self, session_id: &str) -> Result<Session> {
        let mut inner = self.lock()?;
        inner.purge_expired_sessions()?;
        Ok(inner
            .sessions
            .get(session_id)
            .ok_or_else(|| ExecutionerError::SessionNotFound(session_id.to_string()))?
            .session
            .clone())
    }

    pub fn close_session(&self, session_id: &str) -> Result<Session> {
        let mut inner = self.lock()?;
        inner.purge_expired_sessions()?;
        let record = inner
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| ExecutionerError::SessionNotFound(session_id.to_string()))?;
        record.session.state = SessionState::Closed;
        Ok(record.session.clone())
    }

    pub fn destroy_session(&self, session_id: &str) -> Result<Session> {
        let mut inner = self.lock()?;
        inner.purge_expired_sessions()?;
        let mut record = inner
            .sessions
            .remove(session_id)
            .ok_or_else(|| ExecutionerError::SessionNotFound(session_id.to_string()))?;
        record.session.state = SessionState::Destroyed;
        cleanup_managed_workspace(&record.session);
        inner.effects.remove(session_id);
        Ok(record.session)
    }

    pub fn execute_invocation(
        &self,
        request: ToolInvocationRequest,
    ) -> Result<ToolInvocationResult> {
        let session = self.get_session(&request.session_id)?;
        if session.state != SessionState::Ready {
            return Err(ExecutionerError::SessionNotReady(request.session_id));
        }

        let result = match request.tool_name.as_str() {
            "Read" | "read" => read_file(&session, request)?,
            "Write" | "write" => write_file(&session, request)?,
            "Edit" | "edit" => edit_file(&session, request)?,
            "Bash" | "bash" => bash(&session, request)?,
            "Glob" | "glob" => glob_files(&session, request)?,
            "Grep" | "grep" => grep_files(&session, request)?,
            other => return Err(ExecutionerError::ToolNotFound(other.to_string())),
        };

        let mut inner = self.lock()?;
        inner
            .effects
            .entry(result.session_id.clone())
            .or_default()
            .extend(result.effects.clone());
        Ok(result)
    }

    pub fn effects(&self, session_id: &str) -> Result<Vec<crate::protocol::Effect>> {
        let mut inner = self.lock()?;
        inner.purge_expired_sessions()?;
        if !inner.sessions.contains_key(session_id) {
            return Err(ExecutionerError::SessionNotFound(session_id.to_string()));
        }
        Ok(inner.effects.get(session_id).cloned().unwrap_or_default())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HostInner>> {
        self.inner
            .lock()
            .map_err(|_| ExecutionerError::InvalidRequest("host state lock poisoned".to_string()))
    }
}

pub fn empty_metadata() -> Map<String, serde_json::Value> {
    Map::new()
}

impl HostInner {
    fn purge_expired_sessions(&mut self) -> Result<()> {
        let now = SystemTime::now();
        let expired = self
            .sessions
            .iter()
            .filter_map(|(session_id, record)| {
                let expires_at = record.expires_at?;
                if expires_at <= now {
                    Some(session_id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        for session_id in expired {
            if let Some(mut record) = self.sessions.remove(&session_id) {
                record.session.state = SessionState::Destroyed;
                cleanup_managed_workspace(&record.session);
                self.effects.remove(&session_id);
            }
        }

        Ok(())
    }
}

fn cleanup_managed_workspace(session: &Session) {
    if session.workspace.managed {
        let workspace_root = PathBuf::from(&session.workspace.root);
        if let Some(session_dir) = workspace_root.parent() {
            let _ = fs::remove_dir_all(session_dir);
        }
    }
}
