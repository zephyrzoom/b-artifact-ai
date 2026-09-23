/**
 * 工作副本目录树（§6.5 M4.8："工作副本里的文件以目录树显示"）。
 *
 * 数据来源和状态表格完全一样（一次 `status` 扫描的 `StatusItem[]`），这里只做
 * 三件事：**按路径前缀聚合成树**、**汇总目录**、**决定默认展开/勾选**。
 *
 * 全部是纯函数，理由和 `utils/status.ts` 一样：这些规则一旦写错，用户看到的就是
 * "明明改了却提交不了""勾了却没提交上去"，而这类 bug 只有真正点一遍才会暴露。
 */

import type { LockInfo, StatusCode, StatusItem } from '@shared/dto';

/** 树节点。 */
export interface TreeNode {
  /** 仓库内相对路径（posix）。根节点的子项直接用真实路径。 */
  path: string;
  name: string;
  kind: 'file' | 'dir';
  /**
   * 文件用自身状态；目录为 `null` —— 目录的状态是**汇总**，单看某一条会误导
   * （一个目录下既有已修改又有新增，取任何一条都是错的）。
   */
  status: StatusCode | null;
  /** 子树内的变更条目数（文件与目录都计入）。 */
  changed: number;
  /** 子树内出现过的变更状态（去重），用于给目录上色。 */
  changedStatuses: StatusCode[];
  /** 锁：`mine` = 本人持有，`other` = 他人持有，`null` = 没有锁。 */
  locked: 'mine' | 'other' | null;
  lockOwner?: string;
  /**
   * 子树内的锁统计（**目录节点用**）。文件节点恒为 0/1。
   *
   * 为什么要聚合：目录加了锁标记才能回答"为什么我加不上锁"（子树里有人持锁）
   * 与"我该去哪儿解锁"（子树里还有几把我的锁）。
   */
  locksMine: number;
  locksOther: number;
  size: number;
  children: TreeNode[];
}

/** 变更状态（与 `utils/status.ts` 的 `isLocalChange` 对齐，避免循环依赖这里重列一次）。 */
const CHANGE_STATUSES: ReadonlySet<StatusCode> = new Set<StatusCode>([
  'modified',
  'added',
  'deleted',
  'missing',
  'conflicted',
  'unversioned',
]);

export function isChangeStatus(s: StatusCode | null): boolean {
  return s !== null && CHANGE_STATUSES.has(s);
}

/**
 * 树节点数上限。超过就默认只显示变更 —— 24 万文件的工作副本把整棵树塞进 DOM 是不现实的，
 * 而用户看这棵树的目的本来就是"看哪些变了"。
 */
export const LARGE_TREE_LIMIT = 20000;

export interface BuildOptions {
  /** 当前用户名（判定"我的锁"）。 */
  me?: string;
}

/**
 * `StatusItem[] + LockInfo[]` → 树。
 *
 * 中途出现的隐式目录（父路径不在 items 里，例如部分检出只拉了子树）会**补出来**，
 * 否则那些文件会挂不到树上、直接从界面消失。
 */
