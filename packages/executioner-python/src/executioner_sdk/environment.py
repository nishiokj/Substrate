from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import tempfile
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, Mapping, TypedDict
from urllib import error as urlerror
from urllib import request as urlrequest

WorkspaceKind = Literal["new", "existing"]
WorkerKind = Literal["managed", "external"]
HostKind = Literal["managed", "http"]
BackendKind = Literal["file"]
ToolStatus = Literal["success", "error", "timeout", "cancelled", "policy_denied"]
EffectOperation = Literal["read", "create", "update", "delete", "execute"]


class WorkspaceConfig(TypedDict, total=False):
    kind: WorkspaceKind
    root: str


class WorkerConfig(TypedDict, total=False):
    kind: WorkerKind
    id: str
    idleSleepMs: int


class HostConfig(TypedDict, total=False):
    kind: HostKind
    stateDir: str
    host: str
    port: int
    baseUrl: str


class BackendConfig(TypedDict, total=False):
    kind: BackendKind
    queueDir: str


class LifecycleConfig(TypedDict, total=False):
    destroyOnClose: bool
    cleanupQueueOnClose: bool
    cleanupStateOnClose: bool


class ProcessPolicyConfig(TypedDict, total=False):
    allowExec: bool
    allowedCommands: list[str]
    deniedCommands: list[str]


class NetworkPolicyConfig(TypedDict, total=False):
    enabled: bool
    allowHosts: list[str]
    denyHosts: list[str]


class PolicyConfig(TypedDict, total=False):
    readRoots: list[str]
    writeRoots: list[str]
    process: ProcessPolicyConfig
    network: NetworkPolicyConfig
    maxDurationMs: int
    maxOutputBytes: int


class ToolCall(TypedDict, total=False):
    toolName: str
    arguments: dict[str, Any]
    cwd: str
    invocationId: str
    timeoutMs: int
    maxOutputBytes: int
    metadata: dict[str, Any]


class ToolSubmitOptions(TypedDict, total=False):
    cwd: str
    invocationId: str
    timeoutMs: int
    maxOutputBytes: int
    metadata: dict[str, Any]


class EditToolArguments(TypedDict, total=False):
    path: str
    oldString: str
    newString: str
    replaceAll: bool


@dataclass(frozen=True)
class StateEffect:
    id: str
    invocationId: str
    kind: str
    resourceType: str
    uri: str
    operation: EffectOperation
    summary: str | None = None
    reversible: bool = False
    occurredAt: str = ""

    @classmethod
    def from_json(cls, value: Mapping[str, Any]) -> "StateEffect":
        return cls(
            id=str(value.get("id", "")),
            invocationId=str(value.get("invocationId", "")),
            kind=str(value.get("kind", "")),
            resourceType=str(value.get("resourceType", "")),
            uri=str(value.get("uri", "")),
            operation=value.get("operation", "read"),
            summary=value.get("summary"),
            reversible=bool(value.get("reversible", False)),
            occurredAt=str(value.get("occurredAt", "")),
        )


@dataclass(frozen=True)
class WorkspaceInfo:
    root: str
    logicalRoot: str
    mode: str
    fresh: bool
    managed: bool

    @classmethod
    def from_json(cls, value: Mapping[str, Any]) -> "WorkspaceInfo":
        return cls(
            root=str(value.get("root", "")),
            logicalRoot=str(value.get("logicalRoot", "")),
            mode=str(value.get("mode", "")),
            fresh=bool(value.get("fresh", False)),
            managed=bool(value.get("managed", False)),
        )


@dataclass(frozen=True)
class SessionInfo:
    id: str
    state: str
    workspace: WorkspaceInfo
    createdAt: str
    expiresAt: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_json(cls, value: Mapping[str, Any]) -> "SessionInfo":
        return cls(
            id=str(value.get("id", "")),
            state=str(value.get("state", "")),
            workspace=WorkspaceInfo.from_json(value.get("workspace", {})),
            createdAt=str(value.get("createdAt", "")),
            expiresAt=value.get("expiresAt"),
            metadata=dict(value.get("metadata", {})),
        )


