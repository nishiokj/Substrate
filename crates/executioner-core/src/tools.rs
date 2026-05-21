use crate::effects::{display_path, state_ref_for_file, temp_file_path, EffectRecorder};
use crate::error::{ExecutionerError, Result};
use crate::host::empty_metadata;
use crate::protocol::{
    Session, StateRef, ToolInvocationRequest, ToolInvocationResult, ToolResultStatus,
};
use crate::workspace::{AccessKind, WorkspaceResolver};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;
use uuid::Uuid;
use wait_timeout::ChildExt;

const DEFAULT_MAX_BYTES: usize = 100_000;
const DEFAULT_GREP_MAX_RESULTS: usize = 20;
const MAX_GREP_RESULTS: usize = 50;
const DEFAULT_GLOB_MAX_RESULTS: usize = 200;
const DEFAULT_MAX_DEPTH: usize = 20;
const DEFAULT_BASH_TIMEOUT_SECS: u64 = 30;

pub fn read_file(
    session: &Session,
    request: ToolInvocationRequest,
) -> Result<ToolInvocationResult> {
    let started = Instant::now();
    let invocation_id = invocation_id(&request);
    let resolver = WorkspaceResolver::for_session(session)?;
    let mut effects = EffectRecorder::default();

    let path = match string_arg(&request.arguments, "path") {
        Ok(path) => path,
        Err(err) => {
            return Ok(error_result(
                &request,
                &invocation_id,
                err,
                started.elapsed().as_millis() as u64,
            ))
        }
    };
    let max_bytes = usize_arg(&request.arguments, "maxBytes")
        .or(request.max_output_bytes)
        .or(session.policy.max_output_bytes)
        .unwrap_or(DEFAULT_MAX_BYTES);
    let start_line = usize_arg(&request.arguments, "startLine");
    let end_line = usize_arg(&request.arguments, "endLine");

    let resolved = match resolver.resolve_read_target(request.cwd.as_deref(), &path) {
        Ok(resolved) => resolved,
        Err(err) => {
            return Ok(error_result(
                &request,
                &invocation_id,
                err,
                started.elapsed().as_millis() as u64,
            ))
        }
    };

    let metadata = match fs::metadata(&resolved.host_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(tool_error(
                &request,
                &invocation_id,
                format!("File not found: {}", resolved.logical_path),
                started.elapsed().as_millis() as u64,
                empty_metadata(),
            ))
        }
        Err(err) => {
            return Ok(io_error_result(
                &request,
                &invocation_id,
                err,
                started.elapsed().as_millis() as u64,
            ))
        }
    };

    if !metadata.is_file() {
        return Ok(tool_error(
            &request,
            &invocation_id,
            format!("Path is not a file: {}", resolved.logical_path),
            started.elapsed().as_millis() as u64,
            empty_metadata(),
        ));
    }

    let before_state = state_ref_for_file(&resolved.host_path).ok();
    effects.record_file_read(&invocation_id, &resolved.logical_path, before_state);

    let file_size = metadata.len() as usize;
    let mut content = if file_size > max_bytes {
        let mut file = fs::File::open(&resolved.host_path)?;
        let mut buffer = vec![0_u8; max_bytes];
        let bytes_read = file.read(&mut buffer)?;
        buffer.truncate(bytes_read);
        let mut text = String::from_utf8_lossy(&buffer).into_owned();
        text.push_str(&format!("\n...[truncated, file size: {file_size} bytes]"));
        text
    } else {
        let bytes = fs::read(&resolved.host_path)?;
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let mut metadata_json = Map::new();
    metadata_json.insert("path".to_string(), json!(resolved.logical_path));
    metadata_json.insert(
        "hostPath".to_string(),
        json!(display_path(&resolved.host_path)),
    );
    metadata_json.insert("size".to_string(), json!(file_size));
    metadata_json.insert("action".to_string(), json!("read"));

    if start_line.is_some() || end_line.is_some() {
        let lines: Vec<&str> = content.split('\n').collect();
        let total_lines = lines.len();
        let start = start_line.unwrap_or(1).saturating_sub(1);
        let end = end_line.unwrap_or(total_lines).min(total_lines);
        let slice = if start < end { &lines[start..end] } else { &[] };
        content = format!(
            "// Lines {}-{} of {} total\n{}",
            start + 1,
            end,
            total_lines,
            slice.join("\n")
        );
        metadata_json.insert("totalLines".to_string(), json!(total_lines));
        if let Some(start_line) = start_line {
            metadata_json.insert("startLine".to_string(), json!(start_line));
        }
        if let Some(end_line) = end_line {
            metadata_json.insert("endLine".to_string(), json!(end_line));
        }
    }

    Ok(ToolInvocationResult {
        invocation_id,
        session_id: session.id.clone(),
        tool_name: "Read".to_string(),
        status: ToolResultStatus::Success,
        output: content,
        error: None,
        summary: Some(format!("Read {}", resolved.logical_path)),
        effects: effects.into_effects(),
        duration_ms: started.elapsed().as_millis() as u64,
        metadata: metadata_json,
    })
}

