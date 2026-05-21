use executioner_core::{
    CreateSessionRequest, ExecutionPolicy, HostState, NetworkPolicy, ProcessPolicy,
    ToolInvocationRequest, ToolResultStatus, WorkspaceMode, WorkspaceSpec,
};
use serde_json::{json, Map, Value};
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

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

fn create_session(host: &HostState) -> executioner_core::Session {
    host.create_session(CreateSessionRequest {
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
        metadata: Map::new(),
    })
    .unwrap()
    .session
}

fn create_existing_session(host: &HostState, root: &std::path::Path) -> executioner_core::Session {
    host.create_session(CreateSessionRequest {
        session_id: Some("sess_existing".to_string()),
        workspace: WorkspaceSpec {
            mode: WorkspaceMode::Existing,
            root: Some(root.to_string_lossy().into_owned()),
            snapshot_ref: None,
            template_ref: None,
            mount_as_workspace: true,
        },
        policy: policy(),
        ttl_ms: None,
        metadata: Map::new(),
    })
    .unwrap()
    .session
}

fn invoke(session_id: &str, tool_name: &str, arguments: Value) -> ToolInvocationRequest {
    ToolInvocationRequest {
        invocation_id: Some(format!("inv_{tool_name}")),
        session_id: session_id.to_string(),
        tool_name: tool_name.to_string(),
        arguments: arguments.as_object().cloned().unwrap(),
        cwd: Some("/workspace".to_string()),
        timeout_ms: None,
        max_output_bytes: None,
        idempotency_key: None,
        required_capabilities: vec![],
        metadata: Map::new(),
    }
}

#[test]
fn write_creates_file_with_metadata_and_effect() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = create_session(&host);

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Write",
            json!({ "path": "new.txt", "content": "Hello, World!" }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(result.output.contains("Created /workspace/new.txt"));
    assert_eq!(result.metadata["action"], "write");
    assert_eq!(result.metadata["atomic"], true);
    assert_eq!(result.effects.len(), 1);
    assert_eq!(result.effects[0].kind, "file.write");
    assert_eq!(result.effects[0].resource.uri, "file:///workspace/new.txt");
    assert_eq!(
        fs::read_to_string(format!("{}/new.txt", session.workspace.root)).unwrap(),
        "Hello, World!"
    );
}

#[test]
fn write_creates_parent_directories_and_preserves_unicode() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = create_session(&host);
    let content = "Japanese 日本語 and emoji 🎉";

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Write",
            json!({ "path": "a/b/c/unicode.txt", "content": content }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        fs::read_to_string(format!("{}/a/b/c/unicode.txt", session.workspace.root)).unwrap(),
        content
    );
}

#[test]
fn write_fails_if_file_exists_without_mutating() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = create_session(&host);
    fs::write(format!("{}/existing.txt", session.workspace.root), "old").unwrap();

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Write",
            json!({ "path": "existing.txt", "content": "new" }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(result.error.unwrap().contains("already exists"));
    assert_eq!(
        fs::read_to_string(format!("{}/existing.txt", session.workspace.root)).unwrap(),
        "old"
    );
    assert!(result.effects.is_empty());
}

#[test]
fn write_rejects_paths_outside_workspace() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = create_session(&host);

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Write",
            json!({ "path": "/tmp/escape.txt", "content": "bad" }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::PolicyDenied);
    assert!(result.error.unwrap().contains("absolute host paths"));
}

#[test]
fn write_rejects_missing_content_as_tool_error() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = create_session(&host);

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Write",
            json!({ "path": "missing.txt" }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(result.error.unwrap().contains("content"));
}

#[test]
fn read_returns_file_content_and_effect() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = create_session(&host);
    fs::write(
        format!("{}/test.txt", session.workspace.root),
        "Line 1\nLine 2",
    )
    .unwrap();

    let result = host
        .execute_invocation(invoke(&session.id, "Read", json!({ "path": "test.txt" })))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(result.output, "Line 1\nLine 2");
    assert_eq!(result.metadata["action"], "read");
    assert_eq!(result.effects.len(), 1);
    assert_eq!(result.effects[0].kind, "file.read");
}

#[test]
fn read_truncates_large_files() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = create_session(&host);
    fs::write(
        format!("{}/large.txt", session.workspace.root),
        "x".repeat(1000),
    )
    .unwrap();

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Read",
            json!({ "path": "large.txt", "maxBytes": 100 }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(result.output.contains("[truncated"));
    assert!(result.output.len() < 200);
}

#[test]
fn read_supports_line_ranges() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = create_session(&host);
    fs::write(
        format!("{}/lines.txt", session.workspace.root),
        "a\nb\nc\nd",
    )
    .unwrap();

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Read",
            json!({ "path": "lines.txt", "startLine": 2, "endLine": 3 }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(result.output, "// Lines 2-3 of 4 total\nb\nc");
}

#[test]
fn read_reports_missing_file_without_effects() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = create_session(&host);

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Read",
            json!({ "path": "missing.txt" }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(result.error.unwrap().contains("File not found"));
    assert!(result.effects.is_empty());
}

#[test]
fn read_rejects_symlink_escape() {
    #[cfg(unix)]
    {
        let temp = TempDir::new().unwrap();
        let host = HostState::new(temp.path()).unwrap();
        let session = create_session(&host);
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "secret").unwrap();
        std::os::unix::fs::symlink(&outside, format!("{}/link", session.workspace.root)).unwrap();

        let result = host
            .execute_invocation(invoke(&session.id, "Read", json!({ "path": "link" })))
            .unwrap();

        assert_eq!(result.status, ToolResultStatus::PolicyDenied);
        assert!(result.effects.is_empty());
    }
}

#[test]
fn closed_sessions_reject_invocations() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = create_session(&host);
    host.close_session(&session.id).unwrap();

    let err = host
        .execute_invocation(invoke(
            &session.id,
            "Write",
            json!({ "path": "new.txt", "content": "nope" }),
        ))
        .unwrap_err();

    assert!(err.to_string().contains("not ready"));
}

#[test]
fn destroy_removes_managed_workspace() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = create_session(&host);
    let root = session.workspace.root.clone();

    host.destroy_session(&session.id).unwrap();

    assert!(!std::path::Path::new(&root).exists());
}

#[test]
fn destroy_does_not_remove_existing_workspace() {
    let temp = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let host = HostState::new(state.path()).unwrap();
    fs::write(temp.path().join("kept.txt"), "kept").unwrap();
    let session = create_existing_session(&host, temp.path());

    host.destroy_session(&session.id).unwrap();

    assert!(temp.path().exists());
    assert_eq!(
        fs::read_to_string(temp.path().join("kept.txt")).unwrap(),
        "kept"
    );
}

#[test]
fn ttl_expiry_removes_managed_workspace_and_rejects_late_access() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = host
        .create_session(CreateSessionRequest {
            session_id: Some("sess_ttl".to_string()),
            workspace: WorkspaceSpec {
                mode: WorkspaceMode::New,
                root: None,
                snapshot_ref: None,
                template_ref: None,
                mount_as_workspace: true,
            },
            policy: policy(),
            ttl_ms: Some(1),
            metadata: Map::new(),
        })
        .unwrap()
        .session;
    let root = session.workspace.root.clone();

    std::thread::sleep(Duration::from_millis(5));

    let err = host.get_session(&session.id).unwrap_err();
    assert!(err.to_string().contains("session not found"));
    assert!(!std::path::Path::new(&root).exists());
}
