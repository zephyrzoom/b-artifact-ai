/**
 * 服务端 REST 客户端（§7 传输协议）。
 *
 * 三条硬规则来自 §7.1：
 *   1. 分支判断只看 `code`，不看 HTTP 状态、不看 message；
 *   2. 只有"可安全重试"的操作才重试（GET/HEAD、PUT blob、分块 PUT、POST /commit）；
 *   3. 429 / 503 按 `Retry-After` 退避，5xx 指数退避最多 3 次。
 *
 * `fetch` 可注入——单测用桩替换即可，不必起真实服务端。
 */

import { ApiError, type ApiErrorCode } from './errors.js';

export interface ApiClientOptions {
  /** 形如 `http://127.0.0.1:8080`；`/api/v1` 由本模块拼。 */
  baseUrl: string;
  token?: string;
  fetchImpl?: typeof fetch;
  timeoutMs?: number;
  /** 5xx / 429 的最大重试次数。 */
  maxRetries?: number;
}

export interface LoginResult {
  token: string;
  expires_at: string;
  user: UserInfo;
}

export interface UserInfo {
  id: number;
  username: string;
  display_name?: string | null;
  is_admin: boolean;
  source?: string;
}

export interface RepoSummary {
  id: number;
  name: string;
  description: string;
  owner: string;
  head_rev: number;
  created_at: string;
  my_role: string;
  my_permissions: { read: boolean; write: boolean; admin: boolean };
}

export interface RepoInfo extends RepoSummary {
  stats?: Record<string, unknown>;
}

export interface TreeEntry {
  path: string;
  kind: 'file' | 'dir';
  blob_hash: string | null;
  size: number;
  mode: number;
  mtime: number;
  changed_rev: number;
}

export interface TreeResult {
  repo: string;
  rev: number;
  prefix: string;
  depth: number;
  items: TreeEntry[];
  total: number;
}

export interface LogEntry {
  rev: number;
  author: string;
  message: string;
  created_at: string;
  file_count: number;
  byte_delta: number;
  manifest_hash: string;
}

export interface ChangeRow {
  rev: number;
  path: string;
  op: 'add' | 'modify' | 'delete';
  kind: 'file' | 'dir';
  blob_hash: string | null;
  size: number;
  mode: number;
  mtime: number;
}

export interface ChangeSpec {
  path: string;
  op: 'add' | 'modify' | 'delete';
  kind: 'file' | 'dir';
  blob_hash?: string | null;
  size?: number;
  mode?: number;
  mtime?: number;
}

export interface PrepareResult {
  commit_id: string;
  /** 幂等重放时为 true，且带 rev、无 token。 */
  replayed?: boolean;
  rev?: number;
  commit_token?: string;
  need_blobs?: string[];
  expires_in?: number;
}

export interface CommitResult {
  commit_id: string;
  rev: number;
  replayed: boolean;
}

export interface UploadSession {
  upload_id: string;
  hash: string;
  size: number;
  chunk_size: number;
  received_chunks: number[];
}

export interface LockInfo {
  id: number;
  path: string;
  /** v0.4.17：服务端仍回这个字段，但**恒为 `'file'`**（§5.2 只剩文件锁）。 */
  kind: 'file' | 'dir';
  owner_id: number;
  owner: string;
  comment: string | null;
  created_at: string;
  expires_at: string | null;
  token?: string;
}

export interface Progress {
  loaded: number;
  total: number;
}

const DEFAULT_TIMEOUT_MS = 30_000;

/** 路径段做 percent-encoding（§7.1：URL 段编码，路径本身保持 posix）。 */
function enc(s: string): string {
  return s
    .split('/')
    .map((seg) => encodeURIComponent(seg))
    .join('/');
}

/** 解析 `Retry-After`（秒数或 HTTP 日期）。 */
function parseRetryAfter(v: string | null): number | null {
  if (!v) return null;
  const secs = Number(v);
  if (Number.isFinite(secs)) return Math.max(0, secs * 1000);
  const when = Date.parse(v);
  if (Number.isNaN(when)) return null;
  return Math.max(0, when - Date.now());
}

