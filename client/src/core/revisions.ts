/**
 * 历史修订的只读访问（§6.5 历史视图）。
 *
 * 与 `wc.ts` 的分工：这里**不碰 wc.db**——看历史、下载旧版本是"仓库级"操作，
 * 只要登录态 + 仓库名即可，不需要先检出。所以它是一个独立的、可注入 client 的小模块。
 *
 * ⚠️ 已知性能边界：非 HEAD 修订的列目录（`tree?rev=N`）在服务端是 `list_dir_at_rev` 路径，
 * M1.6 实测在 24 万文件档位是 7.4s（唯一未达标核心项，见 §15.2）。所以这里的用法要克制：
 * 只在用户**选中某个修订**时按需拉一层目录，不做预取、不递归展开。
 */

import { createWriteStream, existsSync } from 'node:fs';
import { mkdir, rename, rm } from 'node:fs/promises';
import { basename, extname, join } from 'node:path';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';

import type { ApiClient, TreeEntry } from './api.js';
import { WcError } from './errors.js';
import { withRetry } from './ioRetry.js';
import { baseName, parentOf } from './paths.js';

export interface RevisionFile {
  path: string;
  blob_hash: string;
  size: number;
  /** 该文件内容最后一次变化的修订。 */
  changed_rev: number;
}

export interface DownloadOutcome {
  /** 实际落盘路径（重名时会自动带修订号后缀）。 */
  saved: string;
  size: number;
  rev: number;
  path: string;
}

/** `rev <= 0` 统一表示 HEAD（与服务端 `tree?rev=0` 的约定一致）。 */
export function normalizeRev(rev: number | undefined): number {
  return !rev || rev < 0 ? 0 : rev;
}

/**
 * 在某个修订下定位一个文件。
 *
 * 只拉**父目录一层**（depth=1）而不是整棵树：既省流量，也避开大仓库上
 * `list_dir_at_rev` 的慢路径。
 */
export async function resolveFileAtRevision(
  client: ApiClient,
  repo: string,
  path: string,
  rev: number,
): Promise<RevisionFile> {
  const parent = parentOf(path) ?? '';
  const tree = await client.tree(repo, { rev: normalizeRev(rev), prefix: parent, depth: 1 });
  const hit = tree.items.find((it: TreeEntry) => it.path === path && it.kind === 'file');
  if (!hit || !hit.blob_hash) {
    const where = rev > 0 ? `r${rev}` : 'HEAD';
    throw new WcError('NOT_FOUND', `${where} 下没有这个文件：${path}`);
  }
  return { path: hit.path, blob_hash: hit.blob_hash, size: hit.size, changed_rev: hit.changed_rev };
}

/**
 * 目标文件名：优先原名，重名时退回 `<stem>.r<rev><ext>`，再重名就加序号。
 *
 * 不做静默覆盖——用户从历史里"下载此版本"往往正是为了跟当前版本对照，
 * 覆盖掉已经在那儿的那份会很难解释。
 */
export function uniqueTargetName(dir: string, path: string, rev: number): string {
  const base = basename(path);
  const first = join(dir, base);
  if (!existsSync(first)) return first;

  const ext = extname(base);
  const stem = base.slice(0, base.length - ext.length);
  const tagged = `${stem}.r${normalizeRev(rev)}${ext}`;
  let candidate = join(dir, tagged);
  for (let i = 2; existsSync(candidate) && i < 100; i++) {
    candidate = join(dir, `${stem}.r${normalizeRev(rev)} (${i})${ext}`);
  }
  return candidate;
}

/**
 * 把某个修订下的文件下载到本地目录（流式，不把大文件读进内存）。
 */
export async function downloadRevisionFile(
  client: ApiClient,
  repo: string,
  opts: { path: string; rev: number; targetDir: string },
): Promise<DownloadOutcome> {
  const rev = normalizeRev(opts.rev);
  const file = await resolveFileAtRevision(client, repo, opts.path, rev);

  await mkdir(opts.targetDir, { recursive: true });
  const dest = uniqueTargetName(opts.targetDir, opts.path, rev);
  const part = `${dest}.part-${process.pid}`;

  const resp = await client.openBlobStream(repo, file.blob_hash);
  if (!resp.body) throw new WcError('IO', `下载失败（空响应）：${file.path}`);

  try {
    await pipeline(Readable.fromWeb(resp.body as never), createWriteStream(part));
    await withRetry(() => rename(part, dest));
  } catch (e) {
    await rm(part, { force: true }).catch(() => undefined);
    throw e;
  }

  return { saved: dest, size: file.size, rev, path: file.path };
}

/** 展示用的短名（面包屑最后一段）。 */
export function shortName(p: string): string {
  return p === '' ? '/' : baseName(p);
}
