import { describe, expect, it } from 'vitest';

import {
  baseName,
  covers,
  findCaseCollisions,
  isNfc,
  joinPath,
  normalizeNfc,
  parentOf,
  segments,
  toPosix,
  validatePath,
} from '../src/core/paths.js';

describe('路径规范化（§6.6 ①）', () => {
  it('反斜杠统一转斜杠', () => {
    expect(toPosix('a\\b\\c.psd')).toBe('a/b/c.psd');
  });

  it('NFD 输入被规范化为 NFC，且能识别已规范化形态', () => {
    // "é" 的 NFD 形态是 e + U+0301
    const nfd = 'e\u0301';
    const nfc = '\u00e9';
    expect(isNfc(nfd)).toBe(false);
    expect(isNfc(nfc)).toBe(true);
    expect(normalizeNfc(nfd)).toBe(nfc);
  });

  it('拼接时丢弃空段与 . 段', () => {
    expect(joinPath('a', '', 'b', '.', 'c')).toBe('a/b/c');
    expect(joinPath('a\\b', 'c')).toBe('a/b/c');
  });
});

describe('路径段与层级', () => {
  it('segments / parentOf / baseName', () => {
    expect(segments('a/b/c.psd')).toEqual(['a', 'b', 'c.psd']);
    expect(parentOf('a/b/c.psd')).toBe('a/b');
    expect(parentOf('c.psd')).toBeNull();
    expect(baseName('a/b/c.psd')).toBe('c.psd');
  });

  it('covers：前缀覆盖自身与子树，但不覆盖同级同名前缀', () => {
    expect(covers('', 'anything')).toBe(true);
    expect(covers('char', 'char')).toBe(true);
    expect(covers('char', 'char/hero.psd')).toBe(true);
    expect(covers('char', 'chars/hero.psd')).toBe(false);
    expect(covers('char/hero.psd', 'char')).toBe(false);
  });
});

describe('路径合法性校验（§6.6 ③④）', () => {
  it('接受普通路径', () => {
    expect(validatePath('characters/hero/texture.psd')).toBeNull();
  });

  it('拒绝绝对路径与 .. 段', () => {
    expect(validatePath('/abs/path')).toMatch(/相对路径/);
    expect(validatePath('a/../b')).toMatch(/\.\./);
    expect(validatePath('')).toMatch(/为空/);
  });

  it('拒绝非 NFC 路径', () => {
    expect(validatePath('cafe\u0301/logo.png')).toMatch(/NFC/);
  });

  it('拒绝 Windows 非法字符与控制字符', () => {
    expect(validatePath('a/b?.psd')).toMatch(/非法字符/);
    expect(validatePath('a/b|c.psd')).toMatch(/非法字符/);
    expect(validatePath('a/b\u0001c.psd')).toMatch(/控制字符/);
  });

  it('拒绝 Windows 保留设备名（含扩展名）', () => {
    expect(validatePath('a/con.psd')).toMatch(/保留名/);
    expect(validatePath('a/NUL')).toMatch(/保留名/);
    expect(validatePath('a/lpt9.txt')).toMatch(/保留名/);
  });

  it('拒绝以空格或点结尾的段', () => {
    expect(validatePath('a/b.psd ')).toMatch(/空格或点/);
    expect(validatePath('a/b.')).toMatch(/空格或点/);
    expect(validatePath('a/ b.psd')).toBeNull(); // 段首空格不违规
  });

  it('拒绝超长路径与超长段', () => {
    expect(validatePath('x'.repeat(256))).toMatch(/段超长/);
    expect(validatePath(Array.from({ length: 60 }, () => 'y'.repeat(20)).join('/'))).toMatch(/路径超长/);
  });
});

describe('大小写折叠冲突（§6.6 ②）', () => {
  it('识别仅大小写不同的重复路径', () => {
    const groups = findCaseCollisions(['a/Hero.psd', 'a/hero.psd', 'b/x.png', 'a/HERO.psd']);
    expect(groups).toHaveLength(1);
    expect(groups[0]!.sort()).toEqual(['a/HERO.psd', 'a/Hero.psd', 'a/hero.psd']);
  });

  it('无冲突时返回空数组', () => {
    expect(findCaseCollisions(['a.psd', 'b.psd'])).toEqual([]);
  });
});
