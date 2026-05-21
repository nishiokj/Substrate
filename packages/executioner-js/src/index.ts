import { spawn, type ChildProcess } from 'node:child_process';
import { createServer } from 'node:net';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { tmpdir } from 'node:os';
import { randomUUID } from 'node:crypto';
import { fileURLToPath } from 'node:url';

export type WorkspaceConfig =
  | { kind: 'new' }
  | { kind: 'existing'; root: string };

export type WorkerConfig =
  | { kind: 'managed'; id?: string; idleSleepMs?: number }
  | { kind: 'external' };

export type HostConfig =
  | { kind: 'managed'; stateDir?: string; host?: string; port?: number }
  | { kind: 'http'; baseUrl: string };

export type BackendConfig = {
  kind: 'file';
  queueDir?: string;
};

export type LifecycleConfig = {
  destroyOnClose?: boolean;
  cleanupQueueOnClose?: boolean;
  cleanupStateOnClose?: boolean;
};

export type ProcessPolicyConfig = {
  allowExec?: boolean;
  allowedCommands?: string[];
  deniedCommands?: string[];
};

export type NetworkPolicyConfig = {
  enabled?: boolean;
  allowHosts?: string[];
  denyHosts?: string[];
};

export type PolicyConfig = {
  readRoots?: string[];
  writeRoots?: string[];
  process?: ProcessPolicyConfig;
  network?: NetworkPolicyConfig;
  maxDurationMs?: number;
  maxOutputBytes?: number;
};

export type EnvironmentConfig = {
  binaryPath?: string;
  backend?: BackendConfig;
  host?: HostConfig;
  worker?: WorkerConfig;
  workspace?: WorkspaceConfig;
  policy?: PolicyConfig;
  lifecycle?: LifecycleConfig;
  submitTimeoutMs?: number;
};

export type ToolCall = {
  toolName: string;
  arguments: Record<string, unknown>;
  cwd?: string;
  invocationId?: string;
  timeoutMs?: number;
  maxOutputBytes?: number;
  metadata?: Record<string, unknown>;
};

export type ToolSubmitOptions = Omit<ToolCall, 'toolName' | 'arguments'>;

export type EditToolArguments = {
  path: string;
  oldString: string;
  newString: string;
  replaceAll?: boolean;
};

export type StateEffect = {
  id: string;
  invocationId: string;
  kind: string;
  resourceType: string;
  uri: string;
  operation: 'read' | 'create' | 'update' | 'delete' | 'execute';
  summary?: string;
  reversible: boolean;
  occurredAt: string;
};

export type SubmitResult = {
  invocationId: string;
  toolName: string;
  status: 'success' | 'error' | 'timeout' | 'cancelled' | 'policy_denied';
  output: string;
  error?: string | null;
  summary?: string | null;
  effects: StateEffect[];
  durationMs: number;
  metadata: Record<string, unknown>;
};

export type SessionInfo = {
  id: string;
  state: string;
  workspace: {
    root: string;
    logicalRoot: string;
    mode: string;
    fresh: boolean;
    managed: boolean;
  };
  createdAt: string;
  expiresAt?: string | null;
  metadata: Record<string, unknown>;
};

type CreateSessionResponse = {
  session: SessionInfo;
};

type CompletedEnvelope = {
  result: SubmitResult;
};

type FailedEnvelope = {
  error: {
    code: string;
    message: string;
    retryable: boolean;
  };
};

type ManagedProcess = {
  process: ChildProcess;
  name: string;
};

export class ExecutionerEnvironment {
  private constructor(
    private readonly config: RequiredRuntimeConfig,
    private readonly sessionInfo: SessionInfo,
    private readonly processes: ManagedProcess[],
  ) {}

  static async create(config: EnvironmentConfig = {}): Promise<ExecutionerEnvironment> {
    const runtime = await materializeConfig(config);
    const processes: ManagedProcess[] = [];

    if (runtime.host.kind === 'managed') {
      const hostProcess = spawnProcess(runtime.binaryPath, [
        'host',
        '--addr',
        `${runtime.host.host}:${runtime.host.port}`,
        '--state-dir',
        runtime.host.stateDir,
      ], 'executioner-host');
      processes.push(hostProcess);
      await waitForHealth(runtime.baseUrl, runtime.submitTimeoutMs);
    }

    await ensureFileQueue(runtime.queueDir);

    const session = await createSession(runtime);

    if (runtime.worker.kind === 'managed') {
      const workerProcess = spawnProcess(runtime.binaryPath, [
        'worker',
        'run',
        '--id',
        runtime.worker.id,
        '--host-url',
        runtime.baseUrl,
        '--queue-dir',
        runtime.queueDir,
        '--idle-sleep-ms',
        String(runtime.worker.idleSleepMs),
      ], 'executioner-worker');
      processes.push(workerProcess);
    }

    return new ExecutionerEnvironment(runtime, session, processes);
  }