export function buildTree(
  items: readonly StatusItem[],
  locks: readonly LockInfo[] = [],
  opts: BuildOptions = {},
): TreeNode[] {
  const lockOf = new Map<string, LockInfo>();
  for (const l of locks) lockOf.set(l.path, l);

  const nodes = new Map<string, TreeNode>();
  const root: TreeNode[] = [];

  const ensureDir = (path: string): TreeNode => {
    const existing = nodes.get(path);
    if (existing) return existing;
    const node: TreeNode = {
      path,
      name: path.split('/').pop() ?? path,
      kind: 'dir',
      status: null,
      changed: 0,
      changedStatuses: [],
      locked: null,
      locksMine: 0,
      locksOther: 0,
      size: 0,
      children: [],
    };
    nodes.set(path, node);
    const parent = parentOf(path);
    if (parent === null) root.push(node);
    else ensureDir(parent).children.push(node);
    return node;
  };

  for (const it of items) {
    // 逐级补出目录：`a/b/c.psd` 会保证 `a` 与 `a/b` 都在树上
    const parts = it.path.split('/');
    let prefix = '';
    for (let i = 0; i < parts.length - 1; i++) {
      prefix = prefix === '' ? parts[i]! : `${prefix}/${parts[i]!}`;
      ensureDir(prefix);
    }

    const lock = lockOf.get(it.path);
    // v0.4.17：**未纳管的目录条目不计入变更数**。新建一个目录再往里放文件时，
    // 目录本身也会以 `unversioned` 出现；把它算进去，"变更 2"背后其实只有一个文件
    // （用户按数字去数会发现对不上）。
    const counts = isChangeStatus(it.status) && !(it.kind === 'dir' && it.status === 'unversioned');
    const node: TreeNode = {
      path: it.path,
      name: parts[parts.length - 1]!,
      kind: it.kind,
      status: it.status,
      changed: counts ? 1 : 0,
      changedStatuses: counts ? [it.status] : [],
      locked: lock ? (lock.owner === opts.me ? 'mine' : 'other') : null,
      lockOwner: lock?.owner,
      // 文件节点自带一把锁时先记 1，目录的聚合值在下面 walk 里累加
      locksMine: lock && lock.owner === opts.me ? 1 : 0,
      locksOther: lock && lock.owner !== opts.me ? 1 : 0,
      size: it.size,
      children: [],
    };
    const existing = nodes.get(it.path);
    if (existing) {
      // 之前作为"隐式目录"补出来过（父路径先于子项出现），这里用真实条目覆盖内容
      Object.assign(existing, { ...node, children: existing.children });
    } else {
      nodes.set(it.path, node);
      const parent = parentOf(it.path);
      if (parent === null) root.push(node);
      else ensureDir(parent).children.push(node);
    }

    // 锁向上汇总（本人 / 他人分开数：一个目录下可能两种都有）
    if (node.locksMine > 0 || node.locksOther > 0) {
      for (let p = parentOf(it.path); p !== null; p = parentOf(p)) {
        const dir = nodes.get(p);
        if (!dir) break;
        dir.locksMine += node.locksMine;
        dir.locksOther += node.locksOther;
      }
    }

    // 变更向上汇总
    if (node.changed > 0) {
      for (let p = parentOf(it.path); p !== null; p = parentOf(p)) {
        const dir = nodes.get(p);
        if (!dir) break;
        dir.changed += 1;
        if (!dir.changedStatuses.includes(it.status)) dir.changedStatuses.push(it.status);
      }
    }
  }

  sortTree(root);
  return root;
}

