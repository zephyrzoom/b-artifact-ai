/**
 * 状态机的展示与筛选逻辑（纯函数，§6.2）。
 *
 * 状态本身由引擎判定（`core/scan.ts`），这里只负责"怎么给人看、哪些能提交"。
 * 抽成纯函数是为了能单测——"哪些状态可提交"这类规则一旦写错，用户就会在
 * 提交后才从服务端报错里发现，代价很高。
 */

import type { StatusCode, StatusItem } from '@shared/dto';

export const STATUS_LABEL: Record<StatusCode, string> = {
  // v0.4.17：不再需要用户"标记新增"，这种状态直接呈现为「新增」（§6.5）
  unversioned: '新增',
  ignored: '已忽略',
  normal: '正常',
  modified: '已修改',
  added: '待新增',
  deleted: '待删除',
  missing: '缺失',
  conflicted: '冲突',
  'needs-update': '需更新',
};

export type TagType = 'success' | 'warning' | 'danger' | 'info' | 'primary';

export const STATUS_TAG: Record<StatusCode, TagType> = {
  unversioned: 'info',
  ignored: 'info',
  normal: 'success',
  modified: 'warning',
  added: 'primary',
  deleted: 'danger',
  missing: 'danger',
  conflicted: 'danger',
  'needs-update': 'warning',
};

/** 会在下一次提交里被带上的本地状态（§6.3：提交候选=本地变更）。 */
const LOCAL_CHANGE: ReadonlySet<StatusCode> = new Set<StatusCode>([
  'added',
  'modified',
  'deleted',
  'missing',
  'conflicted',
]);

export function isLocalChange(s: StatusCode): boolean {
  return LOCAL_CHANGE.has(s);
}

/** 冲突必须先解决，带上它提交会被服务端/引擎直接拒绝（§6.3）。 */
export function isCommittable(s: StatusCode): boolean {
  return isLocalChange(s) && s !== 'conflicted';
}

/**
 * 是否可以勾选（"这一步有东西可做"）。
 *
 * 注意它**不等于** `isCommittable`：`unversioned` 不在提交集里，但"标记新增"必须先能勾上它，
 * 否则用户永远没法把一个新文件纳入版本控制——表格里全是灰的。冲突同理：
 * 不能提交，但要能选中后 revert。
 */
export function isActionable(s: StatusCode): boolean {
  return s === 'unversioned' || isLocalChange(s);
}

export interface StatusSummary {
  total: number;
  /** 可提交的条目数。 */
  committable: number;
  conflicts: number;
  unversioned: number;
  ignored: number;
}

export function summarize(items: readonly StatusItem[]): StatusSummary {
  let committable = 0;
  let conflicts = 0;
  let unversioned = 0;
  let ignored = 0;
  for (const it of items) {
    if (isCommittable(it.status)) committable += 1;
    if (it.status === 'conflicted') conflicts += 1;
    if (it.status === 'unversioned') unversioned += 1;
    if (it.status === 'ignored') ignored += 1;
  }
  return { total: items.length, committable, conflicts, unversioned, ignored };
}

/** 人类可读的一句话摘要，用于状态栏。 */
export function summaryText(s: StatusSummary): string {
  if (s.total === 0) return '工作副本干净';
  const parts: string[] = [];
  if (s.committable > 0) parts.push(`${s.committable} 个待提交`);
  if (s.conflicts > 0) parts.push(`${s.conflicts} 个冲突`);
  if (s.unversioned > 0) parts.push(`${s.unversioned} 个新增`);
  // 没有待办时也要明确说"干净"——只显示"2 个条目"读者无法判断这是不是正常
  if (parts.length === 0) return `工作副本干净（${s.total} 个条目）`;
  return parts.join(' · ');
}

const RANK: Partial<Record<StatusCode, number>> = {
  conflicted: 0,
  modified: 1,
  added: 2,
  deleted: 3,
  missing: 4,
  'needs-update': 5,
  unversioned: 6,
  normal: 7,
  ignored: 8,
};

/** 排序：需要处理的先冒头，同档按路径字典序（表格稳定，不随扫描顺序抖动）。 */
export function sortStatus(items: readonly StatusItem[]): StatusItem[] {
  return [...items].sort((a, b) => {
    const ra = RANK[a.status] ?? 99;
    const rb = RANK[b.status] ?? 99;
    if (ra !== rb) return ra - rb;
    return a.path < b.path ? -1 : a.path > b.path ? 1 : 0;
  });
}

/** 只保留可提交项（用于"提交全部"与勾选默认值）。 */
export function committableItems(items: readonly StatusItem[]): StatusItem[] {
  return sortStatus(items.filter((it) => isCommittable(it.status)));
}

/** 提交前的阻断原因；null 表示可以提交。 */
export function commitBlockedReason(items: readonly StatusItem[]): string | null {
  const conflicts = items.filter((it) => it.status === 'conflicted');
  if (conflicts.length > 0) {
    return `有 ${conflicts.length} 个冲突未解决，请先处理再提交`;
  }
  if (!items.some((it) => isCommittable(it.status))) {
    return '没有可提交的变更';
  }
  return null;
}