  get session(): SessionInfo {
    return this.sessionInfo;
  }

  async submit(call: ToolCall): Promise<SubmitResult> {
    assertObject(call.arguments, 'tool call arguments');
    const invocationId = call.invocationId ?? `inv_${randomUUID().replaceAll('-', '')}`;
    const request = {
      invocationId,
      sessionId: this.sessionInfo.id,
      toolName: call.toolName,
      arguments: call.arguments,
      cwd: call.cwd ?? '/workspace',
      timeoutMs: call.timeoutMs,
      maxOutputBytes: call.maxOutputBytes,
      metadata: call.metadata ?? {},
    };

    await writeJsonAtomic(
      join(this.config.queueDir, 'pending', `${invocationId}.json`),
      request,
    );

    return waitForResult(this.config.queueDir, invocationId, this.config.submitTimeoutMs);
  }

  async edit(args: EditToolArguments, options: ToolSubmitOptions = {}): Promise<SubmitResult> {
    return this.submit({
      ...options,
      toolName: 'Edit',
      arguments: { ...args },
    });
  }

  async close(): Promise<SessionInfo> {
    const session = this.config.lifecycle.destroyOnClose
      ? await deleteJson<SessionInfo>(`${this.config.baseUrl}sessions/${this.sessionInfo.id}`)
      : await postJson<SessionInfo>(`${this.config.baseUrl}sessions/${this.sessionInfo.id}/close`, null);

    for (const managed of [...this.processes].reverse()) {
      terminateProcess(managed);
    }

    if (this.config.lifecycle.cleanupQueueOnClose) {
      await rm(this.config.queueDir, { recursive: true, force: true });
    }
    if (this.config.lifecycle.cleanupStateOnClose && this.config.host.kind === 'managed') {
      await rm(this.config.host.stateDir, { recursive: true, force: true });
    }

    return session;
  }
}

type RequiredRuntimeConfig = {
  binaryPath: string;
  queueDir: string;
  baseUrl: string;
  host: { kind: 'managed'; stateDir: string; host: string; port: number } | { kind: 'http'; baseUrl: string };
  worker: { kind: 'managed'; id: string; idleSleepMs: number } | { kind: 'external' };
  workspace: WorkspaceConfig;
  policy: RequiredPolicyConfig;
  lifecycle: Required<LifecycleConfig>;
  submitTimeoutMs: number;
};

type RequiredPolicyConfig = {
  readRoots: string[];
  writeRoots: string[];
  process: Required<ProcessPolicyConfig>;
  network: Required<NetworkPolicyConfig>;
  maxDurationMs: number;
  maxOutputBytes: number;
};

async function materializeConfig(config: EnvironmentConfig): Promise<RequiredRuntimeConfig> {
  const binaryPath = resolveBinaryPath(config.binaryPath);
  const queueDir = config.backend?.queueDir ?? await mkdtemp(join(tmpdir(), 'executioner-queue-'));
  const submitTimeoutMs = config.submitTimeoutMs ?? 30_000;
  const lifecycle = {
    destroyOnClose: config.lifecycle?.destroyOnClose ?? true,
    cleanupQueueOnClose: config.lifecycle?.cleanupQueueOnClose ?? config.backend?.queueDir === undefined,
    cleanupStateOnClose: config.lifecycle?.cleanupStateOnClose ?? config.host?.kind !== 'http',
  };

  const hostConfig = config.host ?? { kind: 'managed' as const };
  const host = hostConfig.kind === 'http'
    ? hostConfig
    : {
        kind: 'managed' as const,
        stateDir: hostConfig.stateDir ?? await mkdtemp(join(tmpdir(), 'executioner-state-')),
        host: hostConfig.host ?? '127.0.0.1',
        port: hostConfig.port ?? await freePort(),
      };
  const baseUrl = host.kind === 'http'
    ? normalizeBaseUrl(host.baseUrl)
    : `http://${host.host}:${host.port}/`;

  const workerConfig = config.worker ?? { kind: 'managed' as const };
  const worker = workerConfig.kind === 'external'
    ? workerConfig
    : {
        kind: 'managed' as const,
        id: workerConfig.id ?? 'executioner-js-worker',
        idleSleepMs: workerConfig.idleSleepMs ?? 10,
      };

  return {
    binaryPath,
    queueDir,
    baseUrl,
    host,
    worker,
    workspace: config.workspace ?? { kind: 'new' },
    policy: materializePolicy(config.policy),
    lifecycle,
    submitTimeoutMs,
  };
}

function materializePolicy(policy?: PolicyConfig): RequiredPolicyConfig {
  return {
    readRoots: policy?.readRoots ?? ['/workspace'],
    writeRoots: policy?.writeRoots ?? ['/workspace'],
    process: {
      allowExec: policy?.process?.allowExec ?? false,
      allowedCommands: policy?.process?.allowedCommands ?? [],
      deniedCommands: policy?.process?.deniedCommands ?? [],
    },
    network: {
      enabled: policy?.network?.enabled ?? false,
      allowHosts: policy?.network?.allowHosts ?? [],
      denyHosts: policy?.network?.denyHosts ?? [],
    },
    maxDurationMs: policy?.maxDurationMs ?? 300_000,
    maxOutputBytes: policy?.maxOutputBytes ?? 100_000,
  };
}

