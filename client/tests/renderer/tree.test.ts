/**
 * 工作副本目录树（§6.5 M4.8）的纯函数测试。
 *
 * 这些规则决定了"用户看到什么、勾了什么、最后提交了什么"，写错的代价是
 * "改了却提交不了"或"没勾的东西被提交上去"，所以逐条钉住。
 */

import { describe, expect, it } from 'vitest';

import type { LockInfo, StatusItem } from '../../src/shared/dto.js';
import {
  buildTree,
  filesUnder,
  findNode,
  myLocksUnder,
  otherLocksUnder,
  defaultChecked,
  expandKeys,
  filterChanged,
  LARGE_TREE_LIMIT,
  nodeLabel,
  selectedPaths,
  type TreeNode,
} from '../../src/renderer/utils/tree.js';

function item(path: string, status: StatusItem['status'], kind: StatusItem['kind'] = 'file'): StatusItem {
  return { path, kind, status, base_rev: 1, base_hash: null, size: 0, needs_update: false };
}

function lock(path: string, owner: string): LockInfo {
  return {
    id: 1,
    path,
    kind: 'file',
    owner_id: 1,
    owner,
    comment: null,
    created_at: '',
    expires_at: null,
  };
}

/** 把树拍平成 path → node，便于断言。 */
function index(nodes: readonly TreeNode[]): Map<string, TreeNode> {
  const out = new Map<string, TreeNode>();
  const walk = (list: readonly TreeNode[]): void => {
    for (const n of list) {
      out.set(n.path, n);
      walk(n.children);
    }
  };
  walk(nodes);
  return out;
}

describe('buildTree', () => {
  it('按 `/` 聚合成树：目录在前、同类按名字排序', () => {
    const nodes = buildTree([
      item('characters/hero.psd', 'modified'),
      item('characters/bg.psd', 'modified'),
      item('props/table.png', 'added'),
    ]);
    expect(nodes.map((n) => n.path)).toEqual(['characters', 'props']);
    expect(nodes[0]!.kind).toBe('dir');
    expect(nodes[0]!.children.map((c) => c.path)).toEqual([
      'characters/bg.psd',
      'characters/hero.psd',
    ]);
  });

  it('目录汇总子树内的变更数（多级也累积）', () => {
    const nodes = index(
      buildTree([
        item('a/b/c/d.psd', 'modified'),
        item('a/b/e.psd', 'added'),
        item('a/f.psd', 'modified'),
      ]),
    );
    expect(nodes.get('a')!.changed).toBe(3);
    expect(nodes.get('a/b')!.changed).toBe(2);
    expect(nodes.get('a/b/c')!.changed).toBe(1);
    // 干净文件不计入变更
    const withClean = index(buildTree([item('a/x.psd', 'normal'), item('a/y.psd', 'modified')]));
    expect(withClean.get('a')!.changed).toBe(1);
  });

  it('目录节点自身没有 status（汇总才是它的状态），并带上出现过的状态集合', () => {
    const nodes = index(buildTree([item('a/x.psd', 'modified'), item('a/y.psd', 'unversioned')]));
    const dir = nodes.get('a')!;
    expect(dir.status).toBeNull();
    expect(dir.changedStatuses.sort()).toEqual(['modified', 'unversioned']);
  });

  it('未纳管的**目录**条目不计入变更数（新建目录+放文件不该显示"变更 2"）', () => {
    const nodes = index(
      buildTree([
        item('newdir', 'unversioned', 'dir'),
        item('newdir/a.psd', 'unversioned'),
      ]),
    );
    expect(nodes.get('newdir')!.changed).toBe(1);
  });

  it('部分检出留下的隐式目录会被补出来（否则那些文件会从界面上消失）', () => {
    const nodes = index(buildTree([item('deep/nested/only.psd', 'modified')]));
    expect(nodes.get('deep')!.kind).toBe('dir');
    expect(nodes.get('deep/nested')!.kind).toBe('dir');
    expect(nodes.get('deep/nested/only.psd')!.kind).toBe('file');
  });

  it('锁标记区分本人与他人', () => {
    const nodes = index(
      buildTree(
        [item('a/mine.psd', 'modified'), item('a/other.psd', 'modified')],
        [lock('a/mine.psd', 'alice'), lock('a/other.psd', 'bob')],
        { me: 'alice' },
      ),
    );
    expect(nodes.get('a/mine.psd')!.locked).toBe('mine');
    expect(nodes.get('a/other.psd')!.locked).toBe('other');
    expect(nodes.get('a/other.psd')!.lockOwner).toBe('bob');
  });
});

