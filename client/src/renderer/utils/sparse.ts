/**
 * 部分检出的前缀选择逻辑（纯函数，§6.5 勾选式部分检出）。
 *
 * 部分检出是**白名单前缀**语义（服务端按前缀子树过滤），所以选择集必须保持
 * "最小且无歧义"：只要 `a` 在集合里，`a/b` 就是冗余的。这里把这条不变量
 * 收在 `normalizeSparse` 一处，勾选/取消都从它推导，避免界面上出现
 * "看起来勾了两个、其实只生效一个"的混乱。
 */

import type { TreeEntryDto } from '@shared/dto';

/** 树节点的展示形状（`el-tree` 直接吃这个）。 */
export interface TreeNode {
  path: string;
  name: string;
  kind: 'file' | 'dir';
  /** 叶子节点（文件）不可勾选：部分检出前缀只针对目录。 */
  leaf: boolean;
}

/** 路径取最后一段（根路径显示为空串，由调用方决定占位）。 */
export function nodeName(path: string): string {
  return path.split('/').filter(Boolean).pop() ?? '';
}

export function toTreeNode(entry: TreeEntryDto): TreeNode {
  return {
    path: entry.path,
    name: nodeName(entry.path),
    kind: entry.kind,
    leaf: entry.kind === 'file',
  };
}

/** `a` 是否严格位于 `b` 的祖先链上（按段比较，避免 `ab` 误判为 `a/b` 的祖先）。 */
export function isAncestorOf(ancestor: string, path: string): boolean {
  return ancestor !== '' && path.startsWith(`${ancestor}/`);
}

/** 该路径是否已被选中集合覆盖（自身被选中，或某个祖先被选中）。 */
export function isCovered(prefixes: readonly string[], path: string): boolean {
  return prefixes.some((p) => p === path || isAncestorOf(p, path));
}

/**
 * 规范化：去空、去重、**去掉被祖先覆盖的后代**、排序。
 *
 * 这是选择集的唯一不变量入口——勾选与取消都走它。
 */
export function normalizeSparse(prefixes: readonly string[]): string[] {
  const cleaned = [...new Set(prefixes.map((p) => p.trim()).filter(Boolean))];
  // 先按段数少的（更靠祖先的）在前，再按字典序，保证"祖先先入集合"
  cleaned.sort((a, b) => {
    const da = a.split('/').length;
    const db = b.split('/').length;
    return da !== db ? da - db : a < b ? -1 : a > b ? 1 : 0;
  });
  const out: string[] = [];
  for (const p of cleaned) {
    if (out.some((kept) => isAncestorOf(kept, p))) continue; // 已被祖先覆盖
    out.push(p);
  }
  return out.sort();
}

/**
 * 切换一个目录的勾选状态。
 *
 *   - 取消：从集合里移除它
 *   - 勾选：移除覆盖它的**祖先**（用户想收窄范围）、同时去掉它的**后代**（放宽到整棵）
 *
 * 这样每次点击之后，集合里既不会出现"祖先 + 后代"并存，也不会出现
 * "点了没反应"（勾一个已被祖先覆盖的子目录时会自动把祖先换成它）。
 */
export function toggleSparse(prefixes: readonly string[], path: string): string[] {
  const cur = normalizeSparse(prefixes);
  if (cur.includes(path)) return cur.filter((p) => p !== path);
  const next = cur.filter((p) => !isAncestorOf(p, path) && !isAncestorOf(path, p));
  next.push(path);
  return normalizeSparse(next);
}

/** 从集合里去掉某个前缀（界面上的标签 × 按钮）。 */
export function removeSparse(prefixes: readonly string[], path: string): string[] {
  return normalizeSparse(prefixes).filter((p) => p !== path);
}

/** 一句话说明当前选择。 */
export function describeSparse(prefixes: readonly string[]): string {
  const list = normalizeSparse(prefixes);
  if (list.length === 0) return '全量检出（勾选目录可只检出指定子树）';
  return `部分检出：${list.join('、')}（共 ${list.length} 个前缀）`;
}

/**
 * 路径合法性（渲染层快速反馈，权威校验仍在服务端，§6.6）。
 *
 * 只拦明显写错的：空段、`.` / `..`、段首尾空格、段尾点、反斜杠。
 */
export function validateSparsePath(path: string): string | null {
  if (!path) return '路径不能为空';
  if (path.includes('\\')) return '路径只能用 / 分隔';
  if (path.includes('\0')) return '路径含非法字符';
  for (const seg of path.split('/')) {
    if (!seg) return '路径里有空段（开头/结尾/连续斜杠）';
    if (seg === '.' || seg === '..') return '不允许 . 或 .. 段';
    if (seg !== seg.trim()) return '路径段首尾不能有空格';
    if (seg.endsWith('.')) return '路径段不能以点结尾';
  }
  return null;
}

/** 从树里挑出可勾选的目录（文件不参与部分检出前缀）。 */
export function selectableDirs(nodes: readonly TreeNode[]): TreeNode[] {
  return nodes.filter((n) => n.kind === 'dir');
}
