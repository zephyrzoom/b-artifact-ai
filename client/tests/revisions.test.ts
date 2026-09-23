import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, readdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ApiClient, TreeEntry, TreeResult } from '../src/core/api.js';
import {
  downloadRevisionFile,
  normalizeRev,
  resolveFileAtRevision,
  shortName,
  uniqueTargetName,
} from '../src/core/revisions.js';

let dir: string;

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), 'ba-rev-'));
});

afterEach(async () => {
  await rm(dir, { recursive: true, force: true });
});

function entry(path: string, hash: string | null, over: Partial<TreeEntry> = {}): TreeEntry {
  return {
    path,
    kind: hash ? 'file' : 'dir',
    blob_hash: hash,
    size: 5,
    mode: 0,
    mtime: 0,
    changed_rev: 3,
    ...over,
  };
}

/** 只实现历史视图用到的三个方法。 */
function fakeClient(opts: {
  items?: TreeEntry[];
  blobs?: Record<string, string>;
  onTree?: (q: { rev?: number; prefix?: string; depth?: number }) => void;
}): ApiClient {
  return {
    tree: async (_repo: string, q: { rev?: number; prefix?: string; depth?: number } = {}): Promise<TreeResult> => {
      opts.onTree?.(q);
      return {
        repo: 'art',
        rev: q.rev ?? 0,
        prefix: q.prefix ?? '',
        depth: q.depth ?? 1,
        items: opts.items ?? [],
        total: (opts.items ?? []).length,
      };
    },
    openBlobStream: async (_repo: string, hash: string): Promise<Response> => {
      const content = opts.blobs?.[hash];
      if (content === undefined) return new Response(null, { status: 404 });
      return new Response(new TextEncoder().encode(content), { status: 200 });
    },
  } as unknown as ApiClient;
}

describe('normalizeRev', () => {
  it('0 / 负数 / undefined 统一表示 HEAD', () => {
    expect(normalizeRev(0)).toBe(0);
    expect(normalizeRev(-1)).toBe(0);
    expect(normalizeRev(undefined)).toBe(0);
  });

  it('正数原样保留', () => {
    expect(normalizeRev(7)).toBe(7);
  });
});

describe('resolveFileAtRevision', () => {
  it('只拉父目录一层（大仓库上避开 tree?rev 的慢路径）', async () => {
    const seen: Array<{ prefix?: string; depth?: number; rev?: number }> = [];
    const client = fakeClient({
      items: [entry('docs/readme.txt', 'sha$r')],
      onTree: (q) => seen.push(q),
    });

    const f = await resolveFileAtRevision(client, 'art', 'docs/readme.txt', 2);
    expect(f).toEqual({ path: 'docs/readme.txt', blob_hash: 'sha$r', size: 5, changed_rev: 3 });
    expect(seen).toEqual([{ rev: 2, prefix: 'docs', depth: 1 }]);
  });

  it('根目录下的文件用空前缀', async () => {
    const seen: Array<{ prefix?: string }> = [];
    const client = fakeClient({ items: [entry('a.psd', 'sha$a')], onTree: (q) => seen.push(q) });
    await resolveFileAtRevision(client, 'art', 'a.psd', 0);
    expect(seen[0]!.prefix).toBe('');
  });

  it('该修订下没有这个文件时给出带修订号的明确报错', async () => {
    const client = fakeClient({ items: [] });
    await expect(resolveFileAtRevision(client, 'art', 'gone.psd', 4)).rejects.toMatchObject({
      code: 'NOT_FOUND',
      message: expect.stringContaining('r4'),
    });
  });

  it('同名目录不算命中（只要文件）', async () => {
    const client = fakeClient({ items: [entry('docs/readme.txt', null)] });
    await expect(resolveFileAtRevision(client, 'art', 'docs/readme.txt', 1)).rejects.toMatchObject({
      code: 'NOT_FOUND',
    });
  });

  it('rev=0 的报错文案说 HEAD（而不是 r0）', async () => {
    const client = fakeClient({ items: [] });
    await expect(resolveFileAtRevision(client, 'art', 'x.psd', 0)).rejects.toMatchObject({
      message: expect.stringContaining('HEAD'),
    });
  });
});