pub fn write_file(
    session: &Session,
    request: ToolInvocationRequest,
) -> Result<ToolInvocationResult> {
    let started = Instant::now();
    let invocation_id = invocation_id(&request);
    let resolver = WorkspaceResolver::for_session(session)?;
    let mut effects = EffectRecorder::default();

    let path = match string_arg(&request.arguments, "path") {
        Ok(path) => path,
        Err(err) => {
            return Ok(error_result(
                &request,
                &invocation_id,
                err,
                started.elapsed().as_millis() as u64,
            ))
        }
    };
    let content = match request.arguments.get("content") {
        Some(Value::String(value)) => value.clone(),
        _ => {
            return Ok(tool_error(
                &request,
                &invocation_id,
                "content must be a string".to_string(),
                started.elapsed().as_millis() as u64,
                empty_metadata(),
            ))
        }
    };

    let resolved = match resolver.resolve_write_target(request.cwd.as_deref(), &path) {
        Ok(resolved) => resolved,
        Err(err) => {
            return Ok(error_result(
                &request,
                &invocation_id,
                err,
                started.elapsed().as_millis() as u64,
            ))
        }
    };

    if resolved.host_path.exists() {
        return Ok(tool_error(
            &request,
            &invocation_id,
            format!(
                "File already exists: {}. Use Edit to modify existing files.",
                resolved.logical_path
            ),
            started.elapsed().as_millis() as u64,
            empty_metadata(),
        ));
    }

    let parent = resolved.host_path.parent().ok_or_else(|| {
        ExecutionerError::InvalidRequest("write target has no parent".to_string())
    })?;
    fs::create_dir_all(parent)?;
    atomic_write(&resolved.host_path, content.as_bytes())?;

    let after = state_ref_for_file(&resolved.host_path).ok();
    effects.record_file_write(&invocation_id, &resolved.logical_path, None, after, true);

    let line_count = content.split('\n').count();
    let preview = content
        .split('\n')
        .take(5)
        .collect::<Vec<&str>>()
        .join("\n");
    let suffix = if line_count > 5 {
        format!("\n... ({} more lines)", line_count - 5)
    } else {
        String::new()
    };

    let output = format!(
        "Created {} ({} bytes, {} lines)\n\nPreview:\n{}{}",
        resolved.logical_path,
        content.len(),
        line_count,
        preview,
        suffix
    );

    let mut metadata_json = Map::new();
    metadata_json.insert("path".to_string(), json!(resolved.logical_path));
    metadata_json.insert(
        "hostPath".to_string(),
        json!(display_path(&resolved.host_path)),
    );
    metadata_json.insert("bytesWritten".to_string(), json!(content.len()));
    metadata_json.insert("action".to_string(), json!("write"));
    metadata_json.insert("atomic".to_string(), json!(true));

    Ok(ToolInvocationResult {
        invocation_id,
        session_id: session.id.clone(),
        tool_name: "Write".to_string(),
        status: ToolResultStatus::Success,
        output,
        error: None,
        summary: Some(format!("Created {}", resolved.logical_path)),
        effects: effects.into_effects(),
        duration_ms: started.elapsed().as_millis() as u64,
        metadata: metadata_json,
    })
}

pub fn edit_file(
    session: &Session,
    request: ToolInvocationRequest,
) -> Result<ToolInvocationResult> {
    let started = Instant::now();
    let invocation_id = invocation_id(&request);
    let resolver = WorkspaceResolver::for_session(session)?;
    let mut effects = EffectRecorder::default();

    let path = match string_arg(&request.arguments, "path") {
        Ok(path) => path,
        Err(err) => {
            return Ok(error_result(
                &request,
                &invocation_id,
                err,
                elapsed_ms(started),
            ))
        }
    };
    let old_string = match string_arg_allow_empty(&request.arguments, "oldString") {
        Ok(value) if !value.is_empty() => value,
        _ => {
            return Ok(tool_error(
                &request,
                &invocation_id,
                "Must provide 'oldString' and 'newString' for edit".to_string(),
                elapsed_ms(started),
                empty_metadata(),
            ))
        }
    };
    let new_string = match string_arg_allow_empty(&request.arguments, "newString") {
        Ok(value) => value,
        Err(err) => {
            return Ok(error_result(
                &request,
                &invocation_id,
                err,
                elapsed_ms(started),
            ))
        }
    };
    let replace_all = bool_arg(&request.arguments, "replaceAll").unwrap_or(false);

    let resolved = match resolver.resolve_existing(request.cwd.as_deref(), &path, AccessKind::Read)
    {
        Ok(resolved) => resolved,
        Err(err) if matches!(err, ExecutionerError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(tool_error(
                &request,
                &invocation_id,
                format!("File not found for edit: {path}. Use Write to create new files."),
                elapsed_ms(started),
                empty_metadata(),
            ));
        }
        Err(err) => {
            return Ok(error_result(
                &request,
                &invocation_id,
                err,
                elapsed_ms(started),
            ))
        }
    };
    if let Err(err) = resolver.resolve_write_target(request.cwd.as_deref(), &path) {
        return Ok(error_result(
            &request,
            &invocation_id,
            err,
            elapsed_ms(started),
        ));
    }

    let original = match fs::read_to_string(&resolved.host_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(tool_error(
                &request,
                &invocation_id,
                format!(
                    "File not found for edit: {}. Use Write to create new files.",
                    resolved.logical_path
                ),
                elapsed_ms(started),
                empty_metadata(),
            ))
        }
        Err(err) => {
            return Ok(io_error_result(
                &request,
                &invocation_id,
                err,
                elapsed_ms(started),
            ))
        }
    };
    let count = count_occurrences(&original, &old_string);
    if count == 0 {
        return Ok(tool_error(
            &request,
            &invocation_id,
            format!(
                "oldString not found in {}. Verify the exact text including whitespace.",
                resolved.logical_path
            ),
            elapsed_ms(started),
            metadata_with_path(&resolved.logical_path, "edit"),
        ));
    }
    if count > 1 && !replace_all {
        let first_idx = original.find(&old_string).unwrap_or(0);
        let snippet_start = first_idx.saturating_sub(30);
        let snippet_end = (first_idx + old_string.len() + 30).min(original.len());
        let snippet = &original[snippet_start..snippet_end];
        return Ok(tool_error(
            &request,
            &invocation_id,
            format!(
                "oldString found {count} times - not unique. Add surrounding context to make unique, or use replaceAll=true. First occurrence near: ...{snippet}..."
            ),
            elapsed_ms(started),
            metadata_with_path(&resolved.logical_path, "edit"),
        ));
    }

    let before = state_ref_for_file(&resolved.host_path).ok();
    let new_content = if replace_all {
        original.replace(&old_string, &new_string)
    } else {
        original.replacen(&old_string, &new_string, 1)
    };
    atomic_write(&resolved.host_path, new_content.as_bytes())?;
    let after = state_ref_for_file(&resolved.host_path).ok();
    effects.record_file_write(&invocation_id, &resolved.logical_path, before, after, false);

    let replacements = if replace_all { count } else { 1 };
    let mut metadata_json = metadata_with_path(&resolved.logical_path, "edit");
    metadata_json.insert("bytesWritten".to_string(), json!(new_content.len()));
    metadata_json.insert("replacements".to_string(), json!(replacements));
    metadata_json.insert("atomic".to_string(), json!(true));

    Ok(ToolInvocationResult {
        invocation_id,
        session_id: session.id.clone(),
        tool_name: "Edit".to_string(),
        status: ToolResultStatus::Success,
        output: format!(
            "Edited {}\nReplaced {} occurrence(s)",
            resolved.logical_path, replacements
        ),
        error: None,
        summary: Some(format!("Edited {}", resolved.logical_path)),
        effects: effects.into_effects(),
        duration_ms: elapsed_ms(started),
        metadata: metadata_json,
    })
}

