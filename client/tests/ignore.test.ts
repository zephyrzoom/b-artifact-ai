import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

import { IgnoreRules } from '../src/core/ignore.js';

function rules(text: string, base = ''): IgnoreRules {
  return new IgnoreRules().add(base, text);
}

describe('.b-artifactignore（gitignore 子集）', () => {
  it('忽略注释与空行', () => {
    const r = rules('# 注释\n\n*.tmp\n');
    expect(r.size).toBe(1);
    expect(r.matches('a.tmp', false)).toBe(true);
  });

  it('未锚定的规则匹配任意层级', () => {
    const r = rules('*.psd');
    expect(r.matches('a.psd', false)).toBe(true);
    expect(r.matches('deep/nest/a.psd', false)).toBe(true);
    expect(r.matches('a.psdx', false)).toBe(false);
  });

  it('前导 / 锚定到 base 目录', () => {
    const r = rules('/build');
    expect(r.matches('build', true)).toBe(true);
    expect(r.matches('sub/build', true)).toBe(false);
  });

  it('尾部 / 只匹配目录', () => {
    const r = rules('cache/');
    expect(r.matches('cache', true)).toBe(true);
    expect(r.matches('cache', false)).toBe(false);
  });

  it('含分隔符的规则自动锚定', () => {
    const r = rules('a/b.txt');
    expect(r.matches('a/b.txt', false)).toBe(true);
    expect(r.matches('x/a/b.txt', false)).toBe(false);
  });

  it('** 跨层级匹配（前导 **/ 含零层，与 git 一致）', () => {
    const r = rules('**/intermediate/**');
    expect(r.matches('proj/intermediate/x.bin', false)).toBe(true);
    expect(r.matches('a/b/intermediate/x.bin', false)).toBe(true);
    // git 语义：前导 `**/` 匹配「所有目录」，含当前目录，故根层也命中。
    expect(r.matches('intermediate/x.bin', false)).toBe(true);
    expect(r.matches('intermediateX/x.bin', false)).toBe(false);
  });

  it('取反规则覆盖先前的忽略（最后匹配生效）', () => {
    const r = rules('*.psd\n!important.psd');
    expect(r.matches('a.psd', false)).toBe(true);
    expect(r.matches('important.psd', false)).toBe(false);
  });

  it('字符类与单字符通配', () => {
    const r = rules('v[0-9].bin\nq?.dat');
    expect(r.matches('v3.bin', false)).toBe(true);
    expect(r.matches('vx.bin', false)).toBe(false);
    expect(r.matches('q1.dat', false)).toBe(true);
    expect(r.matches('q12.dat', false)).toBe(false);
  });

  it('base 目录限定规则作用域', () => {
    const r = rules('*.tmp', 'art');
    expect(r.matches('art/x.tmp', false)).toBe(true);
    expect(r.matches('code/x.tmp', false)).toBe(false);
  });

  it('从文件加载；文件不存在等价于空规则集', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'ba-ignore-'));
    const f = join(dir, '.b-artifactignore');
    await writeFile(f, '*.bak\n', 'utf8');
    const r = new IgnoreRules().addFile('', f);
    expect(r.matches('x.bak', false)).toBe(true);
    const r2 = new IgnoreRules().addFile('', join(dir, 'nope'));
    expect(r2.size).toBe(0);
  });
});
