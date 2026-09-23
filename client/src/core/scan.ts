/**
 * 工作副本扫描与状态计算（§6.2 文件状态机）。
 *
 * 三条不变量：
 *   1. 路径一律 posix + NFC（§6.6 ①），否则 macOS 上"看起来同名"的两个文件会状态错乱；
 *   2. 符号链接一律跳过（§6.6 ④）——v1 不入库，避免目录逃逸与跨平台语义混乱；
 *   3. mtime + size 只作**快速跳过**的启发式，最终判定看内容哈希（§6.1）。
 */

import { readdir, stat } from 'node:fs/promises';
import { join } from 'node:path';
import type { EntryStatus, Wc } from './db.js';
import { WC_DIR } from './db.js';
import { hashFile } from './hash.js';
import { IGNORE_FILE, IgnoreRules } from './ignore.js';
import { normalizeNfc } from './paths.js';

/** 状态机全集（含两个纯计算态 `unversioned` / `ignored`，它们不落库）。 */
export type StatusCode = EntryStatus | 'unversioned' | 'ignored';

export interface DiskEntry {
  path: string;
  absPath: string;
  kind: 'file' | 'dir';
  size: number;
  mtime_ms: number;
  /** 仅保留可执行位。 */
  mode: number;
}

export interface ScanResult {
  entries: DiskEntry[];
  ignored: string[];
  /** 存在 `<path>.mine` 伴生文件的主体路径集合（§6.2 `conflicted`）。 */
  conflicts: Set<string>;
  warnings: string[];
}

export const MINE_SUFFIX = '.mine';

export interface ScanOptions {
  /** 忽略规则来源；不给则扫描时顺带加载各层 `.b-artifactignore`。 */
  ignore?: IgnoreRules;
  /** 哈希计算的并发上限。 */
  concurrency?: number;
}

/** 递归扫描工作副本（跳过 `.b-artifact` 与符号链接）。 */
export async function scanDisk(root: string, opts: ScanOptions = {}): Promise<ScanResult> {
  const ignore = opts.ignore ?? new IgnoreRules();
  const entries: DiskEntry[] = [];
  const ignored: string[] = [];
  const conflicts = new Set<string>();
  const warnings: string[] = [];

  // 栈式遍历：避免递归深度随目录层级爆掉。
  type Frame = { rel: string; abs: string };
  const stack: Frame[] = [{ rel: '', abs: root }];

  while (stack.length > 0) {
    const frame = stack.pop()!;
    // 每层目录都可能带自己的 .b-artifactignore（base = 该目录）。
    if (!opts.ignore) {
      ignore.addFile(frame.rel, join(frame.abs, IGNORE_FILE));
    }

    let dirents;
    try {
      dirents = await readdir(frame.abs, { withFileTypes: true });
    } catch (e) {
      warnings.push(`无法读取目录 ${frame.rel || '.'}：${(e as Error).message}`);
      continue;
    }

    for (const d of dirents) {
      const name = normalizeNfc(d.name);
      const rel = frame.rel === '' ? name : `${frame.rel}/${name}`;
      const abs = join(frame.abs, d.name);

      if (frame.rel === '' && name === WC_DIR) continue; // 元数据目录不参与版本管理

      let isDir = d.isDirectory();
      let isFile = d.isFile();
      if (d.isSymbolicLink()) {
        warnings.push(`跳过符号链接：${rel}（v1 不支持 symlink）`);
        continue;
      }

      if (name.endsWith(MINE_SUFFIX)) {
        // 冲突伴生文件：登记主体路径，自身不作为版本化条目。
        conflicts.add(rel.slice(0, -MINE_SUFFIX.length));
        continue;
      }

      if (ignore.matches(rel, isDir)) {
        ignored.push(rel);
        continue;
      }

      if (isDir) {
        entries.push({ path: rel, absPath: abs, kind: 'dir', size: 0, mtime_ms: 0, mode: 0 });
        stack.push({ rel, abs });
        continue;
      }
      if (!isFile) {
        warnings.push(`跳过非常规文件：${rel}`);
        continue;
      }

      const st = await stat(abs);
      isDir = st.isDirectory();
      isFile = st.isFile();
      if (isDir) {
        entries.push({ path: rel, absPath: abs, kind: 'dir', size: 0, mtime_ms: 0, mode: 0 });
        stack.push({ rel, abs });
        continue;
      }
      entries.push({
        path: rel,
        absPath: abs,
        kind: 'file',
        size: st.size,
        mtime_ms: Math.floor(st.mtimeMs),
        mode: st.mode & 0o111,
      });
    }
  }

  entries.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
  return { entries, ignored, conflicts, warnings };
}