/** 目录在前、同类按名字排序（稳定，不随扫描顺序抖动）。 */
function sortTree(nodes: TreeNode[]): void {
  nodes.sort((a, b) => {
    if (a.kind !== b.kind) return a.kind === 'dir' ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
  for (const n of nodes) if (n.children.length > 0) sortTree(n.children);
}

/** `a/b/c` → `a/b`；顶层返回 null。 */
export function parentOf(path: string): string | null {
  const i = path.lastIndexOf('/');
  return i < 0 ? null : path.slice(0, i);
}

/**
 * "只看变更"：保留有变更的分支，剪掉干净的部分。
 * 目录只要子树里有变更就保留（否则父路径断了，用户看不到东西在哪）。
 */
export function filterChanged(nodes: readonly TreeNode[]): TreeNode[] {
  const out: TreeNode[] = [];
  for (const n of nodes) {
    if (n.kind === 'file') {
      if (n.changed > 0) out.push(n);
      continue;
    }
    const children = filterChanged(n.children);
    if (children.length > 0) out.push({ ...n, children });
    else if (n.changed > 0) out.push({ ...n, children: [] });
  }
  return out;
}

/**
 * "可提交的本地变更"集合（已修改 / 待删除 / 缺失 / 冲突后的改动，**不含 `unversioned`**）。
 *
 * 两条历史注记：
 *
 * 1. **界面不再拿它做"打开时默认勾选"**（v0.4.18）：刚打开工作副本时树上是干净的，
 *    一个勾都没有（真实反馈："目录树都不应该选中"—— 之前预勾全部变更，还会把父目录带成
 *    勾选态，看起来就是目录被选中）。现在它等价于 `selectedPaths(items, [])`，
 *    也就是"用户从未动过勾选 → 提交全部"那条规则；
 * 2. 不含 `unversioned` 的理由没变：未纳管文件默认不进提交集，必须由用户勾一下。
 */
export function defaultChecked(nodes: readonly TreeNode[]): string[] {
  const out: string[] = [];
  const walk = (list: readonly TreeNode[]): void => {
    for (const n of list) {
      if (n.kind === 'dir') {
        walk(n.children);
        continue;
      }
      if (isCommittableStatus(n.status)) out.push(n.path);
    }
  };
  walk(nodes);
  return out;
}

/** 可提交状态（不含 `unversioned` 与 `conflicted`）。 */
function isCommittableStatus(s: StatusCode | null): boolean {
  return s === 'modified' || s === 'added' || s === 'deleted' || s === 'missing';
}

/**
 * 默认展开的节点：**含变更的分支**（最多 `max` 个）。
 *
 * 工作副本动辄上万文件，全展开既没意义也会卡住渲染；而"哪里变了"才是用户打开
 * 这棵树的原因。干净的目录留成折叠状态，需要时自己点开。
 */
export function expandKeys(nodes: readonly TreeNode[], max = 200): string[] {
  const out: string[] = [];
  const walk = (list: readonly TreeNode[]): void => {
    for (const n of list) {
      if (out.length >= max) return;
      if (n.kind === 'dir' && n.changed > 0) {
        out.push(n.path);
        walk(n.children);
      }
    }
  };
  walk(nodes);
  return out;
}

/**
 * 本次提交实际会带上哪些路径 —— 与引擎 `commit()` 的筛选规则**必须一致**
 * （否则界面显示"提交 3 项"、实际提交 5 项，用户会以为丢了什么东西）。
 *
 * 规则：
 *   - 勾选集为空 → 全部本地变更（`unversioned` 除外）；
 *   - 勾选集非空 → 勾中的路径 + 勾中目录的**整棵子树**；
 *   - `unversioned` 只在前者之外**显式勾选**时才计入（§6.5 默认不勾选）。
 */
export function selectedPaths(
  items: readonly StatusItem[],
  checked: readonly string[],
): string[] {
  const set = new Set(checked);
  const inScope = (p: string): boolean =>
    set.size === 0 || set.has(p) || [...set].some((c) => p.startsWith(`${c}/`));

  const out: string[] = [];
  for (const it of items) {
    if (!inScope(it.path)) continue;
    if (isCommittableStatus(it.status)) out.push(it.path);
    else if (it.status === 'unversioned' && set.size > 0) {
      // 未纳管的**目录**不算提交项：它们由服务端 `ensure_dirs` 随文件隐式补建
      // （算进去会让"提交（2）"背后其实只有一个文件，用户会以为漏了什么）
      // 注意：被显式**删除**的目录不算这一类，它是真正的变更（L1 目录删除）。
      if (it.kind === 'file') out.push(it.path);
    }
  }
  return out;
}

/**
 * 按路径在树里找节点（右键菜单要用它把"被点的那个节点"还原成 `TreeNode`，
 * 后续的"可加锁 / 我的锁 / 他人的锁"都能直接复用同一套子树计算）。
 */
export function findNode(nodes: readonly TreeNode[], path: string): TreeNode | null {
  for (const n of nodes) {
    if (n.path === path) return n;
    if (n.kind === 'dir') {
      const hit = findNode(n.children, path);
      if (hit) return hit;
    }
  }
  return null;
}

/** 子树内的全部**文件**路径（含自身是文件的情况）；目录不算 —— 目录不是变更实体。 */
export function filesUnder(node: TreeNode): string[] {
  if (node.kind === 'file') return [node.path];
  const out: string[] = [];
  const walk = (n: TreeNode): void => {
    for (const c of n.children) {
      if (c.kind === 'file') out.push(c.path);
      else walk(c);
    }
  };
  walk(node);
  return out;
}

/** 子树内**本人持有**的锁路径（含自身是文件的情况）。 */
export function myLocksUnder(node: TreeNode, locks: readonly LockInfo[], me: string): string[] {
  const scope = new Set(node.kind === 'file' ? [node.path] : filesUnder(node));
  return locks.filter((l) => l.owner === me && scope.has(l.path)).map((l) => l.path);
}

/** 子树内**他人持有**的锁（路径 + 持有者），用于"加不上锁"的解释与强制解锁入口。 */
export function otherLocksUnder(
  node: TreeNode,
  locks: readonly LockInfo[],
  me: string,
): { path: string; owner: string }[] {
  const scope = new Set(node.kind === 'file' ? [node.path] : filesUnder(node));
  return locks
    .filter((l) => l.owner !== me && scope.has(l.path))
    .map((l) => ({ path: l.path, owner: l.owner }));
}

/** 目录/文件的一行摘要文案（状态列优先展示的信息）。 */
export function nodeLabel(node: TreeNode): string {
  if (node.kind === 'dir') return node.changed > 0 ? `变更 ${node.changed}` : '';
  return node.status ?? '';
}