pub fn batch_edit_file(
    session: &Session,
    request: ToolInvocationRequest,
) -> Result<ToolInvocationResult> {
    let started = Instant::now();
    let invocation_id = invocation_id(&request);
    let resolver = WorkspaceResolver::for_session(session)?;
    let mut effects = EffectRecorder::default();
    let edits = match request.arguments.get("edits") {
        Some(Value::Array(edits)) if !edits.is_empty() => edits.clone(),
        _ => {
            return Ok(tool_error(
                &request,
                &invocation_id,
                "Must provide non-empty edits array".to_string(),
                elapsed_ms(started),
                empty_metadata(),
            ))
        }
    };

    #[derive(Clone)]
    struct PlannedEdit {
        index: usize,
        path: String,
        logical_path: String,
        host_path: PathBuf,
        old_string: String,
        new_string: String,
        replace_all: bool,
    }

    let mut planned = Vec::<PlannedEdit>::new();
    let mut contents = std::collections::BTreeMap::<PathBuf, String>::new();
    let mut validation_errors = Vec::<Value>::new();

    for (index, edit) in edits.iter().enumerate() {
        let Some(edit_obj) = edit.as_object() else {
            validation_errors.push(
                json!({ "index": index, "path": "<missing>", "error": "Edit must be an object" }),
            );
            continue;
        };
        let path = match edit_obj.get("path").and_then(Value::as_str) {
            Some(path) if !path.is_empty() => path.to_string(),
            _ => {
                validation_errors.push(json!({ "index": index, "path": "<missing>", "error": "Missing required fields (path, oldString, newString)" }));
                continue;
            }
        };
        let old_string = match edit_obj.get("oldString").and_then(Value::as_str) {
            Some(value) if !value.is_empty() => value.to_string(),
            _ => {
                validation_errors.push(json!({ "index": index, "path": path, "error": "Missing required fields (path, oldString, newString)" }));
                continue;
            }
        };
        let new_string = match edit_obj.get("newString").and_then(Value::as_str) {
            Some(value) => value.to_string(),
            _ => {
                validation_errors.push(json!({ "index": index, "path": path, "error": "Missing required fields (path, oldString, newString)" }));
                continue;
            }
        };
        let replace_all = edit_obj
            .get("replaceAll")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let resolved =
            match resolver.resolve_existing(request.cwd.as_deref(), &path, AccessKind::Read) {
                Ok(resolved) => resolved,
                Err(_) => {
                    validation_errors
                        .push(json!({ "index": index, "path": path, "error": "File not found" }));
                    continue;
                }
            };
        if let Err(err) = resolver.resolve_write_target(request.cwd.as_deref(), &path) {
            validation_errors
                .push(json!({ "index": index, "path": path, "error": err.to_string() }));
            continue;
        }
        if !contents.contains_key(&resolved.host_path) {
            match fs::read_to_string(&resolved.host_path) {
                Ok(content) => {
                    contents.insert(resolved.host_path.clone(), content);
                }
                Err(err) => {
                    validation_errors.push(json!({ "index": index, "path": path, "error": format!("Read error: {err}") }));
                    continue;
                }
            }
        }
        let content = contents
            .get(&resolved.host_path)
            .cloned()
            .unwrap_or_default();
        let count = count_occurrences(&content, &old_string);
        if count == 0 {
            validation_errors
                .push(json!({ "index": index, "path": path, "error": "oldString not found" }));
        } else if count > 1 && !replace_all {
            validation_errors.push(json!({ "index": index, "path": path, "error": format!("oldString found {count} times - not unique") }));
        }
        planned.push(PlannedEdit {
            index,
            path,
            logical_path: resolved.logical_path,
            host_path: resolved.host_path,
            old_string,
            new_string,
            replace_all,
        });
    }

    if !validation_errors.is_empty() {
        let mut metadata = empty_metadata();
        metadata.insert("success".to_string(), json!(false));
        metadata.insert("details".to_string(), Value::Array(validation_errors));
        return Ok(tool_error(
            &request,
            &invocation_id,
            "Validation failed".to_string(),
            elapsed_ms(started),
            metadata,
        ));
    }

    planned.sort_by_key(|edit| edit.index);
    let mut output_results = Vec::<Value>::new();
    let mut modified_files =
        std::collections::BTreeMap::<PathBuf, (String, String, StateRef)>::new();
    let mut total_replacements = 0_usize;

    for edit in &planned {
        let content = contents
            .get_mut(&edit.host_path)
            .expect("validated content");
        let replacements = if edit.replace_all {
            let count = count_occurrences(content, &edit.old_string);
            *content = content.replace(&edit.old_string, &edit.new_string);
            count
        } else {
            *content = content.replacen(&edit.old_string, &edit.new_string, 1);
            1
        };
        total_replacements += replacements;
        output_results.push(json!({ "path": edit.path, "replacements": replacements }));
        modified_files
            .entry(edit.host_path.clone())
            .or_insert_with(|| {
                let before = state_ref_for_file(&edit.host_path).unwrap_or(StateRef {
                    hash: None,
                    bytes: None,
                    content_ref: None,
                    snapshot_ref: None,
                    metadata: Map::new(),
                });
                (edit.logical_path.clone(), edit.path.clone(), before)
            });
    }

    for (host_path, (logical_path, _display_path, before)) in modified_files {
        let content = contents.get(&host_path).cloned().unwrap_or_default();
        atomic_write(&host_path, content.as_bytes())?;
        let after = state_ref_for_file(&host_path).ok();
        effects.record_file_write(&invocation_id, &logical_path, Some(before), after, false);
    }

    let mut metadata = empty_metadata();
    metadata.insert("success".to_string(), json!(true));
    metadata.insert("filesModified".to_string(), json!(contents.len()));
    metadata.insert("totalReplacements".to_string(), json!(total_replacements));
    metadata.insert("edits".to_string(), Value::Array(output_results.clone()));

    Ok(ToolInvocationResult {
        invocation_id,
        session_id: session.id.clone(),
        tool_name: "BatchEdit".to_string(),
        status: ToolResultStatus::Success,
        output: format!(
            "BatchEdit complete: {} edits to {} file(s)\nTotal replacements: {}",
            edits.len(),
            contents.len(),
            total_replacements
        ),
        error: None,
        summary: Some(format!("Batch edited {} file(s)", contents.len())),
        effects: effects.into_effects(),
        duration_ms: elapsed_ms(started),
        metadata,
    })
}

