use executioner_core::{
    CreateSessionRequest, ExecutionPolicy, HostState, NetworkPolicy, ProcessPolicy,
    ToolInvocationRequest, ToolResultStatus, WorkspaceMode, WorkspaceSpec,
};
use serde_json::{json, Map, Value};
use std::fs;
use tempfile::TempDir;

fn policy(allow_exec: bool, allow_network: bool) -> ExecutionPolicy {
    ExecutionPolicy {
        read_roots: vec!["/workspace".to_string()],
        write_roots: vec!["/workspace".to_string()],
        process: ProcessPolicy {
            allow_exec,
            allowed_commands: vec![],
            denied_commands: vec!["rm -rf /".to_string()],
            max_processes: None,
        },
        network: NetworkPolicy {
            enabled: allow_network,
            allow_hosts: vec![],
            deny_hosts: vec![],
        },
        ..ExecutionPolicy::default()
    }
}

fn session(host: &HostState, allow_exec: bool, allow_network: bool) -> executioner_core::Session {
    session_with_policy(host, policy(allow_exec, allow_network))
}

fn session_with_policy(host: &HostState, policy: ExecutionPolicy) -> executioner_core::Session {
    host.create_session(CreateSessionRequest {
        session_id: Some("sess".to_string()),
        workspace: WorkspaceSpec {
            mode: WorkspaceMode::New,
            root: None,
            snapshot_ref: None,
            template_ref: None,
            mount_as_workspace: true,
        },
        policy,
        ttl_ms: None,
        metadata: Map::new(),
    })
    .unwrap()
    .session
}

fn invoke(session_id: &str, tool_name: &str, args: Value) -> ToolInvocationRequest {
    ToolInvocationRequest {
        invocation_id: Some(format!("inv_{tool_name}")),
        session_id: session_id.to_string(),
        tool_name: tool_name.to_string(),
        arguments: args.as_object().cloned().unwrap(),
        cwd: Some("/workspace".to_string()),
        timeout_ms: None,
        max_output_bytes: None,
        idempotency_key: None,
        required_capabilities: vec![],
        metadata: Map::new(),
    }
}

#[test]
fn edit_replaces_single_occurrence_and_records_effect() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = session(&host, false, false);
    fs::write(
        format!("{}/file.txt", session.workspace.root),
        "Hello, World!",
    )
    .unwrap();

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Edit",
            json!({ "path": "file.txt", "oldString": "World", "newString": "Universe" }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        fs::read_to_string(format!("{}/file.txt", session.workspace.root)).unwrap(),
        "Hello, Universe!"
    );
    assert_eq!(result.effects[0].kind, "file.write");
}

#[test]
fn edit_rejects_non_unique_without_replace_all() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = session(&host, false, false);
    fs::write(format!("{}/file.txt", session.workspace.root), "foo foo").unwrap();

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Edit",
            json!({ "path": "file.txt", "oldString": "foo", "newString": "bar" }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(result.error.unwrap().contains("not unique"));
    assert_eq!(
        fs::read_to_string(format!("{}/file.txt", session.workspace.root)).unwrap(),
        "foo foo"
    );
}

#[test]
fn batch_edit_validates_before_writing_anything() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = session(&host, false, false);
    fs::write(format!("{}/a.txt", session.workspace.root), "alpha").unwrap();
    fs::write(format!("{}/b.txt", session.workspace.root), "beta").unwrap();

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "BatchEdit",
            json!({
                "edits": [
                    { "path": "a.txt", "oldString": "alpha", "newString": "ALPHA" },
                    { "path": "b.txt", "oldString": "missing", "newString": "BETA" }
                ]
            }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Error);
    assert_eq!(
        fs::read_to_string(format!("{}/a.txt", session.workspace.root)).unwrap(),
        "alpha"
    );
    assert_eq!(
        fs::read_to_string(format!("{}/b.txt", session.workspace.root)).unwrap(),
        "beta"
    );
}

#[test]
fn batch_edit_applies_multiple_files() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = session(&host, false, false);
    fs::write(format!("{}/a.txt", session.workspace.root), "alpha").unwrap();
    fs::write(format!("{}/b.txt", session.workspace.root), "beta beta").unwrap();

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "BatchEdit",
            json!({
                "edits": [
                    { "path": "a.txt", "oldString": "alpha", "newString": "ALPHA" },
                    { "path": "b.txt", "oldString": "beta", "newString": "BETA", "replaceAll": true }
                ]
            }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(result.effects.len(), 2);
    assert_eq!(
        fs::read_to_string(format!("{}/a.txt", session.workspace.root)).unwrap(),
        "ALPHA"
    );
    assert_eq!(
        fs::read_to_string(format!("{}/b.txt", session.workspace.root)).unwrap(),
        "BETA BETA"
    );
}

#[test]
fn bash_obeys_process_policy_and_records_process_effect() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = session(&host, true, false);

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Bash",
            json!({ "command": "printf hello", "timeout": 2 }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(result.output, "hello");
    assert_eq!(result.effects[0].kind, "process.exec");
}

#[test]
fn bash_denied_when_exec_disabled() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = session(&host, false, false);

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Bash",
            json!({ "command": "printf hello" }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::PolicyDenied);
}

#[test]
fn bash_enforces_allowed_command_allowlist() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let mut policy = policy(true, false);
    policy.process.allowed_commands = vec!["printf".to_string()];
    let session = session_with_policy(&host, policy);

    let allowed = host
        .execute_invocation(invoke(
            &session.id,
            "Bash",
            json!({ "command": "printf ok" }),
        ))
        .unwrap();
    let denied = host
        .execute_invocation(invoke(
            &session.id,
            "Bash",
            json!({ "command": "echo nope" }),
        ))
        .unwrap();

    assert_eq!(allowed.status, ToolResultStatus::Success);
    assert_eq!(allowed.output, "ok");
    assert_eq!(denied.status, ToolResultStatus::PolicyDenied);
    assert!(denied.effects.is_empty());
}

