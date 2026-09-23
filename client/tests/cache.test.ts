import { existsSync, linkSync } from 'node:fs';
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { CACHE_TMP_DIR, cacheStats, clearCache } from '../src/core/cache.js';
import { blobPath } from '../src/core/pristine.js';

let dir: string;
let cache: string;

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), 'ba-cache-'));
  cache = join(dir, 'cache');
  await mkdir(join(cache, CACHE_TMP_DIR), { recursive: true });
});

afterEach(async () => {
  await rm(dir, { recursive: true, force: true });
});

/** 按真实布局落一个 blob：`<cache>/aa/bb/<hash>`。 */
async function putBlob(hash: string, content: string): Promise<string> {
  const p = blobPath(hash, cache);
  await mkdir(join(p, '..'), { recursive: true });
  await writeFile(p, content);
  return p;
}

const H1 = 'aa11'.padEnd(64, '0');
const H2 = 'bb22'.padEnd(64, '1');

describe('cacheStats', () => {
  it('空缓存（目录不存在）全为 0，不抛异常', async () => {
    const s = await cacheStats(join(dir, 'nope'));
    expect(s).toEqual({ dir: join(dir, 'nope'), blobs: 0, bytes: 0, tmpFiles: 0, tmpBytes: 0 });
  });

  it('分别统计 blob 与残留临时文件', async () => {
    await putBlob(H1, 'hello');
    await putBlob(H2, 'world!');
    await writeFile(join(cache, CACHE_TMP_DIR, 'x.dl-123'), 'partial');

    const s = await cacheStats(cache);
    expect(s.blobs).toBe(2);
    expect(s.bytes).toBe('hello'.length + 'world!'.length);
    expect(s.tmpFiles).toBe(1);
    expect(s.tmpBytes).toBe('partial'.length);
    expect(s.dir).toBe(cache);
  });

  it('分片目录本身不计入（只数文件）', async () => {
    await putBlob(H1, 'x');
    expect((await cacheStats(cache)).blobs).toBe(1);
  });
});

describe('clearCache', () => {
  it('清空后 blob 与临时文件都没了，但目录骨架还在', async () => {
    const p1 = await putBlob(H1, 'hello');
    await writeFile(join(cache, CACHE_TMP_DIR, 'y.dl-1'), 'tmp');

    const r = await clearCache(cache);
    expect(r.removed).toBe(2);
    expect(r.bytes).toBe('hello'.length + 'tmp'.length);
    expect(existsSync(p1)).toBe(false);
    expect(existsSync(join(cache, CACHE_TMP_DIR))).toBe(false);
    await expect(cacheStats(cache)).resolves.toMatchObject({ blobs: 0, bytes: 0 });
  });

  it('keepTmp：只清临时文件，已缓存的 blob 原样保留', async () => {
    const p1 = await putBlob(H1, 'hello');
    await writeFile(join(cache, CACHE_TMP_DIR, 'z.dl-1'), 'tmp');

    const r = await clearCache(cache, { keepTmp: true });
    expect(r.removed).toBe(1);
    expect(existsSync(p1)).toBe(true);
    expect(await readFile(p1, 'utf8')).toBe('hello');
  });

  it('**硬链接安全**：清缓存后 pristine 里的内容仍可读（这正是"清缓存不会毁工作副本"的依据）', async () => {
    const blob = await putBlob(H1, 'payload');
    const pristineDir = join(dir, 'wc', '.b-artifact', 'pristine');
    const pristine = blobPath(H1, pristineDir);
    await mkdir(join(pristine, '..'), { recursive: true });
    // 与真实布局一致：pristine 是与缓存硬链接的另一份目录项
    linkSync(blob, pristine);

    await clearCache(cache);

    expect(existsSync(blob)).toBe(false);
    expect(existsSync(pristine)).toBe(true);
    expect(await readFile(pristine, 'utf8')).toBe('payload');
  });

  it('空缓存上清理是空操作', async () => {
    const r = await clearCache(join(dir, 'empty-cache'));
    expect(r).toMatchObject({ removed: 0, bytes: 0 });
  });

  it('顺带清掉分片空目录之外的东西不会误删（只碰文件）', async () => {
    await mkdir(join(cache, 'aa', 'bb'), { recursive: true });
    await writeFile(join(cache, 'aa', 'bb', 'stray'), 'x');
    const r = await clearCache(cache);
    expect(r.removed).toBe(1);
    // 目录骨架保留（下次写入不用重新建，也避开并发创建的竞态）
    expect(existsSync(join(cache, 'aa', 'bb'))).toBe(true);
  });
});

describe('统计口径', () => {
  it('符号链接不计入也不删除（不跟到工作副本外面去）', async () => {
    const outside = join(dir, 'outside.txt');
    await writeFile(outside, 'do-not-touch');
    await mkdir(join(cache, 'aa', 'bb'), { recursive: true });
    const { symlink } = await import('node:fs/promises');
    await symlink(outside, join(cache, 'aa', 'bb', 'link'));

    const before = await cacheStats(cache);
    expect(before.blobs).toBe(0);
    await clearCache(cache);
    expect(existsSync(outside)).toBe(true);
    expect(await stat(outside)).toBeTruthy();
  });
});
