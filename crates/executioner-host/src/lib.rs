use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use executioner_core::{
    CreateSessionRequest, ErrorEnvelope, ExecutionerError, HostState, Session,
    ToolInvocationRequest, ToolInvocationResult,
};
use serde::Serialize;
use std::net::SocketAddr;

#[derive(Clone)]
pub struct HostServer {
    state: HostState,
}

impl HostServer {
    pub fn new(state: HostState) -> Self {
        Self { state }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/sessions", post(create_session))
            .route(
                "/sessions/{session_id}",
                get(get_session).delete(delete_session),
            )
            .route("/sessions/{session_id}/close", post(close_session))
            .route(
                "/sessions/{session_id}/invocations",
                post(execute_invocation),
            )
            .route("/sessions/{session_id}/effects", get(get_effects))
            .with_state(self.state)
    }
}

pub async fn serve(state: HostState, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, HostServer::new(state).router()).await?;
    Ok(())
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn create_session(
    State(state): State<HostState>,
    Json(request): Json<CreateSessionRequest>,
) -> Result<Json<executioner_core::CreateSessionResponse>, ApiError> {
    Ok(Json(state.create_session(request)?))
}

async fn get_session(
    State(state): State<HostState>,
    Path(session_id): Path<String>,
) -> Result<Json<Session>, ApiError> {
    Ok(Json(state.get_session(&session_id)?))
}

async fn close_session(
    State(state): State<HostState>,
    Path(session_id): Path<String>,
) -> Result<Json<Session>, ApiError> {
    Ok(Json(state.close_session(&session_id)?))
}

async fn delete_session(
    State(state): State<HostState>,
    Path(session_id): Path<String>,
) -> Result<Json<Session>, ApiError> {
    Ok(Json(state.destroy_session(&session_id)?))
}

async fn execute_invocation(
    State(state): State<HostState>,
    Path(session_id): Path<String>,
    Json(mut request): Json<ToolInvocationRequest>,
) -> Result<Json<ToolInvocationResult>, ApiError> {
    request.session_id = session_id;
    Ok(Json(state.execute_invocation(request)?))
}

async fn get_effects(
    State(state): State<HostState>,
    Path(session_id): Path<String>,
) -> Result<Json<Vec<executioner_core::Effect>>, ApiError> {
    Ok(Json(state.effects(&session_id)?))
}

#[derive(Debug)]
struct ApiError(ExecutionerError);

impl From<ExecutionerError> for ApiError {
    fn from(value: ExecutionerError) -> Self {
        Self(value)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0 {
            ExecutionerError::SessionNotFound(_) => StatusCode::NOT_FOUND,
            ExecutionerError::PolicyDenied(_) => StatusCode::FORBIDDEN,
            ExecutionerError::InvalidRequest(_) | ExecutionerError::SessionNotReady(_) => {
                StatusCode::BAD_REQUEST
            }
            ExecutionerError::ToolNotFound(_) => StatusCode::NOT_FOUND,
            ExecutionerError::Io(_) | ExecutionerError::Json(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };

        let body = ErrorBody {
            error: ErrorEnvelope {
                code: self.0.code().to_string(),
                message: self.0.to_string(),
                retryable: false,
            },
        };
        (status, Json(body)).into_response()
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: ErrorEnvelope,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use executioner_core::{
        ExecutionPolicy, NetworkPolicy, ProcessPolicy, WorkspaceMode, WorkspaceSpec,
    };
    use serde_json::json;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn policy() -> ExecutionPolicy {
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

    #[tokio::test]
    async fn creates_session_over_http_router() {
        let temp = TempDir::new().unwrap();
        let app = HostServer::new(HostState::new(temp.path()).unwrap()).router();
        let body = serde_json::to_vec(&executioner_core::CreateSessionRequest {
            session_id: None,
            workspace: WorkspaceSpec {
                mode: WorkspaceMode::New,
                root: None,
                snapshot_ref: None,
                template_ref: None,
                mount_as_workspace: true,
            },
            policy: policy(),
            ttl_ms: None,
            metadata: serde_json::Map::new(),
        })
        .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn runs_write_then_read_over_router() {
        let temp = TempDir::new().unwrap();
        let state = HostState::new(temp.path()).unwrap();
        let session = state
            .create_session(executioner_core::CreateSessionRequest {
                session_id: Some("sess".to_string()),
                workspace: WorkspaceSpec {
                    mode: WorkspaceMode::New,
                    root: None,
                    snapshot_ref: None,
                    template_ref: None,
                    mount_as_workspace: true,
                },
                policy: policy(),
                ttl_ms: None,
                metadata: serde_json::Map::new(),
            })
            .unwrap()
            .session;

        let app = HostServer::new(state).router();
        let write_body = json!({
            "sessionId": session.id,
            "toolName": "Write",
            "arguments": { "path": "hello.txt", "content": "hello" },
            "cwd": "/workspace"
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/sessions/sess/invocations")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&write_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