@dataclass(frozen=True)
class SubmitResult:
    invocationId: str
    toolName: str
    status: ToolStatus
    output: str
    error: str | None
    summary: str | None
    effects: list[StateEffect]
    durationMs: int
    metadata: dict[str, Any]

    @classmethod
    def from_json(cls, value: Mapping[str, Any]) -> "SubmitResult":
        return cls(
            invocationId=str(value.get("invocationId", "")),
            toolName=str(value.get("toolName", "")),
            status=value.get("status", "error"),
            output=str(value.get("output", "")),
            error=value.get("error"),
            summary=value.get("summary"),
            effects=[StateEffect.from_json(effect) for effect in value.get("effects", [])],
            durationMs=int(value.get("durationMs", 0)),
            metadata=dict(value.get("metadata", {})),
        )


@dataclass
class _ManagedProcess:
    process: subprocess.Popen[bytes]
    name: str


@dataclass
class _RuntimeConfig:
    binaryPath: str
    queueDir: str
    baseUrl: str
    host: dict[str, Any]
    worker: dict[str, Any]
    workspace: WorkspaceConfig
    policy: dict[str, Any]
    lifecycle: dict[str, bool]
    submitTimeoutMs: int


class ExecutionerEnvironment:
    def __init__(
        self,
        config: _RuntimeConfig,
        session: SessionInfo,
        processes: list[_ManagedProcess],
    ) -> None:
        self._config = config
        self._session = session
        self._processes = processes

    @classmethod
    def create(
        cls,
        *,
        binaryPath: str | None = None,
        backend: BackendConfig | None = None,
        host: HostConfig | None = None,
        worker: WorkerConfig | None = None,
        workspace: WorkspaceConfig | None = None,
        policy: PolicyConfig | None = None,
        lifecycle: LifecycleConfig | None = None,
        submitTimeoutMs: int | None = None,
    ) -> "ExecutionerEnvironment":
        runtime = _materialize_config(
            binary_path=binaryPath,
            backend=backend,
            host=host,
            worker=worker,
            workspace=workspace,
            policy=policy,
            lifecycle=lifecycle,
            submit_timeout_ms=submitTimeoutMs,
        )
        processes: list[_ManagedProcess] = []

        if runtime.host["kind"] == "managed":
            processes.append(
                _spawn_process(
                    runtime.binaryPath,
                    [
                        "host",
                        "--addr",
                        f"{runtime.host['host']}:{runtime.host['port']}",
                        "--state-dir",
                        runtime.host["stateDir"],
                    ],
                    "executioner-host",
                )
            )
            _wait_for_health(runtime.baseUrl, runtime.submitTimeoutMs)

        _ensure_file_queue(runtime.queueDir)
        session = _create_session(runtime)

        if runtime.worker["kind"] == "managed":
            processes.append(
                _spawn_process(
                    runtime.binaryPath,
                    [
                        "worker",
                        "run",
                        "--id",
                        runtime.worker["id"],
                        "--host-url",
                        runtime.baseUrl,
                        "--queue-dir",
                        runtime.queueDir,
                        "--idle-sleep-ms",
                        str(runtime.worker["idleSleepMs"]),
                    ],
                    "executioner-worker",
                )
            )

        return cls(runtime, session, processes)

    @property
    def session(self) -> SessionInfo:
        return self._session

    def submit(self, call: ToolCall) -> SubmitResult:
        arguments = call.get("arguments")
        if not isinstance(arguments, dict):
            raise TypeError("tool call arguments must be a JSON object")
        tool_name = call.get("toolName")
        if not isinstance(tool_name, str) or not tool_name:
            raise TypeError("toolName must be a non-empty string")

        invocation_id = call.get("invocationId") or f"inv_{uuid.uuid4().hex}"
        request = {
            "invocationId": invocation_id,
            "sessionId": self._session.id,
            "toolName": tool_name,
            "arguments": arguments,
            "cwd": call.get("cwd", "/workspace"),
            "timeoutMs": call.get("timeoutMs"),
            "maxOutputBytes": call.get("maxOutputBytes"),
            "metadata": call.get("metadata", {}),
        }
        _write_json_atomic(Path(self._config.queueDir) / "pending" / f"{invocation_id}.json", request)
        return _wait_for_result(self._config.queueDir, invocation_id, self._config.submitTimeoutMs)

    def edit(self, args: EditToolArguments, options: ToolSubmitOptions | None = None) -> SubmitResult:
        call: ToolCall = {
            **(options or {}),
            "toolName": "Edit",
            "arguments": dict(args),
        }
        return self.submit(call)

    def close(self) -> SessionInfo:
        if self._config.lifecycle["destroyOnClose"]:
            session_data = _delete_json(f"{self._config.baseUrl}sessions/{self._session.id}")
        else:
            session_data = _post_json(f"{self._config.baseUrl}sessions/{self._session.id}/close", None)
        session = SessionInfo.from_json(session_data)

        for managed in reversed(self._processes):
            _terminate_process(managed)

        if self._config.lifecycle["cleanupQueueOnClose"]:
            shutil.rmtree(self._config.queueDir, ignore_errors=True)
        if self._config.lifecycle["cleanupStateOnClose"] and self._config.host["kind"] == "managed":
            shutil.rmtree(self._config.host["stateDir"], ignore_errors=True)

        return session

    def __enter__(self) -> "ExecutionerEnvironment":
        return self

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        self.close()


