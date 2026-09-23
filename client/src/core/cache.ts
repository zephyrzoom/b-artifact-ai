/**
 * 全局 blob 缓存的统计与清理（§6.5 设置页）。
 *
 * **为什么删缓存是安全的**：缓存是内容寻址的唯一物理副本，但每个工作副本的
 * `<wc>/.b-artifact/pristine/<hash>` 是与它**硬链接**的另一个目录项——删掉缓存里的名字，
 * 链接数减一，pristine 里的内容依然可读，工作副本不会损坏。清理的代价只是下次
 * 需要重新下载（这正是"清理缓存"该有的语义）。
 *
 * 因此这里**只删文件、保留目录骨架**：目录本身很便宜，重建反而要处理并发创建的竞态。
 */

import type { Dirent } from 'node:fs';
import { readdir, rm, stat } from 'node:fs/promises';
import { join } from 'node:path';

/** 下载中转目录（`storeToCache` 之前落的临时文件）。 */
export const CACHE_TMP_DIR = 'tmp';

export interface CacheStats {
  dir: string;
  /** 已缓存的 blob 个数与总字节。 */
  blobs: number;
  bytes: number;
  /** 残留的临时文件（上次下载中断留下的）。 */
  tmpFiles: number;
  tmpBytes: number;
}

export interface CacheClearResult {
  removed: number;
  bytes: number;
  /** 是否连临时目录一起清了（默认会）。 */
  clearedTmp: boolean;
}

async function walk(
  dir: string,
  visit: (abs: string, size: number) => void | Promise<void>,
): Promise<void> {
  let entries: Dirent[];
  try {
    entries = await readdir(dir, { withFileTypes: true });
  } catch {
    return; // 目录不存在 = 空缓存
  }
  for (const e of entries) {
    const abs = join(dir, e.name);
    if (e.isDirectory()) {
      await walk(abs, visit);
      continue;
    }
    if (!e.isFile()) continue; // 符号链接等一律不碰
    try {
      const st = await stat(abs);
      await visit(abs, st.size);
    } catch {
      /* 并发删除，忽略 */
    }
  }
}

/** 统计缓存占用（同时报告残留临时文件，便于界面提示"可清理 N 个"）。 */
export async function cacheStats(cacheDir: string): Promise<CacheStats> {
  const stats: CacheStats = { dir: cacheDir, blobs: 0, bytes: 0, tmpFiles: 0, tmpBytes: 0 };
  const tmpRoot = join(cacheDir, CACHE_TMP_DIR);

  await walk(cacheDir, (abs, size) => {
    if (abs.startsWith(`${tmpRoot}/`) || abs === tmpRoot) {
      stats.tmpFiles += 1;
      stats.tmpBytes += size;
      return;
    }
    stats.blobs += 1;
    stats.bytes += size;
  });

  return stats;
}

/**
 * 清空缓存。
 *
 * @param keepTmp 只清临时文件，保留已缓存的 blob（"清理残留"用）
 */
export async function clearCache(
  cacheDir: string,
  opts: { keepTmp?: boolean } = {},
): Promise<CacheClearResult> {
  const result: CacheClearResult = { removed: 0, bytes: 0, clearedTmp: false };
  const tmpRoot = join(cacheDir, CACHE_TMP_DIR);

  if (opts.keepTmp) {
    await walk(tmpRoot, async (abs, size) => {
      await rm(abs, { force: true });
      result.removed += 1;
      result.bytes += size;
    });
    return result;
  }

  await walk(cacheDir, async (abs, size) => {
    await rm(abs, { force: true });
    result.removed += 1;
    result.bytes += size;
    if (abs.startsWith(`${tmpRoot}/`)) result.clearedTmp = true;
  });
  result.clearedTmp = result.clearedTmp || (await pathExists(tmpRoot));
  // 临时目录整棵删掉（里面的空目录骨架留着没意义，而且常有残留的 .part 目录）
  await rm(tmpRoot, { recursive: true, force: true });

  return result;
}

async function pathExists(p: string): Promise<boolean> {
  try {
    await stat(p);
    return true;
  } catch {
    return false;
  }
}
