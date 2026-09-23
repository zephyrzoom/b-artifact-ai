import { describe, expect, it } from 'vitest';

import {
  basename,
  displayPath,
  formatBytes,
  formatRelative,
  formatTime,
  percent,
  progressText,
} from '../../src/renderer/utils/format.js';

describe('formatBytes', () => {
  it('空值显示 -，字节档不保留小数', () => {
    expect(formatBytes(null)).toBe('-');
    expect(formatBytes(undefined)).toBe('-');
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(1023)).toBe('1023 B');
  });

  it('KB 档保留一位小数（1.5 KB 不能被显示成 2 KB）', () => {
    expect(formatBytes(1024)).toBe('1.0 KB');
    expect(formatBytes(1536)).toBe('1.5 KB');
    expect(formatBytes(1024 ** 2 * 1.25)).toBe('1.3 MB');
  });

  it('≥100 时去掉小数，单位到顶后不再进位', () => {
    expect(formatBytes(100 * 1024)).toBe('100 KB');
    expect(formatBytes(1024 ** 6)).toBe('1024 PB');
  });
});

describe('formatTime', () => {
  it('空值与非法值都有兜底', () => {
    expect(formatTime(null)).toBe('-');
    expect(formatTime('')).toBe('-');
    expect(formatTime('nope')).toBe('nope');
  });

  it('本地时区 YYYY-MM-DD HH:mm 且补零', () => {
    expect(formatTime(new Date(2026, 0, 5, 9, 7).toISOString())).toBe('2026-01-05 09:07');
  });
});

describe('formatRelative', () => {
  const now = new Date(2026, 8, 15, 12, 0, 0).getTime();

  it('一分钟内显示"刚刚"', () => {
    expect(formatRelative(now - 30_000, now)).toBe('刚刚');
  });

  it('分钟 / 小时 / 天分级', () => {
    expect(formatRelative(now - 5 * 60_000, now)).toBe('5 分钟前');
    expect(formatRelative(now - 3 * 3600_000, now)).toBe('3 小时前');
    expect(formatRelative(now - 2 * 86400_000, now)).toBe('2 天前');
  });

  it('超过 7 天退化为绝对时间', () => {
    expect(formatRelative(now - 10 * 86400_000, now)).toBe('2026-09-05 12:00');
  });

  it('时钟回拨（未来时间）不产生负数', () => {
    expect(formatRelative(now + 60_000, now)).toBe('刚刚');
  });
});

describe('percent / progressText', () => {
  it('total 为 0 时给 0（不产生 NaN）', () => {
    expect(percent(0, 0)).toBe(0);
    expect(percent(5, Number.NaN)).toBe(0);
  });

  it('夹在 0–100 之间并四舍五入', () => {
    expect(percent(1, 3)).toBe(33);
    expect(percent(3, 3)).toBe(100);
    expect(percent(5, 3)).toBe(100);
    expect(percent(-1, 3)).toBe(0);
  });

  it('文案包含分子分母与百分比', () => {
    expect(progressText(12, 30)).toBe('12 / 30（40%）');
  });
});

describe('displayPath', () => {
  it('空路径显示为仓库根', () => {
    expect(displayPath('')).toBe('/');
    expect(displayPath(null)).toBe('/');
    expect(displayPath('a/b')).toBe('a/b');
  });
});

describe('basename', () => {
  it('取最后一段，空值与根路径显示为 /', () => {
    expect(basename('a/b/c.psd')).toBe('c.psd');
    expect(basename('c.psd')).toBe('c.psd');
    expect(basename('')).toBe('/');
    expect(basename(null)).toBe('/');
    expect(basename('a//b//')).toBe('b');
  });
});