export class ApiClient {
  readonly baseUrl: string;
  private token: string | null;
  private readonly f: typeof fetch;
  private readonly timeoutMs: number;
  private readonly maxRetries: number;

  constructor(opts: ApiClientOptions) {
    this.baseUrl = opts.baseUrl.replace(/\/+$/, '');
    this.token = opts.token ?? null;
    this.f = opts.fetchImpl ?? fetch;
    this.timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.maxRetries = opts.maxRetries ?? 3;
  }

  setToken(t: string | null): void {
    this.token = t;
  }

  get tokenValue(): string | null {
    return this.token;
  }

  /** 暴露注入的 fetch，便于按同一实现重建 client（例如按 meta 切换服务端地址）。 */
  get fetchImpl(): typeof fetch {
    return this.f;
  }

  private url(path: string, query?: Record<string, string | number | undefined>): string {
    let u = `${this.baseUrl}/api/v1${path}`;
    if (query) {
      const qs = new URLSearchParams();
      for (const [k, v] of Object.entries(query)) {
        if (v !== undefined && v !== '') qs.set(k, String(v));
      }
      const s = qs.toString();
      if (s) u += `?${s}`;
    }
    return u;
  }

  private headers(extra: Record<string, string> = {}): Record<string, string> {
    const h: Record<string, string> = { ...extra };
    if (this.token) h['Authorization'] = `Bearer ${this.token}`;
    return h;
  }

  /** 把非 2xx 响应转成 ApiError。 */
  private static async toError(resp: Response): Promise<ApiError> {
    let code: ApiErrorCode = 'UNKNOWN';
    let message = `HTTP ${resp.status}`;
    let details: unknown = null;
    try {
      const text = await resp.text();
      if (text) {
        const j = JSON.parse(text) as { error?: { code?: string; message?: string; details?: unknown } };
        if (j.error?.code) code = j.error.code as ApiErrorCode;
        if (j.error?.message) message = j.error.message;
        details = j.error?.details ?? null;
      }
    } catch {
      /* 非 JSON 错误体：保留默认 message */
    }
    return new ApiError({
      status: resp.status,
      code,
      message,
      details,
      retryAfterMs: parseRetryAfter(resp.headers.get('retry-after')),
    });
  }

  private async request(
    method: string,
    path: string,
    opts: {
      query?: Record<string, string | number | undefined>;
      body?: unknown;
      raw?: Uint8Array;
      headers?: Record<string, string>;
      /** 该请求是否可安全重试（§7.1「重试语义」）。 */
      idempotent?: boolean;
      signal?: AbortSignal;
    } = {},
  ): Promise<Response> {
    const retryable = opts.idempotent ?? (method === 'GET' || method === 'HEAD');
    let lastErr: ApiError | null = null;

    for (let attempt = 0; attempt <= this.maxRetries; attempt++) {
      if (attempt > 0) {
        const backoff = Math.min(2000, 200 * 2 ** (attempt - 1));
        await sleep(lastErr?.retryAfterMs ?? backoff);
      }
      const ac = new AbortController();
      const timer = setTimeout(() => ac.abort(), this.timeoutMs);
      if (opts.signal) {
        if (opts.signal.aborted) ac.abort();
        else opts.signal.addEventListener('abort', () => ac.abort(), { once: true });
      }
      try {
        const resp = await this.f(this.url(path, opts.query), {
          method,
          headers: this.headers(
            opts.raw
              ? { 'Content-Type': 'application/octet-stream', ...(opts.headers ?? {}) }
              : { 'Content-Type': 'application/json; charset=utf-8', ...(opts.headers ?? {}) },
          ),
          body: opts.raw !== undefined
            ? new Uint8Array(opts.raw)
            : opts.body === undefined
              ? undefined
              : JSON.stringify(opts.body),
          signal: ac.signal,
        });
        if (resp.ok) return resp;
        const err = await ApiClient.toError(resp);
        if (!retryable || !err.retryable()) throw err;
        lastErr = err;
      } catch (e) {
        const err = e instanceof ApiError ? e : new ApiError({
          status: 0,
          code: 'NETWORK',
          message: e instanceof Error ? e.message : String(e),
        });
        if (!retryable || err.code !== 'NETWORK') throw err;
        lastErr = err;
      } finally {
        clearTimeout(timer);
      }
    }
    throw lastErr ?? new ApiError({ status: 0, code: 'UNKNOWN', message: '请求失败' });
  }