export interface StatusItem {
  path: string;
  kind: 'file' | 'dir';
  status: StatusCode;
  base_rev: number;
  base_hash: string | null;
  size: number;
  /** 服务端该路径已变更为更新的修订（由 update 扫描填）。 */
  needs_update: boolean;
}

export interface StatusOptions {
  /** 强制重算哈希，不走 mtime 启发式。 */
  forceHash?: boolean;
  concurrency?: number;
  /** 服务端已变更路径（update 后填 needs_update）。 */
  serverChanged?: ReadonlySet<string>;
}

/**
 * 状态计算：`scan(root) × wc.entries` 的集合运算 + pristine 哈希比对。
 *
 * 目录不做哈希比对，只判存在性；文件走哈希（命中 mtime+size 启发式则跳过）。
 */
export async function computeStatus(
  wc: Wc,
  opts: StatusOptions = {},
): Promise<StatusItem[]> {
  const scan = await scanDisk(wc.root);
  const disk = new Map(scan.entries.map((e) => [e.path, e]));
  const base = new Map(wc.allEntries().map((e) => [e.path, e]));

  const out: StatusItem[] = [];

  // ① 基线视角：normal / modified / deleted / missing / conflicted
  for (const b of base.values()) {
    const d = disk.get(b.path);
    const item = (status: StatusCode): StatusItem => ({
      path: b.path,
      kind: b.kind,
      status,
      base_rev: b.base_rev,
      base_hash: b.base_hash,
      size: d?.size ?? b.size,
      needs_update: opts.serverChanged?.has(b.path) ?? false,
    });

    if (!d) {
      out.push(item(wc.pendingOp(b.path) === 'delete' ? 'deleted' : 'missing'));
      continue;
    }
    if (scan.conflicts.has(b.path)) {
      out.push(item('conflicted'));
      continue;
    }
    if (d.kind === 'dir' || b.kind === 'dir') {
      out.push(item('normal'));
      continue;
    }
    // mtime + size 完全一致 → 视为未改（启发式，§6.1）。
    if (!opts.forceHash && d.mtime_ms === b.mtime_ms && d.size === b.size) {
      out.push(item('normal'));
      continue;
    }
    const h = await hashFile(d.absPath);
    out.push(item(h === b.base_hash ? 'normal' : 'modified'));
  }

  // ② 磁盘视角：added / unversioned
  for (const d of disk.values()) {
    if (base.has(d.path)) continue;
    const staged = wc.pendingOp(d.path);
    out.push({
      path: d.path,
      kind: d.kind,
      status: staged === 'add' ? 'added' : 'unversioned',
      base_rev: 0,
      base_hash: null,
      size: d.size,
      needs_update: false,
    });
  }

  for (const p of scan.ignored) {
    const d = disk.get(p);
    out.push({
      path: p,
      kind: d?.kind ?? 'file',
      status: 'ignored',
      base_rev: 0,
      base_hash: null,
      size: d?.size ?? 0,
      needs_update: false,
    });
  }

  out.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
  return out;
}

/** 状态是否为"有本地改动"（用于阻止 update / 提示提交）。 */
export function isLocalChange(s: StatusCode): boolean {
  return (
    s === 'modified' || s === 'added' || s === 'deleted' || s === 'missing' || s === 'conflicted'
  );
}
