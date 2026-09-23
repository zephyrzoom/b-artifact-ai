import { mkdir, mkdtemp, rm, stat, symlink, utimes, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { Wc, type EntryRow, type WcMeta } from '../src/core/db.js';
import {
  EMPTY_SHA256,
  hashBuffer,
  hashFile,
  hashFileSync,
  needsChunkedUpload,
  WHOLE_BLOB_LIMIT,
} from '../src/core/hash.js';
import { computeStatus, isLocalChange, scanDisk, type StatusItem } from '../src/core/scan.js';

let dir: string;

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), 'ba-scan-'));
});

afterEach(async () => {
  await rm(dir, { recursive: true, force: true });
});

function meta(over: Partial<WcMeta> = {}): WcMeta {
  return {
    server: 'http://127.0.0.1:1/api/v1',
    repo: 'art',
    revision: 1,
    sparse_paths: [],
    user: 'alice',
    cache_dir: join(dir, 'cache'),
    ...over,
  };
}

async function put(rel: string, content: string): Promise<void> {
  const abs = join(dir, ...rel.split('/'));
  await mkdir(join(abs, '..'), { recursive: true });
  await writeFile(abs, content, 'utf8');
}

/** 建库并把给定文件落成"已提交基线"（base_hash/mtime/size 都按磁盘真值）。 */
async function seed(files: Record<string, string>): Promise<Wc> {
  const wc = Wc.init(dir, meta());
  const rows: EntryRow[] = [];
  for (const [rel, content] of Object.entries(files)) {
    await put(rel, content);
    const st = await stat(join(dir, ...rel.split('/')));
    rows.push({
      path: rel,
      kind: 'file',
      base_rev: 1,
      base_hash: await hashFile(join(dir, ...rel.split('/'))),
      size: st.size,
      mtime_ms: Math.floor(st.mtimeMs),
      mode: 0,
      status: 'normal',
      case_conflict: false,
    });
  }
  wc.tx(() => rows.forEach((r) => wc.upsertEntry(r)));
  return wc;
}

function byPath(items: StatusItem[]): Map<string, StatusItem> {
  return new Map(items.map((i) => [i.path, i]));
}

describe('内容哈希', () => {
  it('空内容哈希是约定的 well-known 值', () => {
    expect(hashBuffer('')).toBe(EMPTY_SHA256);
    expect(hashBuffer(new Uint8Array())).toBe(EMPTY_SHA256);
  });

  it('流式哈希与整块哈希结果一致', async () => {
    const abs = join(dir, 'big.bin');
    await writeFile(abs, Buffer.alloc(300_000, 7), 'utf8');
    expect(await hashFile(abs)).toBe(hashFileSync(abs));
  });

  it('分块阈值按 >64MiB 判定', () => {
    expect(needsChunkedUpload(WHOLE_BLOB_LIMIT)).toBe(false);
    expect(needsChunkedUpload(WHOLE_BLOB_LIMIT + 1)).toBe(true);
    expect(needsChunkedUpload(0)).toBe(false);
  });
});

describe('scanDisk', () => {
  it('跳过 .b-artifact 元数据目录', async () => {
    await put('.b-artifact/wc.db', 'x');
    await put('a.psd', 'x');
    const r = await scanDisk(dir);
    expect(r.entries.map((e) => e.path)).toEqual(['a.psd']);
  });

  it('跳过符号链接并记 warning', async () => {
    await put('real.psd', 'x');
    await symlink(join(dir, 'real.psd'), join(dir, 'link.psd'));
    const r = await scanDisk(dir);
    expect(r.entries.map((e) => e.path)).toEqual(['real.psd']);
    expect(r.warnings.some((w) => w.includes('link.psd') && w.includes('符号链接'))).toBe(true);
  });

  it('.mine 伴生文件登记为主体路径冲突，自身不入库', async () => {
    await put('a.psd', 'server');
    await put('a.psd.mine', 'mine');
    const r = await scanDisk(dir);
    expect(r.entries.map((e) => e.path)).toEqual(['a.psd']);
    expect([...r.conflicts]).toEqual(['a.psd']);
  });

  it('每层目录各自的 .b-artifactignore 都生效', async () => {
    await put('.b-artifactignore', '*.bak\n');
    await put('sub/.b-artifactignore', '*.tmp\n');
    await put('x.bak', '1');
    await put('sub/y.tmp', '1');
    await put('sub/z.psd', '1');
    await put('w.tmp', '1'); // 根的 ignore 里没有 *.tmp
    const r = await scanDisk(dir);
    expect(r.ignored.sort()).toEqual(['sub/y.tmp', 'x.bak']);
    // ignore 文件本身是要入库的（否则换台机器检出后忽略规则就没了）
    expect(r.entries.map((e) => e.path)).toEqual([
      '.b-artifactignore',
      'sub',
      'sub/.b-artifactignore',
      'sub/z.psd',
      'w.tmp',
    ]);
  });

  it('结果按 path 排序', async () => {
    await put('b.psd', '1');
    await put('a/z.psd', '1');
    await put('a/a.psd', '1');
    const r = await scanDisk(dir);
    expect(r.entries.map((e) => e.path)).toEqual(['a', 'a/a.psd', 'a/z.psd', 'b.psd']);
  });
});

