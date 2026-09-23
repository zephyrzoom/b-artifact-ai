import { describe, expect, it } from 'vitest';

import type { LogEntry } from '../../src/shared/dto.js';
import {
  deltaColor,
  formatByteDelta,
  groupByDay,
  LOG_PAGE_SIZE,
  nextOffset,
  revisionLabel,
  revisionMessage,
  revisionSubtitle,
  summarizeRevisions,
} from '../../src/renderer/utils/history.js';

function rev(over: Partial<LogEntry> = {}): LogEntry {
  return {
    rev: 1,
    author: 'alice',
    message: '提交',
    created_at: '2026-09-15T03:00:00Z',
    file_count: 1,
    byte_delta: 10,
    manifest_hash: 'm',
    ...over,
  };
}

describe('formatByteDelta', () => {
  it('正增量带 +，负增量带 -', () => {
    expect(formatByteDelta(0)).toBe('±0');
    expect(formatByteDelta(512)).toBe('+512 B');
    expect(formatByteDelta(-512)).toBe('-512 B');
    expect(formatByteDelta(2048)).toBe('+2.0 KB');
  });
});

describe('deltaColor（涨红跌绿，与国内行情一致）', () => {
  it('增加用红、减少用绿、零用灰', () => {
    expect(deltaColor(1)).toBe('#f56c6c');
    expect(deltaColor(-1)).toBe('#67c23a');
    expect(deltaColor(0)).toBe('var(--ba-muted)');
  });
});

describe('groupByDay', () => {
  it('按本地日期分组，同一天聚在一起', () => {
    const groups = groupByDay([
      rev({ rev: 3, created_at: new Date(2026, 8, 15, 12, 0).toISOString() }),
      rev({ rev: 2, created_at: new Date(2026, 8, 15, 9, 0).toISOString() }),
      rev({ rev: 1, created_at: new Date(2026, 8, 14, 9, 0).toISOString() }),
    ]);
    expect(groups.map((g) => g.day)).toEqual(['2026-09-15', '2026-09-14']);
    expect(groups[0]!.items.map((i) => i.rev)).toEqual([3, 2]);
  });

  it('保持输入顺序（新的在前），不重排', () => {
    const groups = groupByDay([rev({ rev: 5 }), rev({ rev: 4 }), rev({ rev: 6 })]);
    expect(groups).toHaveLength(1);
    expect(groups[0]!.items.map((i) => i.rev)).toEqual([5, 4, 6]);
  });

  it('空输入给空数组', () => {
    expect(groupByDay([])).toEqual([]);
  });
});

describe('summarizeRevisions', () => {
  it('分别累计增减字节，并去重作者', () => {
    const s = summarizeRevisions([
      rev({ rev: 3, author: 'alice', byte_delta: 100 }),
      rev({ rev: 2, author: 'bob', byte_delta: -30 }),
      rev({ rev: 1, author: 'alice', byte_delta: 0 }),
    ]);
    expect(s.count).toBe(3);
    expect(s.added).toBe(100);
    expect(s.removed).toBe(30);
    expect(s.authors).toEqual(['alice', 'bob']);
  });

  it('作者缺失时不产生空字符串项', () => {
    expect(summarizeRevisions([rev({ author: '' })]).authors).toEqual([]);
  });
});

describe('revisionLabel', () => {
  it('0 显示为占位（没有 r0 这个提交）', () => {
    expect(revisionLabel(0)).toBe('—');
    expect(revisionLabel(7)).toBe('r7');
  });
});

describe('nextOffset（服务端只回当页条数）', () => {
  it('满页 → 继续，offset 前进一页', () => {
    expect(nextOffset(LOG_PAGE_SIZE, 0)).toBe(LOG_PAGE_SIZE);
    expect(nextOffset(LOG_PAGE_SIZE, 50)).toBe(100);
  });

  it('不满页 → 没有下一页（null）', () => {
    expect(nextOffset(3, 0)).toBeNull();
    expect(nextOffset(0, 0)).toBeNull();
  });

  it('页大小可显式指定', () => {
    expect(nextOffset(10, 20, 10)).toBe(30);
    expect(nextOffset(9, 20, 10)).toBeNull();
  });
});

describe('revisionSubtitle / revisionMessage', () => {
  it('副标题是 作者 · 文件数 · 字节增量', () => {
    expect(revisionSubtitle(rev({ author: 'alice', file_count: 3, byte_delta: 2048 }))).toBe(
      'alice · 3 个文件 · +2.0 KB',
    );
  });

  it('作者缺失时给占位而不是空白', () => {
    expect(revisionSubtitle(rev({ author: '' }))).toContain('（未知）');
  });

  it('说明为空时给占位（区分"没写说明"与"没加载出来"）', () => {
    expect(revisionMessage(rev({ message: '   ' }))).toBe('（无说明）');
    expect(revisionMessage(rev({ message: '改贴图' }))).toBe('改贴图');
  });
});
