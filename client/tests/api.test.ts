import { describe, expect, it } from 'vitest';

import {
  ApiClient,
  type ChangeRow,
  type LockInfo,
  type RepoSummary,
} from '../src/core/api.js';
import { ApiError } from '../src/core/errors.js';

interface Call {
  url: string;
  method: string;
  headers: Record<string, string>;
  bodyText?: string;
  bodyBytes?: Uint8Array;
}

/** 记录调用并按要求回包的 fetch 桩。 */
function harness(responder: (call: Call, index: number) => Response) {
  const calls: Call[] = [];
  const f = (async (input: unknown, init: { method?: string; headers?: Record<string, string>; body?: unknown } = {}) => {
    const call: Call = {
      url: String(input),
      method: init.method ?? 'GET',
      headers: (init.headers ?? {}) as Record<string, string>,
    };
    if (init.body instanceof Uint8Array) call.bodyBytes = init.body;
    else if (typeof init.body === 'string') call.bodyText = init.body;
    calls.push(call);
    return responder(call, calls.length - 1);
  }) as unknown as typeof fetch;
  return { calls, f };
}

function client(f: typeof fetch, over: { token?: string; maxRetries?: number } = {}) {
  return new ApiClient({ baseUrl: 'http://srv:8080/', fetchImpl: f, ...over });
}

function json(status: number, body: unknown, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json', ...headers },
  });
}

function errBody(code: string, message = 'boom', details: unknown = null) {
  return { error: { code, message, details } };
}

describe('ApiClient 请求构造', () => {
  it('baseUrl 尾部斜杠被去掉，统一拼 /api/v1', async () => {
    const h = harness(() => json(200, { items: [] }));
    const c = client(h.f);
    await c.listRepos();
    expect(h.calls[0]!.url).toBe('http://srv:8080/api/v1/repos');
  });

  it('带 token 时下发 Authorization，无 token 时不下发', async () => {
    const h = harness(() => json(200, { id: 1 }));
    const c = client(h.f, { token: 'T' });
    await c.me();
    expect(h.calls[0]!.headers['Authorization']).toBe('Bearer T');

    c.setToken(null);
    await c.me();
    expect(h.calls[1]!.headers['Authorization']).toBeUndefined();
  });

  it('仓库名与路径段按段编码（空格不变成 +）', async () => {
    const h = harness(() => json(200, {}));
    await client(h.f).releaseLock('my repo', 'my dir/a.psd', {
      token: 'tk',
      breakLock: true,
      reason: '强制回收',
    });
    const u = new URL(h.calls[0]!.url);
    expect(u.pathname).toBe('/api/v1/repos/my%20repo/locks/my%20dir/a.psd');
    expect(u.searchParams.get('token')).toBe('tk');
    expect(u.searchParams.get('break')).toBe('true');
    expect(u.searchParams.get('reason')).toBe('强制回收');
  });

  it('空查询值与 undefined 不进 query string', async () => {
    const h = harness(() => json(200, { items: [], total: 0 }));
    await client(h.f).listLocks('art', { path: '', owner: undefined });
    expect(h.calls[0]!.url).toBe('http://srv:8080/api/v1/repos/art/locks');
  });

  it('二进制 body 原样发出，并打上 octet-stream', async () => {
    const h = harness(() => new Response(null, { status: 201 }));
    const bytes = new Uint8Array([1, 2, 3, 255]);
    await client(h.f).putBlob('art', 'deadbeef', bytes);
    expect(h.calls[0]!.method).toBe('PUT');
    expect([...(h.calls[0]!.bodyBytes ?? [])]).toEqual([1, 2, 3, 255]);
    expect(h.calls[0]!.headers['Content-Type']).toBe('application/octet-stream');
  });
});

describe('错误码解析与重试（§7.1）', () => {
  it('分支只看 code：解析 error.code / message / details', async () => {
    const h = harness(() => json(409, errBody('NAME_COLLISION', '名字撞了', { path: 'a.psd' })));
    let caught: ApiError | null = null;
    try {
      await client(h.f).acquireLock('art', { path: 'a.psd' });
    } catch (e) {
      caught = e as ApiError;
    }
    expect(caught).toBeInstanceOf(ApiError);
    expect(caught!.code).toBe('NAME_COLLISION');
    expect(caught!.status).toBe(409);
    expect(caught!.details).toEqual({ path: 'a.psd' });
  });

  it('非 JSON 错误体退化为 HTTP 状态文案，不抛解析异常', async () => {
    const h = harness(() => new Response('upstream exploded', { status: 502 }));
    await expect(client(h.f).me()).rejects.toMatchObject({ status: 502, message: 'HTTP 502' });
  });

  it('不可重试的错误只发一次（4xx 非 429）', async () => {
    const h = harness(() => json(404, errBody('NOT_FOUND')));
    await expect(client(h.f).repoInfo('art')).rejects.toMatchObject({ code: 'NOT_FOUND' });
    expect(h.calls).toHaveLength(1);
  });

  it('POST 非幂等操作不重试（login 失败一次就放弃）', async () => {
    const h = harness(() => json(500, errBody('INTERNAL')));
    await expect(client(h.f).login('a', 'b')).rejects.toBeInstanceOf(ApiError);
    expect(h.calls).toHaveLength(1);
  });

  it('幂等 GET 遇到 500 会重试到上限后抛出', async () => {
    const h = harness(() => json(500, errBody('INTERNAL')));
    await expect(client(h.f, { maxRetries: 2 }).me()).rejects.toMatchObject({
      code: 'INTERNAL',
    });
    expect(h.calls).toHaveLength(3); // 首次 + 2 次重试
  });

  it('重试后成功则返回最后一次结果', async () => {
    const h = harness((_c, i) => (i === 0 ? json(503, errBody('INTERNAL')) : json(200, { id: 9 })));
    await expect(client(h.f, { maxRetries: 2 }).me()).resolves.toEqual({ id: 9 });
    expect(h.calls).toHaveLength(2);
  });

  it('Retry-After 支持秒数与 HTTP 日期', async () => {
    const h1 = harness(() => json(429, errBody('RATE_LIMITED'), { 'retry-after': '2' }));
    try {
      await client(h1.f, { maxRetries: 0 }).me();
    } catch (e) {
      expect((e as ApiError).retryAfterMs).toBe(2000);
      expect((e as ApiError).retryable()).toBe(true);
    }

    const when = new Date(Date.now() + 5000).toUTCString();
    const h2 = harness(() => json(503, errBody('INTERNAL'), { 'retry-after': when }));
    try {
      await client(h2.f, { maxRetries: 0 }).me();
    } catch (e) {
      const ms = (e as ApiError).retryAfterMs!;
      expect(ms).toBeGreaterThan(3000);
      expect(ms).toBeLessThanOrEqual(5000);
    }
  });

  it('网络异常归成 NETWORK 码；幂等请求才重试', async () => {
    const h = harness(() => {
      throw new Error('socket hang up');
    });
    await expect(client(h.f, { maxRetries: 1 }).me()).rejects.toMatchObject({ code: 'NETWORK' });
    expect(h.calls).toHaveLength(2);
  });
});

