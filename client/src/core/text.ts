/**
 * 文本判定与受限读取（冲突三方对比用，§6.5 冲突视图）。
 *
 * 美术资源库里绝大多数文件是二进制（psd / png / fbx…），所以"能不能文本合并"
 * 必须先判出来，而不是等把几百 MB 读进内存再发现。三条硬约束：
 *   1. **不读大文件**：超过上限直接判"太大，请用二选一"，不做 diff；
 *   2. **二进制探测**：前若干字节出现 NUL 就当二进制（git 的经验规则）；
 *   3. **严格 UTF-8**：解不出来就是二进制（`TextDecoder({fatal:true})` 直接抛）。
 */

import { open, stat } from 'node:fs/promises';

/** 文本对比的大小上限：超过就不做文本 diff（保护 UI 与内存）。 */
export const TEXT_DIFF_LIMIT = 1024 * 1024;
/** 二进制探测的采样字节数（对齐 git 的 8000）。 */
export const BINARY_SNIFF_BYTES = 8000;

export type TextReadResult =
  | { kind: 'text'; text: string; size: number }
  | { kind: 'binary'; size: number }
  | { kind: 'too-large'; size: number }
  | { kind: 'missing'; size: 0 };

/**
 * 判断一段字节是否"看起来像二进制"。
 *
 * 只看前 `BINARY_SNIFF_BYTES` 字节：要么有 NUL，要么不是合法 UTF-8。
 */
export function isProbablyBinary(buf: Uint8Array): boolean {
  const n = Math.min(buf.length, BINARY_SNIFF_BYTES);
  for (let i = 0; i < n; i++) {
    if (buf[i] === 0) return true;
  }
  try {
    new TextDecoder('utf-8', { fatal: true }).decode(buf.subarray(0, n));
    return false;
  } catch {
    return true;
  }
}

/**
 * 读取一个文件用于文本对比。
 *
 * 返回判别式结果而不是抛异常："不存在 / 太大 / 二进制"都是**正常业务状态**，
 * 界面上要分别给出不同的提示（例如"服务端已删除" vs "二进制文件只能二选一"）。
 */
export async function readTextCapped(
  absPath: string,
  opts: { limit?: number; sniff?: (buf: Uint8Array) => boolean } = {},
): Promise<TextReadResult> {
  const limit = opts.limit ?? TEXT_DIFF_LIMIT;
  const sniff = opts.sniff ?? isProbablyBinary;

  let size: number;
  try {
    const st = await stat(absPath);
    if (!st.isFile()) return { kind: 'missing', size: 0 };
    size = st.size;
  } catch {
    return { kind: 'missing', size: 0 };
  }

  if (size > limit) return { kind: 'too-large', size };

  const fh = await open(absPath, 'r');
  try {
    const buf = Buffer.alloc(size);
    let read = 0;
    while (read < size) {
      const { bytesRead } = await fh.read(buf, read, size - read, read);
      if (bytesRead <= 0) break;
      read += bytesRead;
    }
    const bytes = new Uint8Array(buf.subarray(0, read));
    if (sniff(bytes)) return { kind: 'binary', size };
    try {
      return { kind: 'text', text: new TextDecoder('utf-8', { fatal: true }).decode(bytes), size };
    } catch {
      // 探测只看前 8000 字节，整体仍可能不是合法 UTF-8
      return { kind: 'binary', size };
    }
  } finally {
    await fh.close();
  }
}

/**
 * 三方内容能否做**文本**合并：三方都必须是文本（缺失的一方视为空）。
 *
 * `deleted-remotely` 这类"有一侧根本不存在"的情况不算可合并——那是取舍问题，
 * 不是合并问题。
 */
export function canMergeText(sides: readonly TextReadResult[]): boolean {
  return sides.every((s) => s.kind === 'text');
}
