/**
 * 路径工具（§6.6 文件名与平台兼容性）。
 *
 * 核心原则：服务端永远存规范化的 posix 相对路径（UTF-8 NFC、`/` 分隔），
 * 兼容性问题全部在客户端检出/提交边界消化。
 */

import { existsSync, rmSync, writeFileSync } from 'node:fs';

/** 单段名上限（字节）。 */
export const MAX_SEGMENT_BYTES = 255;
/** 全路径上限（字节）。 */
export const MAX_PATH_BYTES = 1024;

/** Windows 保留设备名（不区分大小写，含扩展名也算：CON.txt 同样非法）。 */
const WINDOWS_RESERVED = new Set([
  'CON',
  'PRN',
  'AUX',
  'NUL',
  'COM1',
  'COM2',
  'COM3',
  'COM4',
  'COM5',
  'COM6',
  'COM7',
  'COM8',
  'COM9',
  'LPT1',
  'LPT2',
  'LPT3',
  'LPT4',
  'LPT5',
  'LPT6',
  'LPT7',
  'LPT8',
  'LPT9',
]);

/** Windows 非法字符（`/` 在这里合法——它是分隔符，段内出现才非法）。 */
const ILLEGAL_CHARS = /[<>:"\\|?*]/;
/** 控制字符（0x00–0x1F 与 0x7F）。 */
const CONTROL_CHARS = /[\u0000-\u001F\u007F]/;

/** 反斜杠转斜杠。 */
export function toPosix(p: string): string {
  return p.replace(/\\/g, '/');
}

/** NFC 规范化。 */
export function normalizeNfc(p: string): string {
  return p.normalize('NFC');
}

/** 判断字符串是否已是 NFC 形态。 */
export function isNfc(p: string): boolean {
  return p.normalize('NFC') === p;
}

/** UTF-8 字节长度。 */
export function utf8Len(s: string): number {
  return Buffer.byteLength(s, 'utf8');
}

/** 拆段（已假定是 posix 相对路径）。 */
export function segments(p: string): string[] {
  return p.split('/').filter((s) => s.length > 0);
}

/** posix 路径拼接，自动去掉多余分隔符与 `.` 段。 */
export function joinPath(...parts: string[]): string {
  const out: string[] = [];
  for (const raw of parts) {
    if (!raw) continue;
    for (const seg of toPosix(raw).split('/')) {
      if (!seg || seg === '.') continue;
      out.push(seg);
    }
  }
  return out.join('/');
}

/** 父路径；根节点（无 `/`）返回 null。 */
export function parentOf(p: string): string | null {
  const i = p.lastIndexOf('/');
  return i < 0 ? null : p.slice(0, i);
}

/** 末段名。 */
export function baseName(p: string): string {
  const i = p.lastIndexOf('/');
  return i < 0 ? p : p.slice(i + 1);
}

/**
 * `prefix` 是否覆盖 `p`（自身视为被覆盖）。
 * 用于部分检出范围判定与目录锁递归（§5.1）。
 */
export function covers(prefix: string, p: string): boolean {
  if (prefix === '' || prefix === p) return true;
  return p.startsWith(prefix + '/');
}

/**
 * 路径合法性校验（§6.6 ③④）。
 * 返回 `null` 表示合法；否则返回人类可读的违规原因。
 */
export function validatePath(p: string): string | null {
  if (p === '') return '路径为空';
  if (p.startsWith('/')) return `路径必须是相对路径：${p}`;
  if (toPosix(p) !== p) return `路径含反斜杠：${p}`;
  if (!isNfc(p)) return `路径非 NFC 规范化：${p}`;
  if (utf8Len(p) > MAX_PATH_BYTES) {
    return `路径超长（${utf8Len(p)} > ${MAX_PATH_BYTES} 字节）：${p}`;
  }

  for (const seg of segments(p)) {
    if (seg === '.' || seg === '..') return `路径含 . 或 .. 段：${p}`;
    if (utf8Len(seg) > MAX_SEGMENT_BYTES) return `路径段超长：${seg}`;
    if (CONTROL_CHARS.test(seg)) return `路径段含控制字符：${seg}`;
    if (ILLEGAL_CHARS.test(seg)) return `路径段含非法字符 < > : " \\ | ? *：${seg}`;
    // 段以空格或点结尾在 Windows 上会被静默截断。
    if (seg.endsWith(' ') || seg.endsWith('.')) return `路径段以空格或点结尾：${seg}`;
    const stem = seg.split('.')[0]?.toUpperCase() ?? '';
    if (WINDOWS_RESERVED.has(stem)) return `路径段是 Windows 保留名：${seg}`;
  }
  return null;
}

/**
 * 大小写折叠冲突检测（§6.6 ②）。
 *
 * 大小写不敏感文件系统上，`a.psd` 与 `A.psd` 无法共存。传入同一目录下的
 * 全部分段名，返回互为 case-only 重复的路径集合。
 */
export function findCaseCollisions(paths: readonly string[]): string[][] {
  const buckets = new Map<string, string[]>();
  for (const p of paths) {
    const key = p.toLowerCase();
    const list = buckets.get(key);
    if (list) list.push(p);
    else buckets.set(key, [p]);
  }
  return [...buckets.values()].filter((g) => g.length > 1);
}

/**
 * 大小敏感文件系统探测：在给定目录里造两个仅大小写不同的名字，看能否共存。
 * 结果按目录缓存，避免重复 I/O。
 */
const caseSensitiveCache = new Map<string, boolean>();

export function isCaseSensitiveFs(dir: string): boolean {
  const cached = caseSensitiveCache.get(dir);
  if (cached !== undefined) return cached;
  let result = true;
  const probe = `${dir}/.b-artifact-case-probe`;
  try {
    writeFileSync(probe, '', 'utf8');
    result = !existsSync(probe.toUpperCase());
    rmSync(probe, { force: true });
  } catch {
    // 探测失败时保守按「不敏感」处理：宁可多报冲突，也不要在 Windows 上静默覆盖。
    result = false;
  }
  caseSensitiveCache.set(dir, result);
  return result;
}