  private async json<T>(method: string, path: string, opts: Parameters<ApiClient['request']>[2] = {}): Promise<T> {
    const resp = await this.request(method, path, opts);
    const text = await resp.text();
    return (text ? JSON.parse(text) : {}) as T;
  }

  // ---------- 认证（§7.2） ----------

  async login(username: string, password: string): Promise<LoginResult> {
    const r = await this.json<LoginResult>('POST', '/auth/login', {
      body: { username, password },
      idempotent: false,
    });
    this.token = r.token;
    return r;
  }

  async me(): Promise<UserInfo> {
    return this.json<UserInfo>('GET', '/auth/me');
  }

  async logout(): Promise<void> {
    await this.request('POST', '/auth/logout', { idempotent: true });
    this.token = null;
  }

  // ---------- 仓库与浏览 ----------

  async listRepos(): Promise<RepoSummary[]> {
    const r = await this.json<{ items: RepoSummary[] }>('GET', '/repos');
    return r.items ?? [];
  }

  async repoInfo(repo: string): Promise<RepoInfo> {
    return this.json<RepoInfo>('GET', `/repos/${encodeURIComponent(repo)}/info`);
  }

  async tree(
    repo: string,
    q: { rev?: number; prefix?: string; depth?: number } = {},
  ): Promise<TreeResult> {
    return this.json<TreeResult>('GET', `/repos/${encodeURIComponent(repo)}/tree`, {
      query: { rev: q.rev ?? 0, prefix: q.prefix ?? '', depth: q.depth ?? 1 },
    });
  }

  async log(
    repo: string,
    q: { prefix?: string; from?: number; limit?: number; offset?: number } = {},
  ): Promise<{ items: LogEntry[]; total: number }> {
    return this.json<{ items: LogEntry[]; total: number }>(
      'GET',
      `/repos/${encodeURIComponent(repo)}/log`,
      { query: { prefix: q.prefix, from: q.from, limit: q.limit, offset: q.offset } },
    );
  }

  /**
   * 增量变更（JSONL 流式，§7.1：大响应不走分页信封）。
   * 返回按 (rev, path) 升序的原始行——调用方自行折叠成最终状态。
   */
  async changes(
    repo: string,
    q: { from: number; to?: number; prefix?: string },
  ): Promise<ChangeRow[]> {
    const resp = await this.request('GET', `/repos/${encodeURIComponent(repo)}/changes`, {
      query: { from: q.from, to: q.to, prefix: q.prefix },
    });
    const text = await resp.text();
    const out: ChangeRow[] = [];
    for (const line of text.split('\n')) {
      const s = line.trim();
      if (!s) continue;
      out.push(JSON.parse(s) as ChangeRow);
    }
    return out;
  }

  // ---------- 传输（§7.2） ----------

  /** 先问后传：返回服务端缺失的 hash（一次最多 1000 个）。 */
  async blobsMissing(repo: string, hashes: string[]): Promise<string[]> {
    const out: string[] = [];
    for (let i = 0; i < hashes.length; i += 1000) {
      const r = await this.json<{ missing: string[] }>(
        'POST',
        `/repos/${encodeURIComponent(repo)}/blobs/missing`,
        { body: { hashes: hashes.slice(i, i + 1000) }, idempotent: true },
      );
      out.push(...(r.missing ?? []));
    }
    return out;
  }

  /** 整块上传（< 64 MB，幂等）。 */
  async putBlob(repo: string, hash: string, data: Uint8Array): Promise<void> {
    await this.request('PUT', `/repos/${encodeURIComponent(repo)}/blobs/${hash}`, {
      raw: data,
      idempotent: true,
    });
  }

  async createUpload(
    repo: string,
    req: { hash: string; size: number; chunk_size: number },
  ): Promise<UploadSession> {
    return this.json<UploadSession>(
      'POST',
      `/repos/${encodeURIComponent(repo)}/blobs/uploads`,
      { body: req, idempotent: false },
    );
  }

