import { describe, expect, it } from 'vitest';

import type { StatusItem } from '../../src/shared/dto.js';
import {
  committableItems,
  commitBlockedReason,
  isActionable,
  isCommittable,
  isLocalChange,
  sortStatus,
  STATUS_LABEL,
  STATUS_TAG,
  summarize,
  summaryText,
} from '../../src/renderer/utils/status.js';

function item(path: string, status: StatusItem['status'], over: Partial<StatusItem> = {}): StatusItem {
  return { path, kind: 'file', status, base_rev: 1, base_hash: null, size: 0, needs_update: false, ...over };
}

describe('状态标签覆盖度', () => {
  it('九个状态都有中文标签与 tag 颜色（少一个界面上就会显示空白）', () => {
    const codes: StatusItem['status'][] = [
      'unversioned',
      'ignored',
      'normal',
      'modified',
      'added',
      'deleted',
      'missing',
      'conflicted',
      'needs-update',
    ];
    for (const c of codes) {
      expect(STATUS_LABEL[c], `${c} 缺标签`).toBeTruthy();
      expect(STATUS_TAG[c], `${c} 缺颜色`).toBeTruthy();
    }
  });
});

describe('isLocalChange / isCommittable', () => {
  it('本地变更集是 added/modified/deleted/missing/conflicted', () => {
    for (const s of ['added', 'modified', 'deleted', 'missing', 'conflicted'] as const) {
      expect(isLocalChange(s), s).toBe(true);
    }
    for (const s of ['normal', 'unversioned', 'ignored', 'needs-update'] as const) {
      expect(isLocalChange(s), s).toBe(false);
    }
  });

  it('冲突属于本地变更但不可提交（必须先解决）', () => {
    expect(isLocalChange('conflicted')).toBe(true);
    expect(isCommittable('conflicted')).toBe(false);
  });
});

describe('isActionable（可勾选）', () => {
  it('未纳管必须可勾选 —— 否则用户永远没法把新文件标记新增（真实 bug 回归）', () => {
    expect(isActionable('unversioned')).toBe(true);
  });

  it('冲突可勾选（要能选中后 revert），但不可提交', () => {
    expect(isActionable('conflicted')).toBe(true);
    expect(isCommittable('conflicted')).toBe(false);
  });

  it('包含全部本地变更', () => {
    for (const s of ['added', 'modified', 'deleted', 'missing'] as const) {
      expect(isActionable(s), s).toBe(true);
    }
  });

  it('normal / ignored 不可勾选（勾了也没动作可做）', () => {
    expect(isActionable('normal')).toBe(false);
    expect(isActionable('ignored')).toBe(false);
  });

  it('可勾选集合严格大于可提交集合（两者不能混用）', () => {
    const all: StatusItem['status'][] = [
      'unversioned',
      'ignored',
      'normal',
      'modified',
      'added',
      'deleted',
      'missing',
      'conflicted',
      'needs-update',
    ];
    const actionable = all.filter(isActionable);
    const committable = all.filter(isCommittable);
    expect(actionable.length).toBeGreaterThan(committable.length);
    for (const s of committable) expect(actionable).toContain(s);
  });
});

describe('summarize', () => {
  it('空列表 → 全 0，文案是"工作副本干净"', () => {
    const s = summarize([]);
    expect(s).toEqual({ total: 0, committable: 0, conflicts: 0, unversioned: 0, ignored: 0 });
    expect(summaryText(s)).toBe('工作副本干净');
  });

  it('分类计数正确，且冲突不计入待提交', () => {
    const s = summarize([
      item('a.psd', 'modified'),
      item('b.psd', 'added'),
      item('c.psd', 'conflicted'),
      item('d.psd', 'unversioned'),
      item('e.psd', 'ignored'),
      item('f.psd', 'normal'),
    ]);
    expect(s.total).toBe(6);
    expect(s.committable).toBe(2);
    expect(s.conflicts).toBe(1);
    expect(s.unversioned).toBe(1);
    expect(s.ignored).toBe(1);
    expect(summaryText(s)).toBe('2 个待提交 · 1 个冲突 · 1 个新增');
  });

  it('没有待办时明确说"干净"，同时带上条目数（只显示"1 个条目"看不出是否正常）', () => {
    expect(summaryText(summarize([item('a', 'normal')]))).toBe('工作副本干净（1 个条目）');
  });
});

describe('sortStatus', () => {
  it('冲突最前，之后按修复优先级，最后是 normal / ignored', () => {
    const sorted = sortStatus([
      item('z', 'ignored'),
      item('a', 'normal'),
      item('c', 'unversioned'),
      item('d', 'modified'),
      item('e', 'conflicted'),
      item('f', 'added'),
    ]);
    expect(sorted.map((s) => s.status)).toEqual([
      'conflicted',
      'modified',
      'added',
      'unversioned',
      'normal',
      'ignored',
    ]);
  });

  it('同档按路径字典序（表格顺序稳定，不随扫描抖动）', () => {
    const sorted = sortStatus([item('b/x', 'modified'), item('a/x', 'modified'), item('c', 'modified')]);
    expect(sorted.map((s) => s.path)).toEqual(['a/x', 'b/x', 'c']);
  });

  it('不修改入参数组', () => {
    const input = [item('b', 'normal'), item('a', 'modified')];
    sortStatus(input);
    expect(input.map((s) => s.path)).toEqual(['b', 'a']);
  });
});

describe('committableItems', () => {
  it('只留可提交项，并已排好序', () => {
    const r = committableItems([
      item('n', 'normal'),
      item('u', 'unversioned'),
      item('c', 'conflicted'),
      item('m', 'modified'),
      item('a', 'added'),
    ]);
    expect(r.map((s) => s.path)).toEqual(['m', 'a']);
  });
});

describe('commitBlockedReason（提交闸门）', () => {
  it('有冲突 → 明确说先解决冲突', () => {
    expect(commitBlockedReason([item('a', 'conflicted'), item('b', 'modified')])).toBe(
      '有 1 个冲突未解决，请先处理再提交',
    );
  });

  it('没有任何变更 → 提示没有可提交的变更', () => {
    expect(commitBlockedReason([item('a', 'normal'), item('b', 'unversioned')])).toBe('没有可提交的变更');
    expect(commitBlockedReason([])).toBe('没有可提交的变更');
  });

  it('可提交 → null（null 才允许点提交）', () => {
    expect(commitBlockedReason([item('a', 'modified')])).toBeNull();
    expect(commitBlockedReason([item('a', 'added'), item('b', 'deleted')])).toBeNull();
  });

  it('冲突优先于"没有变更"（两种问题同时存在时先报冲突）', () => {
    expect(commitBlockedReason([item('a', 'conflicted')])).toContain('冲突');
  });
});