def _materialize_config(
    *,
    binary_path: str | None,
    backend: BackendConfig | None,
    host: HostConfig | None,
    worker: WorkerConfig | None,
    workspace: WorkspaceConfig | None,
    policy: PolicyConfig | None,
    lifecycle: LifecycleConfig | None,
    submit_timeout_ms: int | None,
) -> _RuntimeConfig:
    backend = backend or {"kind": "file"}
    host = host or {"kind": "managed"}
    worker = worker or {"kind": "managed"}
    lifecycle = lifecycle or {}

    queue_dir = backend.get("queueDir") or tempfile.mkdtemp(prefix="executioner-queue-")
    resolved_lifecycle = {
        "destroyOnClose": lifecycle.get("destroyOnClose", True),
        "cleanupQueueOnClose": lifecycle.get("cleanupQueueOnClose", backend.get("queueDir") is None),
        "cleanupStateOnClose": lifecycle.get("cleanupStateOnClose", host.get("kind") != "http"),
    }

    if host.get("kind") == "http":
        base_url = _normalize_base_url(str(host["baseUrl"]))
        resolved_host = {"kind": "http", "baseUrl": base_url}
    else:
        host_name = str(host.get("host", "127.0.0.1"))
        port = int(host.get("port") or _free_port())
        state_dir = str(host.get("stateDir") or tempfile.mkdtemp(prefix="executioner-state-"))
        base_url = f"http://{host_name}:{port}/"
        resolved_host = {
            "kind": "managed",
            "stateDir": state_dir,
            "host": host_name,
            "port": port,
        }

    if worker.get("kind") == "external":
        resolved_worker = {"kind": "external"}
    else:
        resolved_worker = {
            "kind": "managed",
            "id": worker.get("id", "executioner-python-worker"),
            "idleSleepMs": int(worker.get("idleSleepMs", 10)),
        }

    return _RuntimeConfig(
        binaryPath=_resolve_binary_path(binary_path),
        queueDir=queue_dir,
        baseUrl=base_url,
        host=resolved_host,
        worker=resolved_worker,
        workspace=workspace or {"kind": "new"},
        policy=_materialize_policy(policy),
        lifecycle=resolved_lifecycle,
        submitTimeoutMs=submit_timeout_ms or 30_000,
    )


def _materialize_policy(policy: PolicyConfig | None) -> dict[str, Any]:
    policy = policy or {}
    process = policy.get("process", {})
    network = policy.get("network", {})
    return {
        "readRoots": policy.get("readRoots", ["/workspace"]),
        "writeRoots": policy.get("writeRoots", ["/workspace"]),
        "process": {
            "allowExec": process.get("allowExec", False),
            "allowedCommands": process.get("allowedCommands", []),
            "deniedCommands": process.get("deniedCommands", []),
        },
        "network": {
            "enabled": network.get("enabled", False),
            "allowHosts": network.get("allowHosts", []),
            "denyHosts": network.get("denyHosts", []),
        },
        "maxDurationMs": policy.get("maxDurationMs", 300_000),
        "maxOutputBytes": policy.get("maxOutputBytes", 100_000),
    }