pub fn bash(session: &Session, request: ToolInvocationRequest) -> Result<ToolInvocationResult> {
    let started = Instant::now();
    let invocation_id = invocation_id(&request);
    let mut effects = EffectRecorder::default();
    let command = match string_arg(&request.arguments, "command") {
        Ok(command) => command,
        Err(err) => {
            return Ok(error_result(
                &request,
                &invocation_id,
                err,
                elapsed_ms(started),
            ))
        }
    };
    if !session.policy.process.allow_exec {
        return Ok(policy_denied_result(
            &request,
            &invocation_id,
            "process execution is disabled by session policy".to_string(),
            elapsed_ms(started),
        ));
    }
    if session
        .policy
        .process
        .denied_commands
        .iter()
        .any(|denied| command.contains(denied))
    {
        return Ok(policy_denied_result(
            &request,
            &invocation_id,
            "command denied by session policy".to_string(),
            elapsed_ms(started),
        ));
    }
    if !session.policy.process.allowed_commands.is_empty()
        && !session
            .policy
            .process
            .allowed_commands
            .iter()
            .any(|allowed| command_matches_policy_entry(&command, allowed))
    {
        return Ok(policy_denied_result(
            &request,
            &invocation_id,
            "command is not allowed by session policy".to_string(),
            elapsed_ms(started),
        ));
    }
    if session.policy.process.max_processes == Some(0) {
        return Ok(policy_denied_result(
            &request,
            &invocation_id,
            "process limit exceeded by session policy".to_string(),
            elapsed_ms(started),
        ));
    }

    let resolver = WorkspaceResolver::for_session(session)?;
    let cwd = match resolver.resolve_cwd(request.cwd.as_deref()) {
        Ok(cwd) => cwd,
        Err(err) => {
            return Ok(error_result(
                &request,
                &invocation_id,
                err,
                elapsed_ms(started),
            ))
        }
    };
    let timeout_secs = u64_arg(&request.arguments, "timeout").unwrap_or(DEFAULT_BASH_TIMEOUT_SECS);
    let mut child = Command::new("bash")
        .arg("-lc")
        .arg(&command)
        .current_dir(&cwd.host_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let status = match child.wait_timeout(std::time::Duration::from_secs(timeout_secs))? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(ToolInvocationResult {
                invocation_id,
                session_id: session.id.clone(),
                tool_name: "Bash".to_string(),
                status: ToolResultStatus::Timeout,
                output: String::new(),
                error: Some(format!("Bash timed out after {timeout_secs}s")),
                summary: None,
                effects: effects.into_effects(),
                duration_ms: elapsed_ms(started),
                metadata: empty_metadata(),
            });
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    let exit_code = status.code();
    effects.record_process_exec(&invocation_id, &command, exit_code);
    let mut output = stdout;
    if !stderr.is_empty() {
        output.push_str("\n[stderr]: ");
        output.push_str(&stderr);
    }
    output = truncate_string(
        output,
        session.policy.max_output_bytes.unwrap_or(DEFAULT_MAX_BYTES),
    );
    let mut metadata = empty_metadata();
    metadata.insert("returnCode".to_string(), json!(exit_code));
    metadata.insert("cwd".to_string(), json!(cwd.logical_path));

    Ok(ToolInvocationResult {
        invocation_id,
        session_id: session.id.clone(),
        tool_name: "Bash".to_string(),
        status: if status.success() {
            ToolResultStatus::Success
        } else {
            ToolResultStatus::Error
        },
        output,
        error: if status.success() {
            None
        } else {
            Some(format!("Command exited with code {:?}", exit_code))
        },
        summary: Some(format!("Executed command in {}", cwd.logical_path)),
        effects: effects.into_effects(),
        duration_ms: elapsed_ms(started),
        metadata,
    })
}

pub fn glob_files(
    session: &Session,
    request: ToolInvocationRequest,
) -> Result<ToolInvocationResult> {
    let started = Instant::now();
    let invocation_id = invocation_id(&request);
    let resolver = WorkspaceResolver::for_session(session)?;
    let pattern = match string_arg(&request.arguments, "pattern") {
        Ok(pattern) => pattern,
        Err(err) => {
            return Ok(error_result(
                &request,
                &invocation_id,
                err,
                elapsed_ms(started),
            ))
        }
    };
    let max_results =
        usize_arg(&request.arguments, "maxResults").unwrap_or(DEFAULT_GLOB_MAX_RESULTS);
    let max_depth = usize_arg(&request.arguments, "maxDepth").unwrap_or(DEFAULT_MAX_DEPTH);
    let include_hidden = bool_arg(&request.arguments, "includeHidden").unwrap_or(false);
    let cwd = match resolver.resolve_cwd(request.cwd.as_deref()) {
        Ok(cwd) => cwd,
        Err(err) => {
            return Ok(error_result(
                &request,
                &invocation_id,
                err,
                elapsed_ms(started),
            ))
        }
    };
    let mut matches = Vec::<String>::new();
    let pattern_regex = glob_to_regex(&pattern);
    collect_paths(
        &cwd.host_path,
        &cwd.host_path,
        max_depth,
        include_hidden,
        &mut |relative, _path, is_dir| {
            if pattern_regex.is_match(relative) {
                matches.push(if is_dir {
                    format!("{relative}/")
                } else {
                    relative.to_string()
                });
            }
        },
    );
    matches.sort();
    matches.dedup();
    let total_matches = matches.len();
    let truncated = total_matches > max_results;
    matches.truncate(max_results);
    let output = if matches.is_empty() {
        format!("No files found matching pattern: {pattern} (try ../pattern or ../../pattern for sibling directories)")
    } else {
        let mut output = matches.join("\n");
        if truncated {
            output.push_str(&format!(
                "\n...[truncated at {max_results} results, {total_matches} total]"
            ));
        }
        output
    };
    let mut metadata = empty_metadata();
    metadata.insert("pattern".to_string(), json!(pattern));
    metadata.insert("matchCount".to_string(), json!(matches.len()));
    metadata.insert("totalMatches".to_string(), json!(total_matches));
    metadata.insert("truncated".to_string(), json!(truncated));
    Ok(success_tool_result(
        session,
        &request,
        invocation_id,
        "Glob",
        output,
        elapsed_ms(started),
        metadata,
        vec![],
    ))
}

pub fn grep_files(
    session: &Session,
    request: ToolInvocationRequest,
) -> Result<ToolInvocationResult> {
    let started = Instant::now();
    let invocation_id = invocation_id(&request);
    let resolver = WorkspaceResolver::for_session(session)?;
    let pattern = match string_arg(&request.arguments, "pattern") {
        Ok(pattern) => pattern,
        Err(err) => {
            return Ok(error_result(
                &request,
                &invocation_id,
                err,
                elapsed_ms(started),
            ))
        }
    };
    let case_sensitive = bool_arg(&request.arguments, "caseSensitive").unwrap_or(false);
    let regex_pattern = if case_sensitive {
        pattern.clone()
    } else {
        format!("(?i){pattern}")
    };
    let regex = match Regex::new(&regex_pattern) {
        Ok(regex) => regex,
        Err(_) => {
            return Ok(tool_error(
                &request,
                &invocation_id,
                format!("Invalid regex pattern: {pattern}"),
                elapsed_ms(started),
                empty_metadata(),
            ))
        }
    };
    let max_results = usize_arg(&request.arguments, "maxResults")
        .unwrap_or(DEFAULT_GREP_MAX_RESULTS)
        .clamp(1, MAX_GREP_RESULTS);
    let search_path =
        string_arg_allow_empty(&request.arguments, "path").unwrap_or_else(|_| ".".to_string());
    let glob_filter = request
        .arguments
        .get("glob")
        .and_then(Value::as_str)
        .map(glob_to_regex);
    let type_filter = request
        .arguments
        .get("type")
        .and_then(Value::as_str)
        .map(type_extensions);
    let resolved = if search_path == "." {
        resolver.resolve_cwd(request.cwd.as_deref())
    } else {
        resolver.resolve_existing(request.cwd.as_deref(), &search_path, AccessKind::Read)
    };
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(_) => {
            return Ok(success_tool_result(
                session,
                &request,
                invocation_id,
                "Grep",
                format!(
                "Path not found: {search_path} (try ../path or ../../path for sibling directories)"
            ),
                elapsed_ms(started),
                empty_metadata(),
                vec![],
            ))
        }
    };
    let mut matches = Vec::<String>::new();
    let root = if resolved.host_path.is_file() {
        resolved
            .host_path
            .parent()
            .unwrap_or(&resolved.host_path)
            .to_path_buf()
    } else {
        resolved.host_path.clone()
    };
    let mut search_file = |relative: &str, path: &Path| {
        if matches.len() >= max_results {
            return;
        }
        if let Some(glob) = &glob_filter {
            if !glob.is_match(relative) {
                return;
            }
        }
        if let Some(extensions) = &type_filter {
            let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
            if !extensions
                .iter()
                .any(|candidate| candidate.trim_start_matches('.') == ext)
            {
                return;
            }
        }
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => return,
        };
        for (line_idx, line) in content.lines().enumerate() {
            if matches.len() >= max_results {
                break;
            }
            if regex.is_match(line) {
                matches.push(format!(
                    "{relative}:{}: {}",
                    line_idx + 1,
                    truncate_string(line.to_string(), 200)
                ));
            }
        }
    };
    if resolved.host_path.is_file() {
        let relative = resolved
            .host_path
            .strip_prefix(&root)
            .unwrap_or(&resolved.host_path)
            .to_string_lossy()
            .replace('\\', "/");
        search_file(&relative, &resolved.host_path);
    } else {
        collect_paths(
            &resolved.host_path,
            &resolved.host_path,
            DEFAULT_MAX_DEPTH,
            false,
            &mut |relative, path, is_dir| {
                if !is_dir {
                    search_file(relative, path);
                }
            },
        );
    }
    let output = if matches.is_empty() {
        format!("No matches found for pattern: {pattern}")
    } else {
        let mut output = matches.join("\n");
        if matches.len() >= max_results {
            output.push_str(&format!("\n...[truncated at {max_results} results]"));
        }
        output
    };
    let mut metadata = empty_metadata();
    metadata.insert("pattern".to_string(), json!(pattern));
    metadata.insert("matchCount".to_string(), json!(matches.len()));
    metadata.insert("truncated".to_string(), json!(matches.len() >= max_results));
    Ok(success_tool_result(
        session,
        &request,
        invocation_id,
        "Grep",
        output,
        elapsed_ms(started),
        metadata,
        vec![],
    ))
}