describe('uniqueTargetName', () => {
  it('目标不存在时用原名', () => {
    expect(uniqueTargetName(dir, 'docs/readme.txt', 3)).toBe(join(dir, 'readme.txt'));
  });

  it('重名时带修订号后缀，避免静默覆盖用户已有的文件', async () => {
    await writeFile(join(dir, 'readme.txt'), 'existing');
    expect(uniqueTargetName(dir, 'docs/readme.txt', 3)).toBe(join(dir, 'readme.r3.txt'));
  });

  it('再重名就加序号', async () => {
    await writeFile(join(dir, 'readme.txt'), 'a');
    await writeFile(join(dir, 'readme.r3.txt'), 'b');
    expect(uniqueTargetName(dir, 'docs/readme.txt', 3)).toBe(join(dir, 'readme.r3 (2).txt'));
  });

  it('没有扩展名也能生成后缀名', async () => {
    await writeFile(join(dir, 'LICENSE'), 'a');
    expect(uniqueTargetName(dir, 'LICENSE', 9)).toBe(join(dir, 'LICENSE.r9'));
  });
});

describe('downloadRevisionFile', () => {
  it('流式落盘并返回保存路径与大小', async () => {
    const target = join(dir, 'out');
    const client = fakeClient({
      items: [entry('props/table.png', 'sha$t', { size: 11 })],
      blobs: { 'sha$t': 'TABLE-CONTENT' },
    });

    const r = await downloadRevisionFile(client, 'art', {
      path: 'props/table.png',
      rev: 3,
      targetDir: target,
    });

    expect(r.saved).toBe(join(target, 'table.png'));
    expect(r.rev).toBe(3);
    expect(r.size).toBe(11);
    expect(await readFile(r.saved, 'utf8')).toBe('TABLE-CONTENT');
  });

  it('目标目录不存在时自动创建', async () => {
    const target = join(dir, 'deep', 'nested', 'out');
    const client = fakeClient({ items: [entry('a.txt', 'sha$a')], blobs: { 'sha$a': 'x' } });
    const r = await downloadRevisionFile(client, 'art', { path: 'a.txt', rev: 1, targetDir: target });
    expect(existsSync(r.saved)).toBe(true);
  });

  it('重名时保存成带修订号的名字，不动已有文件', async () => {
    const target = join(dir, 'out');
    await mkdir(target, { recursive: true });
    await writeFile(join(target, 'a.txt'), 'ORIGINAL');
    const client = fakeClient({ items: [entry('a.txt', 'sha$a')], blobs: { 'sha$a': 'OLD-REV' } });

    const r = await downloadRevisionFile(client, 'art', { path: 'a.txt', rev: 2, targetDir: target });
    expect(r.saved).toBe(join(target, 'a.r2.txt'));
    expect(await readFile(join(target, 'a.txt'), 'utf8')).toBe('ORIGINAL');
    expect(await readFile(r.saved, 'utf8')).toBe('OLD-REV');
  });

  it('下载失败时不留下半截文件（.part 会被清掉）', async () => {
    const target = join(dir, 'out');
    const client = fakeClient({ items: [entry('a.txt', 'sha$missing')] }); // 没有对应 blob → 404 空响应体
    await expect(
      downloadRevisionFile(client, 'art', { path: 'a.txt', rev: 1, targetDir: target }),
    ).rejects.toBeTruthy();

    const left = existsSync(target) ? await readdir(target) : [];
    expect(left.filter((f) => f.includes('.part'))).toEqual([]);
    expect(left).toEqual([]);
  });

  it('中途失败时把临时文件清干净（不污染目标目录）', async () => {
    const target = join(dir, 'out');
    const client = {
      tree: async (): Promise<TreeResult> => ({
        repo: 'art',
        rev: 1,
        prefix: '',
        depth: 1,
        items: [entry('a.txt', 'sha$a')],
        total: 1,
      }),
      openBlobStream: vi.fn(async () => {
        throw new Error('网络断了');
      }),
    } as unknown as ApiClient;

    await expect(
      downloadRevisionFile(client, 'art', { path: 'a.txt', rev: 1, targetDir: target }),
    ).rejects.toThrow('网络断了');
    expect(existsSync(target) ? await readdir(target) : []).toEqual([]);
  });
});

describe('shortName', () => {
  it('空路径显示为根', () => {
    expect(shortName('')).toBe('/');
    expect(shortName('a/b/c.psd')).toBe('c.psd');
  });
});