describe('锁的聚合与子树操作（v0.4.17 目录批量锁）', () => {
  const items = [
    item('characters/hero.psd', 'modified'),
    item('characters/bg.psd', 'modified'),
    item('characters/deep/sky.psd', 'modified'),
    item('props/table.png', 'modified'),
  ];
  const locks = [
    lock('characters/hero.psd', 'alice'),
    lock('characters/deep/sky.psd', 'alice'),
    lock('characters/bg.psd', 'bob'),
  ];

  it('目录节点把子树里的锁分"我的 / 他人的"数出来（含更深层）', () => {
    const nodes = index(buildTree(items, locks, { me: 'alice' }));
    const dir = nodes.get('characters')!;
    expect(dir.locksMine).toBe(2);
    expect(dir.locksOther).toBe(1);
    // 深层目录也要有（sky.psd 在 characters/deep 下）
    expect(nodes.get('characters/deep')!.locksMine).toBe(1);
    expect(nodes.get('characters/deep')!.locksOther).toBe(0);
    // 没有锁的目录是 0/0
    expect(nodes.get('props')!.locksMine).toBe(0);
    expect(nodes.get('props')!.locksOther).toBe(0);
  });

  it('filesUnder：文件返回自身，目录返回整棵子树里的文件（目录不算）', () => {
    const nodes = index(buildTree(items, locks, { me: 'alice' }));
    expect(filesUnder(nodes.get('props/table.png')!)).toEqual(['props/table.png']);
    expect(filesUnder(nodes.get('characters')!).sort()).toEqual([
      'characters/bg.psd',
      'characters/deep/sky.psd',
      'characters/hero.psd',
    ]);
  });

  it('myLocksUnder / otherLocksUnder 只取子树内的锁，并按持有者分开', () => {
    const nodes = index(buildTree(items, locks, { me: 'alice' }));
    const dir = nodes.get('characters')!;
    expect(myLocksUnder(dir, locks, 'alice').sort()).toEqual([
      'characters/deep/sky.psd',
      'characters/hero.psd',
    ]);
    expect(otherLocksUnder(dir, locks, 'alice')).toEqual([
      { path: 'characters/bg.psd', owner: 'bob' },
    ]);
    // 子树外的一律不算
    expect(myLocksUnder(nodes.get('props')!, locks, 'alice')).toEqual([]);
  });
});

describe('findNode（右键菜单要把"被点的节点"还原成 TreeNode）', () => {
  it('按路径找得到任意深度的节点；找不到返回 null', () => {
    const nodes = buildTree([
      item('characters/deep/sky.psd', 'modified'),
      item('props/table.png', 'modified'),
    ]);
    expect(findNode(nodes, 'characters')!.kind).toBe('dir');
    expect(findNode(nodes, 'characters/deep/sky.psd')!.kind).toBe('file');
    expect(findNode(nodes, 'nope/none.psd')).toBeNull();
  });
});

describe('filterChanged', () => {
  it('只保留有变更的分支，干净的文件与目录都剪掉', () => {
    const tree = buildTree([
      item('keep/a.psd', 'modified'),
      item('clean/b.psd', 'normal'),
      item('keep/clean.psd', 'normal'),
    ]);
    const out = index(filterChanged(tree));
    expect(out.has('keep')).toBe(true);
    expect(out.has('keep/a.psd')).toBe(true);
    expect(out.has('keep/clean.psd')).toBe(false);
    expect(out.has('clean')).toBe(false);
  });
});

describe('defaultChecked / selectedPaths', () => {
  const items = [
    item('a.psd', 'modified'),
    item('b.psd', 'added'),
    item('c.psd', 'missing'),
    item('d.psd', 'conflicted'),
    item('new.psd', 'unversioned'),
    item('ok.psd', 'normal'),
  ];

  it('默认只勾本地变更：**不含未纳管**、不含冲突与正常', () => {
    const picked = defaultChecked(buildTree(items));
    expect(picked.sort()).toEqual(['a.psd', 'b.psd', 'c.psd']);
  });

  it('未勾选任何东西时提交集合 = 全部本地变更（未纳管仍然排除）', () => {
    const paths = selectedPaths(items, []);
    expect(paths.sort()).toEqual(['a.psd', 'b.psd', 'c.psd']);
  });

  it('勾了未纳管文件才把它算进提交集', () => {
    const paths = selectedPaths(items, ['new.psd']);
    expect(paths).toContain('new.psd');
  });

  it('勾选目录 = 勾整棵子树（与引擎的路径前缀规则一致）', () => {
    const paths = selectedPaths(
      [item('characters/hero.psd', 'modified'), item('characters/new.psd', 'unversioned'), item('props/a.png', 'modified')],
      ['characters'],
    );
    expect(paths.sort()).toEqual(['characters/hero.psd', 'characters/new.psd']);
  });

  it('未纳管的**目录**不算提交项（服务端会隐式补建），但被删除的目录要算', () => {
    const paths = selectedPaths(
      [
        { ...item('newdir', 'unversioned', 'dir') },
        { ...item('newdir/a.psd', 'unversioned') },
        { ...item('olddir', 'deleted', 'dir') },
      ],
      ['newdir', 'newdir/a.psd', 'olddir'],
    );
    expect(paths.sort()).toEqual(['newdir/a.psd', 'olddir']);
  });

  it('冲突条目永远不进提交集（服务端也会拒）', () => {
    expect(selectedPaths([item('d.psd', 'conflicted')], [])).toEqual([]);
  });
});

describe('expandKeys', () => {
  it('只展开含变更的分支，并遵守上限', () => {
    const tree = buildTree([
      item('changed/a.psd', 'modified'),
      item('clean/b.psd', 'normal'),
    ]);
    expect(expandKeys(tree)).toEqual(['changed']);
    expect(expandKeys(tree, 0)).toEqual([]);
  });
});

describe('nodeLabel', () => {
  it('目录给变更数、文件给状态码', () => {
    const nodes = index(buildTree([item('a/x.psd', 'modified'), item('a/y.psd', 'normal')]));
    expect(nodeLabel(nodes.get('a')!)).toBe('变更 1');
    expect(nodeLabel(nodes.get('a/y.psd')!)).toBe('normal');
  });
});

describe('超大工作副本', () => {
  it('上限常量是给界面用的（超过就默认只看变更）', () => {
    expect(LARGE_TREE_LIMIT).toBeGreaterThan(0);
  });
});
