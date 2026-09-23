import { describe, expect, it } from 'vitest';

import type { TreeEntryDto } from '../../src/shared/dto.js';
import {
  describeSparse,
  isAncestorOf,
  isCovered,
  nodeName,
  normalizeSparse,
  removeSparse,
  selectableDirs,
  toTreeNode,
  toggleSparse,
  validateSparsePath,
} from '../../src/renderer/utils/sparse.js';

function entry(path: string, kind: 'file' | 'dir'): TreeEntryDto {
  return { path, kind, blob_hash: kind === 'file' ? 'sha$x' : null, size: 0, mode: 0, mtime: 0, changed_rev: 1 };
}

describe('normalizeSparse（选择集的唯一不变量入口）', () => {
  it('去掉空串与空白项', () => {
    expect(normalizeSparse(['', '  ', 'a'])).toEqual(['a']);
  });

  it('去重', () => {
    expect(normalizeSparse(['a', 'a', 'a'])).toEqual(['a']);
  });

  it('去掉被祖先覆盖的后代（保持"最小集合"）', () => {
    expect(normalizeSparse(['a', 'a/b', 'a/b/c', 'd'])).toEqual(['a', 'd']);
  });

  it('不误伤同名前缀（ab 不是 a 的子孙）', () => {
    expect(normalizeSparse(['a', 'ab'])).toEqual(['a', 'ab']);
  });

  it('结果排序稳定（界面标签顺序不会随点击次序抖动）', () => {
    expect(normalizeSparse(['z', 'a', 'm'])).toEqual(['a', 'm', 'z']);
  });

  it('入参顺序不影响结果', () => {
    expect(normalizeSparse(['a/b', 'a', 'c'])).toEqual(normalizeSparse(['c', 'a', 'a/b']));
  });
});

describe('isAncestorOf', () => {
  it('按段比较，前缀相同但不同段不算祖先', () => {
    expect(isAncestorOf('a', 'a/b')).toBe(true);
    expect(isAncestorOf('a/b', 'a/b/c')).toBe(true);
    expect(isAncestorOf('a', 'ab/c')).toBe(false);
    expect(isAncestorOf('a', 'a')).toBe(false); // 自身不算祖先
  });

  it('空串不是任何路径的祖先（根不参与覆盖）', () => {
    expect(isAncestorOf('', 'a/b')).toBe(false);
  });
});

describe('isCovered', () => {
  it('自身被选中即覆盖', () => {
    expect(isCovered(['a'], 'a')).toBe(true);
  });

  it('祖先被选中即覆盖', () => {
    expect(isCovered(['a'], 'a/b/c')).toBe(true);
  });

  it('未选中也不算', () => {
    expect(isCovered(['a'], 'b')).toBe(false);
    expect(isCovered([], 'a')).toBe(false);
  });
});

describe('toggleSparse', () => {
  it('勾选一个新目录就加进去', () => {
    expect(toggleSparse([], 'characters')).toEqual(['characters']);
  });

  it('再点一次就是取消', () => {
    expect(toggleSparse(['characters'], 'characters')).toEqual([]);
  });

  it('勾选父目录时自动去掉已选的后代（放宽到整棵）', () => {
    expect(toggleSparse(['a/b', 'a/c'], 'a')).toEqual(['a']);
  });

  it('勾选子目录时自动去掉覆盖它的祖先（收窄范围）', () => {
    // 用户在"a 整棵"和"只要 a/b"之间改主意，最自然的理解是收窄
    expect(toggleSparse(['a'], 'a/b')).toEqual(['a/b']);
  });

  it('不会出现"点了没反应"（勾已被祖先覆盖的子目录会真的改变集合）', () => {
    const before = normalizeSparse(['a']);
    expect(isCovered(before, 'a/b')).toBe(true);
    expect(toggleSparse(before, 'a/b')).toEqual(['a/b']);
  });

  it('多层级之间互不影响', () => {
    expect(toggleSparse(['a/b', 'x'], 'a')).toEqual(['a', 'x']);
  });

  it('空串不会被加进集合', () => {
    expect(toggleSparse([], '')).toEqual([]);
  });
});

describe('removeSparse', () => {
  it('按前缀精确移除（界面上的标签 ×）', () => {
    expect(removeSparse(['a', 'b'], 'a')).toEqual(['b']);
  });

  it('移除时会顺带规范化', () => {
    expect(removeSparse(['a', 'a/b', 'c'], 'c')).toEqual(['a']);
  });

  it('移除不存在的项不报错', () => {
    expect(removeSparse(['a'], 'zzz')).toEqual(['a']);
  });
});

describe('describeSparse', () => {
  it('空集合说明是全量', () => {
    expect(describeSparse([])).toContain('全量检出');
  });

  it('有集合时列出前缀与个数', () => {
    expect(describeSparse(['a', 'b'])).toBe('部分检出：a、b（共 2 个前缀）');
  });

  it('描述前会规范化（不把冗余项数进去）', () => {
    expect(describeSparse(['a', 'a/b'])).toBe('部分检出：a（共 1 个前缀）');
  });
});

describe('validateSparsePath', () => {
  it('正常路径通过', () => {
    expect(validateSparsePath('characters')).toBeNull();
    expect(validateSparsePath('a/b/c.psd')).toBeNull();
  });

  it('空、空段、. 与 ..、反斜杠、首尾空格、段尾点都被拦下', () => {
    expect(validateSparsePath('')).toMatch(/不能为空/);
    expect(validateSparsePath('/a')).toMatch(/空段/);
    expect(validateSparsePath('a//b')).toMatch(/空段/);
    expect(validateSparsePath('a/')).toMatch(/空段/);
    expect(validateSparsePath('a/../b')).toMatch(/\.\./);
    expect(validateSparsePath('./a')).toMatch(/\./);
    expect(validateSparsePath('a\\b')).toMatch(/\//);
    expect(validateSparsePath('a/ b')).toMatch(/空格/);
    expect(validateSparsePath('a/b.')).toMatch(/点结尾/);
  });
});

describe('树节点', () => {
  it('目录不是叶子、文件是叶子', () => {
    expect(toTreeNode(entry('characters', 'dir')).leaf).toBe(false);
    expect(toTreeNode(entry('characters/hero.psd', 'file')).leaf).toBe(true);
  });

  it('名称取最后一段', () => {
    expect(nodeName('a/b/c.psd')).toBe('c.psd');
    expect(nodeName('a')).toBe('a');
  });

  it('只有目录可作部分检出前缀', () => {
    const nodes = [entry('characters', 'dir'), entry('a.psd', 'file')].map(toTreeNode);
    expect(selectableDirs(nodes).map((n) => n.path)).toEqual(['characters']);
  });
});