async function createSession(config: RequiredRuntimeConfig): Promise<SessionInfo> {
  const workspace = config.workspace.kind === 'existing'
    ? {
        mode: 'existing',
        root: config.workspace.root,
        mountAsWorkspace: true,
      }
    : {
        mode: 'new',
        mountAsWorkspace: true,
      };

  const response = await postJson<CreateSessionResponse>(`${config.baseUrl}sessions`, {
    workspace,
    policy: config.policy,
    metadata: {},
  });
  return response.session;
}

async function waitForResult(queueDir: string, invocationId: string, timeoutMs: number): Promise<SubmitResult> {
  const started = Date.now();
  const completedPath = join(queueDir, 'completed', `${invocationId}.json`);
  const failedPath = join(queueDir, 'failed', `${invocationId}.json`);

  while (Date.now() - started < timeoutMs) {
    if (existsSync(completedPath)) {
      const completed = await readJson<CompletedEnvelope>(completedPath);
      return completed.result;
    }
    if (existsSync(failedPath)) {
      const failed = await readJson<FailedEnvelope>(failedPath);
      throw new Error(`Executioner invocation failed: ${failed.error.message}`);
    }
    await sleep(10);
  }

  throw new Error(`Timed out waiting for Executioner invocation ${invocationId}`);
}

async function ensureFileQueue(queueDir: string): Promise<void> {
  await Promise.all([
    mkdir(join(queueDir, 'pending'), { recursive: true }),
    mkdir(join(queueDir, 'claimed'), { recursive: true }),
    mkdir(join(queueDir, 'completed'), { recursive: true }),
    mkdir(join(queueDir, 'failed'), { recursive: true }),
  ]);
}

function resolveBinaryPath(binaryPath?: string): string {
  if (binaryPath) {
    return binaryPath;
  }
  if (process.env.EXECUTIONER_BIN) {
    return process.env.EXECUTIONER_BIN;
  }

  const packageBinary = join(
    dirname(fileURLToPath(import.meta.url)),
    '../../../target/release/executioner',
  );
  return existsSync(packageBinary) ? packageBinary : 'executioner';
}

function spawnProcess(binaryPath: string, args: string[], name: string): ManagedProcess {
  const child = spawn(binaryPath, args, {
    stdio: ['ignore', 'pipe', 'pipe'],
    env: process.env,
  });
  child.stderr.on('data', (chunk) => {
    process.stderr.write(`[${name}] ${chunk.toString()}`);
  });
  child.on('error', (error) => {
    process.stderr.write(`[${name}] ${error.message}\n`);
  });
  return { process: child, name };
}

function terminateProcess(managed: ManagedProcess): void {
  if (managed.process.killed || managed.process.exitCode !== null) {
    return;
  }
  managed.process.kill('SIGTERM');
}

async function waitForHealth(baseUrl: string, timeoutMs: number): Promise<void> {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    try {
      const response = await fetch(`${baseUrl}health`);
      if (response.ok) {
        return;
      }
    } catch {
      // Host is still starting.
    }
    await sleep(25);
  }
  throw new Error(`Timed out waiting for Executioner host at ${baseUrl}`);
}

async function postJson<T>(url: string, body: unknown): Promise<T> {
  const response = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    throw new Error(`Executioner host returned ${response.status}: ${await response.text()}`);
  }
  return response.json() as Promise<T>;
}

async function deleteJson<T>(url: string): Promise<T> {
  const response = await fetch(url, { method: 'DELETE' });
  if (!response.ok) {
    throw new Error(`Executioner host returned ${response.status}: ${await response.text()}`);
  }
  return response.json() as Promise<T>;
}

async function readJson<T>(path: string): Promise<T> {
  return JSON.parse(await readFile(path, 'utf8')) as T;
}

async function writeJsonAtomic(path: string, value: unknown): Promise<void> {
  const tmpPath = `${path}.tmp.${randomUUID().replaceAll('-', '')}`;
  await writeFile(tmpPath, JSON.stringify(value, null, 2));
  await fsRename(tmpPath, path);
}

async function fsRename(from: string, to: string): Promise<void> {
  const { rename } = await import('node:fs/promises');
  await rename(from, to);
}

function assertObject(value: unknown, label: string): asserts value is Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error(`${label} must be a JSON object`);
  }
}

function normalizeBaseUrl(url: string): string {
  return url.endsWith('/') ? url : `${url}/`;
}

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      if (!address || typeof address === 'string') {
        server.close(() => reject(new Error('Unable to allocate local port')));
        return;
      }
      const port = address.port;
      server.close(() => resolve(port));
    });
  });
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
