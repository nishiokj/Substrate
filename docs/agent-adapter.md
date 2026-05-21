# Agent Adapter Boundary

Executioner should not be shaped around a single agent implementation. Agent
apps integrate by implementing a small tool execution client or by starting a
worker that bridges their broker to an Executioner host.

## SDK Boundary

Agent applications should bind to an environment object and submit work to it.
The broker is an implementation detail selected by config:

```rust
let env = ExecutionerEnvironment::create(
    ExecutionerEnvironment::builder()
        .file_backend("/tmp/executioner/queue")
        .in_process_host("/tmp/executioner/state")
        .in_process_worker("agent-worker")
        .new_workspace()
        .build()?
).await?;

let result = env.submit(
    ToolCall::json("Write", json!({
        "path": "notes.txt",
        "content": "hello"
    }))?
).await?;
```

The SDK intentionally exposes SDK-owned types such as `ToolCall`,
`SubmitResult`, `SessionInfo`, and `StateEffect`. It should not re-export raw
protocol structs as its primary interface. Protocol structs remain owned by
`executioner-core` and are used at transport, persistence, and schema
boundaries.

## Worker Modes

The environment supports three worker shapes:

```text
in_process_worker  -> submit drives one broker claim inline
managed_worker     -> SDK starts a background pull worker task
external_worker    -> SDK only enqueues and waits; another worker runtime pulls
```

The real worker path is `managed_worker` or `external_worker`. Both use the same
internal backend and host traits. The public API names a backend and host
transport by config; it does not hand agent applications a `FileBroker`,
`HttpHostClient`, or queue directory API.

An external worker is started from SDK config too:

```rust
let worker = ExecutionerWorker::start(
    ExecutionerWorker::builder()
        .file_backend("/tmp/executioner/queue")
        .http_host("http://127.0.0.1:8765/")
        .id("agent-worker")
        .build()?
)?;
```

When a Unix socket or another broker is added, it should become another
`HostConfig` or `BackendConfig` variant without changing `env.submit(...)`.

## Generic Runtime Boundary

The Rust protocol structs in `executioner-core::protocol` are the wire and
storage contract. Language SDKs can be generated later from the same schema,
but the execution layer is not TypeScript-owned.

An agent runtime can then choose between local and remote execution:

```text
Agent loop -> host client -> Executioner host
Agent loop -> event broker -> Executioner worker -> Executioner host
```

The worker can be spawned by the CLI:

```sh
executioner worker run --host-url http://host:8765 --queue-dir /path/to/queue
```

It can also be embedded by constructing `executioner_worker::Worker` with any
implementation of `InvocationBroker` and `ToolHostClient`.

## Changes Required In An Existing Agent

The integration should be deliberately small:

1. Create or attach an Executioner session when an agent run starts.
2. Replace direct in-process tool execution with an `ExecutionerEnvironment`.
3. Map the agent cwd to `/workspace` logical paths.
4. Feed only semantic output and summaries into the model context.
5. Store effects separately for trace, audit, cache invalidation, and UI state.

## What The Agent Should Not Own

The agent should not:

- Trust path strings without substrate validation.
- Infer writes from tool names.
- Decide host filesystem authority.
- Replay or rollback state from model context.
- Treat successful tool status as proof that no unintended mutation happened.

## Context Window Policy

The model usually gets:

```text
Updated reports/model.xlsx with a Q4 Forecast sheet and recalculated totals.
```

The system stores:

```json
{
  "effects": [
    {
      "kind": "file.write",
      "resource": {
        "type": "file",
        "uri": "file:///workspace/reports/model.xlsx"
      },
      "operation": "update",
      "reversible": true
    }
  ]
}
```

The context window should be a consumer of summarized state, not the ledger
itself.