pub fn apply_patch_tool(
    session: &Session,
    request: ToolInvocationRequest,
) -> Result<ToolInvocationResult> {
    let started = Instant::now();
    let invocation_id = invocation_id(&request);
    let resolver = WorkspaceResolver::for_session(session)?;
    let patch = match request
        .arguments
        .get("patch")
        .or_else(|| request.arguments.get("input"))
        .and_then(Value::as_str)
    {
        Some(patch) if !patch.is_empty() => patch.to_string(),
        _ => {
            return Ok(tool_error(
                &request,
                &invocation_id,
                "patch is required".to_string(),
                elapsed_ms(started),
                empty_metadata(),
            ))
        }
    };
    let operations = match parse_patch(&patch) {
        Ok(operations) => operations,
        Err(message) => {
            return Ok(tool_error(
                &request,
                &invocation_id,
                format!("Patch parse failed: {message}"),
                elapsed_ms(started),
                empty_metadata(),
            ))
        }
    };

    enum PatchOp {
        Add {
            logical: String,
            host: PathBuf,
            content: String,
        },
        Delete {
            logical: String,
            host: PathBuf,
        },
        Update {
            logical: String,
            host: PathBuf,
            content: String,
            move_to: Option<(String, PathBuf)>,
        },
    }

    let mut planned = Vec::<PatchOp>::new();
    for op in operations {
        match op {
            ParsedPatchOperation::Add { path, content } => {
                let resolved = match resolver.resolve_write_target(request.cwd.as_deref(), &path) {
                    Ok(resolved) => resolved,
                    Err(err) => {
                        return Ok(error_result(
                            &request,
                            &invocation_id,
                            err,
                            elapsed_ms(started),
                        ))
                    }
                };
                if resolved.host_path.exists() {
                    return Ok(tool_error(
                        &request,
                        &invocation_id,
                        format!("File already exists: {}", resolved.logical_path),
                        elapsed_ms(started),
                        empty_metadata(),
                    ));
                }
                planned.push(PatchOp::Add {
                    logical: resolved.logical_path,
                    host: resolved.host_path,
                    content,
                });
            }
            ParsedPatchOperation::Delete { path } => {
                let resolved = match resolver.resolve_existing(
                    request.cwd.as_deref(),
                    &path,
                    AccessKind::Write,
                ) {
                    Ok(resolved) => resolved,
                    Err(err) => {
                        return Ok(error_result(
                            &request,
                            &invocation_id,
                            err,
                            elapsed_ms(started),
                        ))
                    }
                };
                planned.push(PatchOp::Delete {
                    logical: resolved.logical_path,
                    host: resolved.host_path,
                });
            }
            ParsedPatchOperation::Update {
                path,
                hunks,
                move_to,
            } => {
                let resolved = match resolver.resolve_existing(
                    request.cwd.as_deref(),
                    &path,
                    AccessKind::Read,
                ) {
                    Ok(resolved) => resolved,
                    Err(err) => {
                        return Ok(error_result(
                            &request,
                            &invocation_id,
                            err,
                            elapsed_ms(started),
                        ))
                    }
                };
                if let Err(err) = resolver.resolve_write_target(request.cwd.as_deref(), &path) {
                    return Ok(error_result(
                        &request,
                        &invocation_id,
                        err,
                        elapsed_ms(started),
                    ));
                }
                let mut content = fs::read_to_string(&resolved.host_path)?;
                for hunk in hunks {
                    content = match apply_hunk(&content, &hunk) {
                        Ok(content) => content,
                        Err(message) => {
                            return Ok(tool_error(
                                &request,
                                &invocation_id,
                                format!(
                                    "Patch apply failed for {}: {message}",
                                    resolved.logical_path
                                ),
                                elapsed_ms(started),
                                empty_metadata(),
                            ))
                        }
                    };
                }
                let move_to = match move_to {
                    Some(move_path) => {
                        let moved = match resolver
                            .resolve_write_target(request.cwd.as_deref(), &move_path)
                        {
                            Ok(resolved) => resolved,
                            Err(err) => {
                                return Ok(error_result(
                                    &request,
                                    &invocation_id,
                                    err,
                                    elapsed_ms(started),
                                ))
                            }
                        };
                        Some((moved.logical_path, moved.host_path))
                    }
                    None => None,
                };
                planned.push(PatchOp::Update {
                    logical: resolved.logical_path,
                    host: resolved.host_path,
                    content,
                    move_to,
                });
            }
        }
    }

    let mut effects = EffectRecorder::default();
    let mut changed_paths = Vec::<String>::new();
    for op in planned {
        match op {
            PatchOp::Add {
                logical,
                host,
                content,
            } => {
                if let Some(parent) = host.parent() {
                    fs::create_dir_all(parent)?;
                }
                atomic_write(&host, content.as_bytes())?;
                let after = state_ref_for_file(&host).ok();
                effects.record_file_write(&invocation_id, &logical, None, after, true);
                changed_paths.push(logical);
            }
            PatchOp::Delete { logical, host } => {
                let before = state_ref_for_file(&host).ok();
                fs::remove_file(&host)?;
                effects.record_file_delete(&invocation_id, &logical, before);
                changed_paths.push(logical);
            }
            PatchOp::Update {
                logical,
                host,
                content,
                move_to,
            } => {
                let before = state_ref_for_file(&host).ok();
                match move_to {
                    Some((move_logical, move_host)) => {
                        if let Some(parent) = move_host.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        atomic_write(&move_host, content.as_bytes())?;
                        fs::remove_file(&host)?;
                        let after = state_ref_for_file(&move_host).ok();
                        effects.record_file_write(
                            &invocation_id,
                            &move_logical,
                            before,
                            after,
                            false,
                        );
                        changed_paths.push(logical);
                        changed_paths.push(move_logical);
                    }
                    None => {
                        atomic_write(&host, content.as_bytes())?;
                        let after = state_ref_for_file(&host).ok();
                        effects.record_file_write(&invocation_id, &logical, before, after, false);
                        changed_paths.push(logical);
                    }
                }
            }
        }
    }
    let mut metadata = empty_metadata();
    metadata.insert("changedPaths".to_string(), json!(changed_paths));
    Ok(success_tool_result(
        session,
        &request,
        invocation_id,
        "apply_patch",
        "Patch applied successfully".to_string(),
        elapsed_ms(started),
        metadata,
        effects.into_effects(),
    ))
}

