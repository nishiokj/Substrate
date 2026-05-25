# substrate

Python SDK for the Substrate agent execution environment.

Install:

```sh
pip install substrate
```

The package is pure Python. It does not compile Rust during install. For local
managed execution, the SDK discovers a prebuilt `executioner` runtime from a
bundled `executioner_sdk/bin/executioner`, an installed `substrate-runtime`
package, or `executioner` on `PATH`. Remote-host usage does not need a local
runtime.

The public API exposes a small environment facade:

```py
from substrate import Environment

with Environment.create(workspace="new", allow_commands=["ls"]) as env:
    env.write("hello.txt", "hello")
    print(env.read("hello.txt"))

    env.edit({
        "path": "hello.txt",
        "oldString": "hello",
        "newString": "hello from Substrate",
    })

    print(env.bash("ls /workspace"))
    files = env.list()
    artifact = env.export_workspace()
    env.materialize_workspace_artifact(artifact, "/tmp/restored-workspace")
```

For an agent loop, pass Substrate's schemas into the model request, then execute
matching tool-use blocks directly:

```py
from anthropic import Anthropic
from substrate import Environment

client = Anthropic()
messages = [{"role": "user", "content": "Create notes.txt and read it back."}]

with Environment.create(workspace="new", allow_commands=["python", "pytest"]) as env:
    response = client.messages.create(
        model="...",
        max_tokens=1024,
        tools=env.tool_schemas(),
        messages=messages,
    )

    for block in response.content:
        if block.type == "tool_use":
            result = env.execute({
                "id": block.id,
                "name": block.name,
                "input": block.input,
            })
            messages.append({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": block.id,
                    "content": result.output,
                }],
            })
```

The package hides the file-backed queue and worker transport behind the facade.
`ExecutionerEnvironment.create(...)` remains available for advanced host,
worker, and backend configuration.
