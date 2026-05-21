use thiserror::Error;

pub type Result<T> = std::result::Result<T, ExecutionerError>;

#[derive(Debug, Error)]
pub enum ExecutionerError {
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("session is not ready: {0}")]
    SessionNotReady(String),
    #[error("policy denied: {0}")]
    PolicyDenied(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl ExecutionerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::SessionNotFound(_) => "session_not_found",
            Self::SessionNotReady(_) => "session_not_ready",
            Self::PolicyDenied(_) => "policy_denied",
            Self::InvalidRequest(_) => "invalid_request",
            Self::ToolNotFound(_) => "tool_not_found",
            Self::Io(_) => "io_error",
            Self::Json(_) => "json_error",
        }
    }
}