def _create_session(config: _RuntimeConfig) -> SessionInfo:
    workspace = (
        {
            "mode": "existing",
            "root": config.workspace["root"],
            "mountAsWorkspace": True,
        }
        if config.workspace.get("kind") == "existing"
        else {
            "mode": "new",
            "mountAsWorkspace": True,
        }
    )
    response = _post_json(
        f"{config.baseUrl}sessions",
        {
            "workspace": workspace,
            "policy": config.policy,
            "metadata": {},
        },
    )
    return SessionInfo.from_json(response["session"])


def _wait_for_result(queue_dir: str, invocation_id: str, timeout_ms: int) -> SubmitResult:
    started = time.monotonic()
    completed_path = Path(queue_dir) / "completed" / f"{invocation_id}.json"
    failed_path = Path(queue_dir) / "failed" / f"{invocation_id}.json"
    timeout_s = timeout_ms / 1000

    while time.monotonic() - started < timeout_s:
        if completed_path.exists():
            return SubmitResult.from_json(_read_json(completed_path)["result"])
        if failed_path.exists():
            failed = _read_json(failed_path)
            message = failed.get("error", {}).get("message", "unknown error")
            raise RuntimeError(f"Executioner invocation failed: {message}")
        time.sleep(0.01)

    raise TimeoutError(f"Timed out waiting for Executioner invocation {invocation_id}")


def _ensure_file_queue(queue_dir: str) -> None:
    for child in ["pending", "claimed", "completed", "failed"]:
        Path(queue_dir, child).mkdir(parents=True, exist_ok=True)


def _resolve_binary_path(binary_path: str | None) -> str:
    if binary_path:
        return binary_path
    env_binary = os.environ.get("EXECUTIONER_BIN")
    if env_binary:
        return env_binary

    package_binary = Path(__file__).resolve().parents[4] / "target" / "release" / "executioner"
    return str(package_binary) if package_binary.exists() else "executioner"


def _spawn_process(binary_path: str, args: list[str], name: str) -> _ManagedProcess:
    process = subprocess.Popen(
        [binary_path, *args],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return _ManagedProcess(process=process, name=name)


def _close_process_pipes(process: subprocess.Popen[bytes]) -> None:
    for pipe in [process.stdout, process.stderr]:
        if pipe is not None and not pipe.closed:
            pipe.close()


def _terminate_process(managed: _ManagedProcess) -> None:
    if managed.process.poll() is not None:
        _close_process_pipes(managed.process)
        return
    managed.process.terminate()
    try:
        managed.process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        managed.process.kill()
        managed.process.wait(timeout=2)
    _close_process_pipes(managed.process)


def _wait_for_health(base_url: str, timeout_ms: int) -> None:
    started = time.monotonic()
    timeout_s = timeout_ms / 1000
    while time.monotonic() - started < timeout_s:
        try:
            with urlrequest.urlopen(f"{base_url}health", timeout=1) as response:
                if 200 <= response.status < 300:
                    return
        except (OSError, urlerror.URLError):
            pass
        time.sleep(0.025)
    raise TimeoutError(f"Timed out waiting for Executioner host at {base_url}")


def _post_json(url: str, body: Any) -> dict[str, Any]:
    data = json.dumps(body).encode("utf-8") if body is not None else b"null"
    request = urlrequest.Request(
        url,
        data=data,
        method="POST",
        headers={"content-type": "application/json"},
    )
    return _request_json(request)


def _delete_json(url: str) -> dict[str, Any]:
    return _request_json(urlrequest.Request(url, method="DELETE"))


def _request_json(request: urlrequest.Request) -> dict[str, Any]:
    try:
        with urlrequest.urlopen(request) as response:
            return json.loads(response.read().decode("utf-8"))
    except urlerror.HTTPError as error:
        body = error.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"Executioner host returned {error.code}: {body}") from error


def _read_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def _write_json_atomic(path: Path, value: Mapping[str, Any]) -> None:
    tmp_path = path.with_name(f"{path.name}.tmp.{uuid.uuid4().hex}")
    tmp_path.write_text(json.dumps(value, indent=2), encoding="utf-8")
    tmp_path.replace(path)


def _normalize_base_url(url: str) -> str:
    return url if url.endswith("/") else f"{url}/"


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])