fn atomic_write(target: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_file_path(target);
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, target)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }

    result.map_err(ExecutionerError::Io)
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn count_occurrences(content: &str, search: &str) -> usize {
    if search.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut start = 0;
    while let Some(offset) = content[start..].find(search) {
        count += 1;
        start += offset + search.len();
    }
    count
}

fn metadata_with_path(path: &str, action: &str) -> Map<String, Value> {
    let mut metadata = empty_metadata();
    metadata.insert("path".to_string(), json!(path));
    metadata.insert("action".to_string(), json!(action));
    metadata
}

fn success_tool_result(
    session: &Session,
    request: &ToolInvocationRequest,
    invocation_id: String,
    tool_name: &str,
    output: String,
    duration_ms: u64,
    metadata: Map<String, Value>,
    effects: Vec<crate::protocol::Effect>,
) -> ToolInvocationResult {
    ToolInvocationResult {
        invocation_id,
        session_id: session.id.clone(),
        tool_name: tool_name.to_string(),
        status: ToolResultStatus::Success,
        output,
        error: None,
        summary: Some(format!("Executed {}", request.tool_name)),
        effects,
        duration_ms,
        metadata,
    }
}

fn policy_denied_result(
    request: &ToolInvocationRequest,
    invocation_id: &str,
    message: String,
    duration_ms: u64,
) -> ToolInvocationResult {
    ToolInvocationResult {
        invocation_id: invocation_id.to_string(),
        session_id: request.session_id.clone(),
        tool_name: request.tool_name.clone(),
        status: ToolResultStatus::PolicyDenied,
        output: String::new(),
        error: Some(message),
        summary: None,
        effects: vec![],
        duration_ms,
        metadata: empty_metadata(),
    }
}