describe('协议细节', () => {
  it('changes 按 JSONL 逐行解析，空行跳过', async () => {
    const rows: ChangeRow[] = [
      { rev: 2, path: 'a.psd', op: 'modify', kind: 'file', blob_hash: 'h1', size: 1, mode: 0, mtime: 0 },
      { rev: 3, path: 'b.psd', op: 'delete', kind: 'file', blob_hash: null, size: 0, mode: 0, mtime: 0 },
    ];
    const h = harness(() =>
      new Response(rows.map((r) => JSON.stringify(r)).join('\n') + '\n\n', { status: 200 }),
    );
    await expect(client(h.f).changes('art', { from: 2 })).resolves.toEqual(rows);
  });

  it('blobsMissing 超过 1000 个自动分批', async () => {
    const h = harness((c) => {
      const n = JSON.parse(c.bodyText!).hashes.length;
      return json(200, { missing: [`m${n}`] });
    });
    const hashes = Array.from({ length: 2300 }, (_, i) => `h${i}`);
    const missing = await client(h.f).blobsMissing('art', hashes);
    expect(h.calls).toHaveLength(3);
    expect(missing).toEqual(['m1000', 'm1000', 'm300']);
  });

  it('空响应体不炸，解析成空对象', async () => {
    const h = harness(() => new Response('', { status: 200 }));
    await expect(client(h.f).listRepos()).resolves.toEqual([]);
  });

  it('repoInfo / tree / log 的查询参数落位', async () => {
    const h = harness(() => json(200, { items: [], total: 0, head_rev: 3 }));
    const c = client(h.f);
    await c.repoInfo('art');
    await c.tree('art', { rev: 5, prefix: 'sub/dir', depth: 16 });
    await c.log('art', { prefix: 'a', from: 2, limit: 10 });
    expect(h.calls[1]!.url).toContain('rev=5');
    expect(h.calls[1]!.url).toContain('prefix=sub%2Fdir');
    expect(h.calls[1]!.url).toContain('depth=16');
    expect(h.calls[2]!.url).toContain('from=2');
    expect(h.calls[2]!.url).toContain('limit=10');
  });

  it('分块上传的完整调用序列', async () => {
    const h = harness((c, i) => {
      if (i === 0) return json(201, { upload_id: 'u1', hash: 'H', size: 10, chunk_size: 4, received_chunks: [0] });
      if (c.url.endsWith('/complete')) return json(200, { ok: true });
      return new Response(null, { status: 204 });
    });
    const c = client(h.f);
    const s = await c.createUpload('art', { hash: 'H', size: 10, chunk_size: 4 });
    expect(s.upload_id).toBe('u1');
    await c.putChunk('art', s.upload_id, 1, new Uint8Array([9]));
    await c.completeUpload('art', s.upload_id);
    expect(h.calls.map((x) => x.method)).toEqual(['POST', 'PUT', 'POST']);
    expect(h.calls[1]!.url).toContain('/blobs/uploads/u1/1');
  });

  it('login 成功后自动持有 token', async () => {
    const h = harness(() =>
      json(200, { token: 'TK', expires_at: '2026-01-01T00:00:00Z', user: { id: 1, username: 'a', is_admin: false } }),
    );
    const c = client(h.f);
    await c.login('a', 'b');
    expect(c.tokenValue).toBe('TK');
    await c.logout();
    expect(c.tokenValue).toBeNull();
  });

  it('listRepos 直接返回 items', async () => {
    const items = [{ id: 1, name: 'art' }] as unknown as RepoSummary[];
    const h = harness(() => json(200, { items, total: 1 }));
    await expect(client(h.f).listRepos()).resolves.toEqual(items);
  });

  it('锁列表解包 items', async () => {
    const items = [{ id: 7, path: 'a.psd' }] as unknown as LockInfo[];
    const h = harness(() => json(200, { items, total: 1 }));
    await expect(client(h.f).listLocks('art')).resolves.toEqual(items);
  });
});
