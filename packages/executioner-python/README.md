# executioner-sdk

Python bindings for Executioner.

The public API exposes an environment object:

```py
from executioner_sdk import ExecutionerEnvironment

with ExecutionerEnvironment.create(
    binaryPath="/path/to/executioner",
    workspace={"kind": "existing", "root": "."},
) as env:
    result = env.submit({
        "toolName": "Write",
        "arguments": {"path": "hello.txt", "content": "hello"},
    })

    edit = env.edit({
        "path": "hello.txt",
        "oldString": "hello",
        "newString": "hello from Executioner",
    })
```

The package hides the file-backed queue and worker transport behind config.
Agent apps should not write broker files directly.