fn truncate_string(value: String, max_len: usize) -> String {
    if value.len() <= max_len {
        value
    } else {
        format!("{}\n...[truncated]", &value[..max_len])
    }
}

fn command_matches_policy_entry(command: &str, entry: &str) -> bool {
    let entry = entry.trim();
    if entry.is_empty() {
        return false;
    }
    command == entry
        || command
            .strip_prefix(entry)
            .is_some_and(|remaining| remaining.starts_with(char::is_whitespace))
}

fn should_skip_name(name: &str, include_hidden: bool) -> bool {
    const SKIP_DIRS: &[&str] = &[
        "node_modules",
        ".git",
        "dist",
        "build",
        ".next",
        ".turbo",
        ".cache",
        "coverage",
        ".venv",
        "venv",
        "__pycache__",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        "site-packages",
        "htmlcov",
        ".tox",
        ".eggs",
        "logs",
        "log",
    ];
    if !include_hidden && name.starts_with('.') {
        return true;
    }
    SKIP_DIRS.contains(&name) || name.ends_with(".log") || name.ends_with(".pyc")
}

fn collect_paths<F>(
    root: &Path,
    current: &Path,
    depth: usize,
    include_hidden: bool,
    visitor: &mut F,
) where
    F: FnMut(&str, &Path, bool),
{
    if depth == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if should_skip_name(&name, include_hidden) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        visitor(&relative, &path, file_type.is_dir());
        if file_type.is_dir() {
            collect_paths(root, &path, depth - 1, include_hidden, visitor);
        }
    }
}

fn glob_to_regex(pattern: &str) -> Regex {
    let mut regex = String::from("^");
    let chars = pattern.chars().collect::<Vec<_>>();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                regex.push_str(".*");
                i += 2;
            }
            '*' => {
                regex.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                regex.push_str("[^/]");
                i += 1;
            }
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '[' | ']' | '{' | '}' | '\\' => {
                regex.push('\\');
                regex.push(chars[i]);
                i += 1;
            }
            ch => {
                regex.push(ch);
                i += 1;
            }
        }
    }
    regex.push('$');
    Regex::new(&regex).unwrap_or_else(|_| Regex::new("$^").expect("valid fallback regex"))
}

fn type_extensions(file_type: &str) -> Vec<String> {
    match file_type.to_lowercase().as_str() {
        "ts" => vec!["ts", "tsx"],
        "js" => vec!["js", "jsx", "mjs", "cjs"],
        "py" => vec!["py", "pyi"],
        "rust" | "rs" => vec!["rs"],
        "go" => vec!["go"],
        "java" => vec!["java"],
        "json" => vec!["json"],
        "yaml" => vec!["yaml", "yml"],
        "md" => vec!["md", "markdown"],
        "sh" => vec!["sh", "bash", "zsh"],
        other => vec![other],
    }
    .into_iter()
    .map(str::to_string)
    .collect()
}

#[derive(Debug)]
enum ParsedPatchOperation {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Vec<PatchLine>>,
    },
}

#[derive(Debug, Clone)]
enum PatchLine {
    Context(String),
    Add(String),
    Remove(String),
}

