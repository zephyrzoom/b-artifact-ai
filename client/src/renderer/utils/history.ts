/**
 * 历史视图的展示逻辑（纯函数，便于单测）。
 *
 * 这里只做"怎么把修订列表讲清楚"——分组、汇总、变更幅度着色。
 * 真正的取数在 `api.log` / `api.treeAt`。
 */

import type { LogEntry } from '@shared/dto';
import { formatBytes, formatTime } from './format';

/** 每页拉多少条修订。 */
export const LOG_PAGE_SIZE = 50;

/** 单条修订的字节增量文案：`+1.2 MB` / `-300 B` / `±0`。 */
export function formatByteDelta(delta: number): string {
  if (delta === 0) return '±0';
  const sign = delta > 0 ? '+' : '-';
  return `${sign}${formatBytes(Math.abs(delta))}`;
}

/**
 * 字节增量的颜色语义。
 *
 * 与**国内股市的涨跌色**一致（涨红跌绿）——这也是本项目其他界面（管理端仓库详情的
 * 修订表）的既有约定：`+` 用红、`-` 用绿。资产库的直觉是"加法更值得注意"。
 */
export function deltaColor(delta: number): string {
  if (delta > 0) return '#f56c6c';
  if (delta < 0) return '#67c23a';
  return 'var(--ba-muted)';
}

export interface RevisionGroup {
  /** 本地日期 `YYYY-MM-DD`。 */
  day: string;
  items: LogEntry[];
}

/** 按本地日期把修订分组（同一天的历史聚在一起，长列表才看得下去）。 */
export function groupByDay(entries: readonly LogEntry[]): RevisionGroup[] {
  const groups: RevisionGroup[] = [];
  const byDay = new Map<string, LogEntry[]>();
  for (const e of entries) {
    const day = (formatTime(e.created_at) || '').slice(0, 10) || '未知时间';
    let bucket = byDay.get(day);
    if (!bucket) {
      bucket = [];
      byDay.set(day, bucket);
      groups.push({ day, items: bucket });
    }
    bucket.push(e);
  }
  return groups;
}

export interface RevisionSummary {
  count: number;
  added: number;
  removed: number;
  authors: string[];
}

/** 一批修订的汇总（用于列表头："近 3 天 12 个修订 · 3 位作者"）。 */
export function summarizeRevisions(entries: readonly LogEntry[]): RevisionSummary {
  let added = 0;
  let removed = 0;
  const authors: string[] = [];
  for (const e of entries) {
    if (e.byte_delta >= 0) added += e.byte_delta;
    else removed += -e.byte_delta;
    if (e.author && !authors.includes(e.author)) authors.push(e.author);
  }
  return { count: entries.length, added, removed, authors };
}

/** 修订号显示：0 是"空仓库初始态"，没有 r0 这个提交。 */
export function revisionLabel(rev: number): string {
  return rev <= 0 ? '—' : `r${rev}`;
}

/**
 * 服务端 `log` 只回当页条数、不给总数，所以用"是否满页"推断还有没有下一页
 * （与管理端审计页同一套做法）。返回下一批的 offset；没有下一页则 null。
 */
export function nextOffset(
  received: number,
  offset: number,
  pageSize: number = LOG_PAGE_SIZE,
): number | null {
  return received < pageSize ? null : offset + received;
}

/** 单行修订的副标题：作者 · 文件数 · 字节增量。 */
export function revisionSubtitle(entry: LogEntry): string {
  const parts = [entry.author || '（未知）'];
  parts.push(`${entry.file_count} 个文件`);
  parts.push(formatByteDelta(entry.byte_delta));
  return parts.join(' · ');
}

/** 说明为空时给一个占位，避免表格里出现空行看不出是"没写说明"还是"没加载出来"。 */
export function revisionMessage(entry: LogEntry): string {
  return entry.message?.trim() ? entry.message : '（无说明）';
}
