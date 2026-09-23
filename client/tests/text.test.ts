import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  BINARY_SNIFF_BYTES,
  canMergeText,
  isProbablyBinary,
  readTextCapped,
  type TextReadResult,
} from '../src/core/text.js';

let dir: string;

beforeEach(async () => {
  dir = await mkdtemp(join(tmpdir(), 'ba-text-'));
});

afterEach(async () => {
  await rm(dir, { recursive: true, force: true });
});

async function put(name: string, content: string | Uint8Array): Promise<string> {
  const p = join(dir, name);
  await mkdir(join(p, '..'), { recursive: true });
  await writeFile(p, content);
  return p;
}

describe('isProbablyBinary', () => {
  it('纯 ASCII 文本不是二进制', () => {
    expect(isProbablyBinary(new TextEncoder().encode('hello world\n'))).toBe(false);
  });

  it('中文与 emoji 也不是二进制', () => {
    expect(isProbablyBinary(new TextEncoder().encode('美术资源 · 贴图 😀'))).toBe(false);
  });

  it('含 NUL 即判二进制（psd / png 的常态）', () => {
    expect(isProbablyBinary(new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x00, 0x01]))).toBe(true);
  });

  it('NUL 出现在采样窗口之外就不算（只看前 8000 字节，避免大文件全扫）', () => {
    const big = new Uint8Array(BINARY_SNIFF_BYTES + 10);
    big.fill(0x41); // 'A'
    big[BINARY_SNIFF_BYTES + 5] = 0;
    expect(isProbablyBinary(big)).toBe(false);
  });

  it('非法 UTF-8 判二进制', () => {
    // 0xFF 在 UTF-8 里永远非法
    expect(isProbablyBinary(new Uint8Array([0x41, 0xff, 0xfe, 0x41]))).toBe(true);
  });

  it('空内容不是二进制', () => {
    expect(isProbablyBinary(new Uint8Array([]))).toBe(false);
  });
});

describe('readTextCapped', () => {
  it('文本文件返回内容与大小', async () => {
    const p = await put('a.txt', 'hello');
    expect(await readTextCapped(p)).toEqual({ kind: 'text', text: 'hello', size: 5 });
  });

  it('UTF-8 内容正确解码（中文不乱码）', async () => {
    const p = await put('cn.txt', '美术资源');
    const r = await readTextCapped(p);
    expect(r.kind).toBe('text');
    if (r.kind === 'text') expect(r.text).toBe('美术资源');
  });

  it('二进制文件只报大小，不返回内容', async () => {
    const p = await put('a.psd', new Uint8Array([0x00, 0x01, 0x02, 0x03]));
    expect(await readTextCapped(p)).toEqual({ kind: 'binary', size: 4 });
  });

  it('超过上限直接判 too-large（保护 UI 与内存）', async () => {
    const p = await put('big.txt', 'x'.repeat(100));
    expect(await readTextCapped(p, { limit: 10 })).toEqual({ kind: 'too-large', size: 100 });
  });

  it('文件不存在 / 是目录 → missing（这是正常业务状态，不抛异常）', async () => {
    expect(await readTextCapped(join(dir, 'nope.txt'))).toEqual({ kind: 'missing', size: 0 });
    await mkdir(join(dir, 'subdir'), { recursive: true });
    expect(await readTextCapped(join(dir, 'subdir'))).toEqual({ kind: 'missing', size: 0 });
  });

  it('空文件是文本（空内容也算合法文本，用于 one-side-deleted 展示）', async () => {
    const p = await put('empty.txt', '');
    expect(await readTextCapped(p)).toEqual({ kind: 'text', text: '', size: 0 });
  });

  it('探测函数可注入（便于测边界，也让"整体非法 UTF-8"这条分支可达）', async () => {
    const p = await put('a.txt', 'hello');
    // 假装探测说"是二进制" → 直接走 binary 分支，不再尝试解码
    const r = await readTextCapped(p, { sniff: () => true });
    expect(r.kind).toBe('binary');
  });

  it('整体不是合法 UTF-8 时（探测没覆盖到尾部）仍判二进制', async () => {
    const buf = new Uint8Array(4);
    buf.set(new TextEncoder().encode('ok'), 0);
    buf[2] = 0xff;
    buf[3] = 0xff;
    const p = await put('mixed.bin', buf);
    // 探测只看 NUL 与采样窗口；这里强制探测放行，验证解码兜底
    const r = await readTextCapped(p, { sniff: () => false });
    expect(r.kind).toBe('binary');
  });
});

describe('canMergeText', () => {
  it('任意一侧不是文本就不能合并（二进制只能二选一）', () => {
    const t: TextReadResult = { kind: 'text', text: 'x', size: 1 };
    const b: TextReadResult = { kind: 'binary', size: 9 };
    const m: TextReadResult = { kind: 'missing', size: 0 };
    const big: TextReadResult = { kind: 'too-large', size: 1e9 };
    expect(canMergeText([t, b, t])).toBe(false);
    expect(canMergeText([t, m, t])).toBe(false);
    expect(canMergeText([t, big, t])).toBe(false);
    expect(canMergeText([t, t, t])).toBe(true);
  });
});