  async putChunk(repo: string, uploadId: string, n: number, data: Uint8Array): Promise<void> {
    await this.request(
      'PUT',
      `/repos/${encodeURIComponent(repo)}/blobs/uploads/${uploadId}/${n}`,
      { raw: data, idempotent: true },
    );
  }

  async uploadStatus(repo: string, uploadId: string): Promise<UploadSession> {
    return this.json<UploadSession>(
      'GET',
      `/repos/${encodeURIComponent(repo)}/blobs/uploads/${uploadId}`,
    );
  }

  async completeUpload(repo: string, uploadId: string): Promise<void> {
    await this.request(
      'POST',
      `/repos/${encodeURIComponent(repo)}/blobs/uploads/${uploadId}/complete`,
      { body: {}, idempotent: false },
    );
  }

  async abortUpload(repo: string, uploadId: string): Promise<void> {
    await this.request(
      'DELETE',
      `/repos/${encodeURIComponent(repo)}/blobs/uploads/${uploadId}`,
      { idempotent: true },
    );
  }

  /** 下载 blob 原始字节。 */
  async downloadBlob(repo: string, hash: string): Promise<Uint8Array> {
    const resp = await this.request('GET', `/repos/${encodeURIComponent(repo)}/blobs/${hash}`);
    const buf = await resp.arrayBuffer();
    return new Uint8Array(buf);
  }

  /** 下载到流的入口——大文件走这里，避免整块驻留内存。 */
  async openBlobStream(repo: string, hash: string): Promise<Response> {
    return this.request('GET', `/repos/${encodeURIComponent(repo)}/blobs/${hash}`);
  }

  // ---------- 提交（§5.3 / §7.3） ----------

  async prepareCommit(
    repo: string,
    req: { commit_id: string; base_rev: number; message?: string; changes: ChangeSpec[] },
  ): Promise<PrepareResult> {
    return this.json<PrepareResult>(
      'POST',
      `/repos/${encodeURIComponent(repo)}/commit/prepare`,
      { body: req, idempotent: true },
    );
  }

  async commit(
    repo: string,
    req: { commit_id: string; commit_token: string; message?: string },
  ): Promise<CommitResult> {
    return this.json<CommitResult>('POST', `/repos/${encodeURIComponent(repo)}/commit`, {
      body: req,
      idempotent: true,
    });
  }

  // ---------- 锁（§7.2） ----------

  async listLocks(
    repo: string,
    q: { path?: string; owner?: 'me' | string } = {},
  ): Promise<LockInfo[]> {
    const r = await this.json<{ items: LockInfo[] }>(
      'GET',
      `/repos/${encodeURIComponent(repo)}/locks`,
      { query: { path: q.path, owner: q.owner } },
    );
    return r.items ?? [];
  }

  async acquireLock(
    repo: string,
    // v0.4.17：锁只剩文件级，服务端不再接受 kind（§5.2）
    req: { path: string; comment?: string; expires_in?: number },
  ): Promise<LockInfo> {
    return this.json<LockInfo>('POST', `/repos/${encodeURIComponent(repo)}/locks`, {
      body: req,
      idempotent: false,
    });
  }

  async refreshLocks(repo: string, ttlSecs?: number): Promise<{ refreshed: number; expires_at: string | null }> {
    return this.json<{ refreshed: number; expires_at: string | null }>(
      'POST',
      `/repos/${encodeURIComponent(repo)}/locks/refresh`,
      { body: { ttl_secs: ttlSecs }, idempotent: true },
    );
  }

  async releaseLock(
    repo: string,
    path: string,
    opts: { token?: string; breakLock?: boolean; reason?: string } = {},
  ): Promise<Record<string, unknown>> {
    return this.json<Record<string, unknown>>(
      'DELETE',
      `/repos/${encodeURIComponent(repo)}/locks/${enc(path)}`,
      {
        query: {
          token: opts.token,
          break: opts.breakLock ? 'true' : undefined,
          reason: opts.reason,
        },
        idempotent: true,
      },
    );
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
