/**
 * pristine 与全局缓存（§6.1）。
 *
 * 三层布局，磁盘成本算清楚再动手：
 *
 *   全局缓存  ~/.b-artifact/cache/<hash>      ← 唯一物理副本（跨工作副本、跨修订共享）
 *   pristine  <wc>/.b-artifact/pristine/<hash> ← 硬链接到缓存（0 额外空间）
 *   工作文件  <wc>/characters/hero.psd          ← 独立副本
 *
 * 工作文件**必须**是独立副本而不是硬链接：用户用编辑器原地保存（truncate + write）
 * 会顺着硬链接改掉基线，三态对比与 revert 就全废了。这一点与 SVN 的取舍不同——
 * SVN 把工作文件也硬链接到 pristine，代价是依赖外部编辑器不原地写。
 */

import {
  constants,
  copyFile,
  link,
  mkdir,
  rename,
  stat,
  unlink,
} from 'node:fs/promises';
import { join } from 'node:path';
import { WC_DIR } from './db.js';
import { artifactHome } from './home.js';

/** 全局缓存根目录。 */
export function defaultCacheDir(): string {
  return join(artifactHome(), 'cache');
}

/** 两级分片的缓存路径：`<dir>/3f/a1/3fa1c8...`。 */
export function blobPath(hash: string, dir: string): string {
  return join(dir, hash.slice(0, 2), hash.slice(2, 4), hash);
}

export function cachePathFor(hash: string, cacheDir: string): string {
  return blobPath(hash, cacheDir);
}

export function pristinePathFor(wcRoot: string, hash: string): string {
  return blobPath(hash, join(wcRoot, WC_DIR, 'pristine'));
}

export function pristineRoot(wcRoot: string): string {
  return join(wcRoot, WC_DIR, 'pristine');
}

async function exists(p: string): Promise<boolean> {
  try {
    await stat(p);
    return true;
  } catch {
    return false;
  }
}

async function ensureDir(p: string): Promise<void> {
  await mkdir(p, { recursive: true });
}

/**
 * 原子地把内容纳入缓存。
 *
 * `srcAbs` 通常是下载/上传用的临时文件，直接 rename 最省一次全量拷贝；
 * 跨设备（EXDEV）时降级为复制。已有同 hash 内容则丢弃临时文件——内容寻址下二者等价。
 */
export async function storeToCache(
  srcAbs: string,
  hash: string,
  cacheDir: string,
): Promise<string> {
  const dest = cachePathFor(hash, cacheDir);
  if (await exists(dest)) {
    await unlink(srcAbs).catch(() => undefined);
    return dest;
  }
  await ensureDir(join(dest, '..'));
  const tmp = `${dest}.tmp-${process.pid}-${Math.random().toString(36).slice(2, 8)}`;
  try {
    await rename(srcAbs, tmp);
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code !== 'EXDEV') throw e;
    await copyFile(srcAbs, tmp);
    await unlink(srcAbs).catch(() => undefined);
  }
  try {
    await rename(tmp, dest);
  } catch (e) {
    // 并发写入同一 hash 是常态：对方先落位即视为成功。
    if ((e as NodeJS.ErrnoException).code === 'ENOENT' && (await exists(dest))) {
      await unlink(tmp).catch(() => undefined);
      return dest;
    }
    throw e;
  }
  return dest;
}

/** 建立 pristine 硬链接；文件系统不支持时降级为复制。 */
export async function ensurePristine(
  wcRoot: string,
  hash: string,
  cacheDir: string,
): Promise<string> {
  const src = cachePathFor(hash, cacheDir);
  const dest = pristinePathFor(wcRoot, hash);
  if (await exists(dest)) return dest;
  await ensureDir(join(dest, '..'));
  try {
    await link(src, dest);
  } catch (e) {
    const code = (e as NodeJS.ErrnoException).code;
    // EEXIST：并发下别人先建好了；EXDEV/EPERM/ENOSYS：不支持硬链接 → 复制。
    if (code === 'EEXIST') return dest;
    if (code === 'EXDEV' || code === 'EPERM' || code === 'ENOSYS' || code === 'EMLINK') {
      await copyFile(src, dest, constants.COPYFILE_FICLONE_FORCE).catch(() =>
        copyFile(src, dest),
      );
    } else {
      throw e;
    }
  }
  return dest;
}

/**
 * 把基线内容安装到工作副本（检出 / update / revert 都走这里）。
 *
 * 刻意用复制而非硬链接：工作文件要能被用户随意改写，绝不能与基线共享 inode。
 */
export async function installWorkingFile(
  wcRoot: string,
  hash: string,
  destAbs: string,
): Promise<void> {
  const src = pristinePathFor(wcRoot, hash);
  await ensureDir(join(destAbs, '..'));
  const tmp = `${destAbs}.b-artifact-tmp-${process.pid}`;
  await copyFile(src, tmp);
  await rename(tmp, destAbs);
}

/** 工作文件 → pristine 缓存：提交成功后把新基线入库，供后续本地三态对比。 */
export async function importWorkingFile(
  absPath: string,
  cacheDir: string,
  hash: string,
): Promise<void> {
  if (await exists(cachePathFor(hash, cacheDir))) return;
  const dest = cachePathFor(hash, cacheDir);
  await ensureDir(join(dest, '..'));
  const tmp = `${dest}.tmp-${process.pid}-${Math.random().toString(36).slice(2, 8)}`;
  await copyFile(absPath, tmp);
  await rename(tmp, dest);
}

/** 缓存是否存在指定内容（checkout 前求差用）。 */
export async function cacheHas(hash: string, cacheDir: string): Promise<boolean> {
  return exists(cachePathFor(hash, cacheDir));
}