describe('computeStatus（§6.2 状态机）', () => {
  it('基线一致 → normal', async () => {
    const wc = await seed({ 'a.psd': 'hello', 'sub/b.png': 'world' });
    const m = byPath(await computeStatus(wc));
    expect(m.get('a.psd')!.status).toBe('normal');
    expect(m.get('sub/b.png')!.status).toBe('normal');
    wc.close();
  });

  it('内容改动 → modified（mtime 变了，哈希兜底）', async () => {
    const wc = await seed({ 'a.psd': 'hello' });
    await put('a.psd', 'hello world');
    const m = byPath(await computeStatus(wc));
    expect(m.get('a.psd')!.status).toBe('modified');
    wc.close();
  });

  it('mtime+size 未变的同尺寸改写走启发式判 normal，forceHash 才揭穿', async () => {
    const wc = await seed({ 'a.psd': 'aaaa' });
    const abs = join(dir, 'a.psd');
    const e = wc.getEntry('a.psd')!;
    await put('a.psd', 'bbbb'); // 同长度、不同内容
    await utimes(abs, new Date(e.mtime_ms), new Date(e.mtime_ms)); // 伪造 mtime 未变
    // utimes 会按毫秒取整（实测 521.6865 → 522），以落盘后的真实值对齐基线
    const now = await stat(abs);
    wc.upsertEntry({ ...e, mtime_ms: Math.floor(now.mtimeMs) });

    const lazy = byPath(await computeStatus(wc));
    expect(lazy.get('a.psd')!.status).toBe('normal'); // 启发式命中，省一次哈希

    const strict = byPath(await computeStatus(wc, { forceHash: true }));
    expect(strict.get('a.psd')!.status).toBe('modified');
    wc.close();
  });

  it('磁盘上消失但没登记删除 → missing', async () => {
    const wc = await seed({ 'a.psd': 'x' });
    await rm(join(dir, 'a.psd'));
    const m = byPath(await computeStatus(wc));
    expect(m.get('a.psd')!.status).toBe('missing');
    wc.close();
  });

  it('已 stage 的删除 → deleted（而非 missing）', async () => {
    const wc = await seed({ 'a.psd': 'x' });
    wc.stage('a.psd', 'delete');
    await rm(join(dir, 'a.psd'));
    const m = byPath(await computeStatus(wc));
    expect(m.get('a.psd')!.status).toBe('deleted');
    wc.close();
  });

  it('存在 .mine 伴生文件 → conflicted', async () => {
    const wc = await seed({ 'a.psd': 'server' });
    await put('a.psd.mine', 'mine');
    const m = byPath(await computeStatus(wc));
    expect(m.get('a.psd')!.status).toBe('conflicted');
    wc.close();
  });

  it('新文件未 add → unversioned；add 后 → added', async () => {
    const wc = await seed({ 'a.psd': 'x' });
    await put('new.psd', 'n');
    expect(byPath(await computeStatus(wc)).get('new.psd')!.status).toBe('unversioned');

    wc.stage('new.psd', 'add');
    expect(byPath(await computeStatus(wc)).get('new.psd')!.status).toBe('added');
    wc.close();
  });

  it('被忽略的文件是 ignored，不会退化成 unversioned', async () => {
    const wc = await seed({ 'a.psd': 'x' });
    await put('.b-artifactignore', '*.bak\n');
    await put('scratch.bak', 'junk');
    const m = byPath(await computeStatus(wc));
    expect(m.get('scratch.bak')!.status).toBe('ignored');
    expect(m.get('scratch.bak')!.size).toBe(0); // 被忽略的不落磁盘元数据
    wc.close();
  });

  it('目录只判存在性：目录被删 → missing', async () => {
    const wc = await seed({ 'art/a.psd': 'x' });
    wc.upsertEntry({
      path: 'art',
      kind: 'dir',
      base_rev: 1,
      base_hash: null,
      size: 0,
      mtime_ms: 0,
      mode: 0o755,
      status: 'normal',
      case_conflict: false,
    });
    await rm(join(dir, 'art'), { recursive: true, force: true });
    const m = byPath(await computeStatus(wc));
    expect(m.get('art')!.status).toBe('missing');
    wc.close();
  });

  it('serverChanged 标记 needs-update', async () => {
    const wc = await seed({ 'a.psd': 'x' });
    const m = byPath(await computeStatus(wc, { serverChanged: new Set(['a.psd']) }));
    expect(m.get('a.psd')!.needs_update).toBe(true);
    expect(m.get('a.psd')!.status).toBe('normal');
    wc.close();
  });
});

describe('isLocalChange', () => {
  it('只有需要提交的五种状态算本地改动', () => {
    for (const s of ['modified', 'added', 'deleted', 'missing', 'conflicted'] as const) {
      expect(isLocalChange(s)).toBe(true);
    }
    for (const s of ['normal', 'ignored', 'unversioned', 'needs-update'] as const) {
      expect(isLocalChange(s)).toBe(false);
    }
  });
});