fn parse_patch(input: &str) -> std::result::Result<Vec<ParsedPatchOperation>, String> {
    let lines = input.lines().collect::<Vec<_>>();
    if lines.first() != Some(&"*** Begin Patch") {
        return Err("missing *** Begin Patch".to_string());
    }
    if !lines.iter().any(|line| *line == "*** End Patch") {
        return Err("missing *** End Patch".to_string());
    }
    let mut ops = Vec::new();
    let mut i = 1;
    while i < lines.len() {
        let line = lines[i];
        if line == "*** End Patch" {
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            i += 1;
            let mut content = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let Some(add_line) = lines[i].strip_prefix('+') else {
                    return Err(format!("expected + line in add file: {}", lines[i]));
                };
                content.push(add_line.to_string());
                i += 1;
            }
            ops.push(ParsedPatchOperation::Add {
                path: path.trim().to_string(),
                content: content.join("\n"),
            });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(ParsedPatchOperation::Delete {
                path: path.trim().to_string(),
            });
            i += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            i += 1;
            let mut move_to = None;
            let mut hunks = Vec::new();
            while i < lines.len() && !lines[i].starts_with("*** ") {
                if let Some(target) = lines[i].strip_prefix("*** Move to: ") {
                    move_to = Some(target.trim().to_string());
                    i += 1;
                    continue;
                }
                if lines[i].starts_with("@@") {
                    i += 1;
                    let mut hunk = Vec::new();
                    while i < lines.len()
                        && !lines[i].starts_with("@@")
                        && !lines[i].starts_with("*** ")
                    {
                        let hunk_line = lines[i];
                        if let Some(content) = hunk_line.strip_prefix('+') {
                            hunk.push(PatchLine::Add(content.to_string()));
                        } else if let Some(content) = hunk_line.strip_prefix('-') {
                            hunk.push(PatchLine::Remove(content.to_string()));
                        } else if let Some(content) = hunk_line.strip_prefix(' ') {
                            hunk.push(PatchLine::Context(content.to_string()));
                        } else if hunk_line.is_empty() {
                            hunk.push(PatchLine::Context(String::new()));
                        } else {
                            return Err(format!("unexpected hunk line: {hunk_line}"));
                        }
                        i += 1;
                    }
                    hunks.push(hunk);
                    continue;
                }
                return Err(format!("unexpected update line: {}", lines[i]));
            }
            ops.push(ParsedPatchOperation::Update {
                path: path.trim().to_string(),
                move_to,
                hunks,
            });
            continue;
        }
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        return Err(format!("unexpected patch line: {line}"));
    }
    if ops.is_empty() {
        return Err("no patch operations found".to_string());
    }
    Ok(ops)
}

fn apply_hunk(content: &str, hunk: &[PatchLine]) -> std::result::Result<String, String> {
    let old_lines = hunk
        .iter()
        .filter_map(|line| match line {
            PatchLine::Context(content) | PatchLine::Remove(content) => Some(content.clone()),
            PatchLine::Add(_) => None,
        })
        .collect::<Vec<_>>();
    let new_lines = hunk
        .iter()
        .filter_map(|line| match line {
            PatchLine::Context(content) | PatchLine::Add(content) => Some(content.clone()),
            PatchLine::Remove(_) => None,
        })
        .collect::<Vec<_>>();
    let old_block = old_lines.join("\n");
    let new_block = new_lines.join("\n");
    if old_block.is_empty() {
        return Err("empty hunk context is not supported".to_string());
    }
    if !content.contains(&old_block) {
        return Err("hunk context not found".to_string());
    }
    Ok(content.replacen(&old_block, &new_block, 1))
}

fn invocation_id(request: &ToolInvocationRequest) -> String {
    request
        .invocation_id
        .clone()
        .unwrap_or_else(|| format!("inv_{}", Uuid::new_v4().simple()))
}

fn string_arg(args: &Map<String, Value>, name: &str) -> Result<String> {
    match args.get(name) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(ExecutionerError::InvalidRequest(format!(
            "{name} is required"
        ))),
    }
}

fn string_arg_allow_empty(args: &Map<String, Value>, name: &str) -> Result<String> {
    match args.get(name) {
        Some(Value::String(value)) => Ok(value.clone()),
        _ => Err(ExecutionerError::InvalidRequest(format!(
            "{name} is required"
        ))),
    }
}

fn bool_arg(args: &Map<String, Value>, name: &str) -> Option<bool> {
    args.get(name).and_then(Value::as_bool)
}

fn u64_arg(args: &Map<String, Value>, name: &str) -> Option<u64> {
    args.get(name).and_then(Value::as_u64)
}

fn usize_arg(args: &Map<String, Value>, name: &str) -> Option<usize> {
    args.get(name)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn error_result(
    request: &ToolInvocationRequest,
    invocation_id: &str,
    err: ExecutionerError,
    duration_ms: u64,
) -> ToolInvocationResult {
    let status = if matches!(err, ExecutionerError::PolicyDenied(_)) {
        ToolResultStatus::PolicyDenied
    } else {
        ToolResultStatus::Error
    };
    ToolInvocationResult {
        invocation_id: invocation_id.to_string(),
        session_id: request.session_id.clone(),
        tool_name: request.tool_name.clone(),
        status,
        output: String::new(),
        error: Some(err.to_string()),
        summary: None,
        effects: vec![],
        duration_ms,
        metadata: empty_metadata(),
    }
}

fn io_error_result(
    request: &ToolInvocationRequest,
    invocation_id: &str,
    err: std::io::Error,
    duration_ms: u64,
) -> ToolInvocationResult {
    tool_error(
        request,
        invocation_id,
        format!("File read failed: {err}"),
        duration_ms,
        empty_metadata(),
    )
}

fn tool_error(
    request: &ToolInvocationRequest,
    invocation_id: &str,
    message: String,
    duration_ms: u64,
    metadata: Map<String, Value>,
) -> ToolInvocationResult {
    ToolInvocationResult {
        invocation_id: invocation_id.to_string(),
        session_id: request.session_id.clone(),
        tool_name: request.tool_name.clone(),
        status: ToolResultStatus::Error,
        output: String::new(),
        error: Some(message),
        summary: None,
        effects: vec![],
        duration_ms,
        metadata,
    }
}