#[test]
fn bash_allowed_command_prefix_requires_token_boundary() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let mut policy = policy(true, false);
    policy.process.allowed_commands = vec!["printf".to_string()];
    let session = session_with_policy(&host, policy);

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Bash",
            json!({ "command": "printfx should-not-run" }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::PolicyDenied);
    assert!(result.effects.is_empty());
}

#[test]
fn bash_denies_when_process_limit_is_zero_without_side_effects() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let mut policy = policy(true, false);
    policy.process.max_processes = Some(0);
    let session = session_with_policy(&host, policy);

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Bash",
            json!({ "command": "printf bad > limit.txt" }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::PolicyDenied);
    assert!(result.effects.is_empty());
    assert!(!std::path::Path::new(&format!("{}/limit.txt", session.workspace.root)).exists());
}

#[test]
fn bash_rejects_symlink_cwd_escape_without_running() {
    #[cfg(unix)]
    {
        let temp = TempDir::new().unwrap();
        let host = HostState::new(temp.path()).unwrap();
        let session = session(&host, true, false);
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, format!("{}/link", session.workspace.root)).unwrap();
        let mut request = invoke(
            &session.id,
            "Bash",
            json!({ "command": "printf escaped > wrote.txt" }),
        );
        request.cwd = Some("/workspace/link".to_string());

        let result = host.execute_invocation(request).unwrap();

        assert_eq!(result.status, ToolResultStatus::PolicyDenied);
        assert!(result.effects.is_empty());
        assert!(!outside.join("wrote.txt").exists());
    }
}

#[test]
fn bash_timeout_stops_command_before_late_side_effect() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = session(&host, true, false);

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Bash",
            json!({ "command": "sleep 2; printf late > timed_out.txt", "timeout": 1 }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Timeout);
    assert!(result.effects.is_empty());
    assert!(!std::path::Path::new(&format!("{}/timed_out.txt", session.workspace.root)).exists());
}

#[test]
fn bash_nonzero_exit_records_process_effect_and_stderr() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = session(&host, true, false);

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Bash",
            json!({ "command": "printf err >&2; exit 7" }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Error);
    assert!(result.output.contains("[stderr]: err"));
    assert_eq!(result.metadata["returnCode"], 7);
    assert_eq!(result.effects.len(), 1);
    assert_eq!(result.effects[0].kind, "process.exec");
    assert_eq!(result.effects[0].resource.resource_type, "process");
}

#[test]
fn bash_truncates_output_to_policy_limit() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let mut policy = policy(true, false);
    policy.max_output_bytes = Some(64);
    let session = session_with_policy(&host, policy);

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Bash",
            json!({ "command": "for i in {1..200}; do printf xxxxxxxxxx; done" }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(result.output.ends_with("\n...[truncated]"));
    assert!(result.output.len() < 100);
}

#[test]
fn glob_finds_matching_files() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = session(&host, false, false);
    fs::create_dir_all(format!("{}/src", session.workspace.root)).unwrap();
    fs::write(format!("{}/src/a.rs", session.workspace.root), "").unwrap();
    fs::write(format!("{}/src/b.ts", session.workspace.root), "").unwrap();

    let result = host
        .execute_invocation(invoke(&session.id, "Glob", json!({ "pattern": "**/*.rs" })))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(result.output.contains("src/a.rs"));
    assert!(!result.output.contains("src/b.ts"));
}

#[test]
fn grep_finds_regex_matches() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = session(&host, false, false);
    fs::write(
        format!("{}/a.txt", session.workspace.root),
        "one\ntwo\nthree",
    )
    .unwrap();

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "Grep",
            json!({ "pattern": "tw.", "path": "." }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert!(result.output.contains("a.txt:2: two"));
}

#[test]
fn apply_patch_add_update_delete() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = session(&host, false, false);
    fs::write(format!("{}/old.txt", session.workspace.root), "alpha\nbeta").unwrap();
    fs::write(format!("{}/delete.txt", session.workspace.root), "bye").unwrap();
    let patch = "*** Begin Patch\n*** Add File: new.txt\n+hello\n*** Update File: old.txt\n@@\n alpha\n-beta\n+gamma\n*** Delete File: delete.txt\n*** End Patch";

    let result = host
        .execute_invocation(invoke(
            &session.id,
            "apply_patch",
            json!({ "patch": patch }),
        ))
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(
        fs::read_to_string(format!("{}/new.txt", session.workspace.root)).unwrap(),
        "hello"
    );
    assert_eq!(
        fs::read_to_string(format!("{}/old.txt", session.workspace.root)).unwrap(),
        "alpha\ngamma"
    );
    assert!(!std::path::Path::new(&format!("{}/delete.txt", session.workspace.root)).exists());
    assert_eq!(result.effects.len(), 3);
}

#[test]
fn non_substrate_tools_are_not_registered() {
    let temp = TempDir::new().unwrap();
    let host = HostState::new(temp.path()).unwrap();
    let session = session(&host, false, false);

    for tool in ["PromptUser", "WebFetch", "WebSearch", "ExpandConversation"] {
        let err = host
            .execute_invocation(invoke(
                &session.id,
                tool,
                json!({ "url": "https://example.com", "query": "example" }),
            ))
            .unwrap_err();
        assert!(err.to_string().contains("tool not found"), "{tool}: {err}");
    }
}
