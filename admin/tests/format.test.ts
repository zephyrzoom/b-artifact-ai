import { describe, expect, it } from 'vitest'

import {
  ACTION_LABEL,
  actionLabel,
  basename,
  formatBytes,
  formatPercent,
  formatTime,
  LEVEL_LABEL,
  LEVEL_TAG,
  SUBJECT_LABEL,
} from '@/utils/format'

describe('formatBytes', () => {
  it('空值显示为 -', () => {
    expect(formatBytes(undefined)).toBe('-')
    expect(formatBytes(null)).toBe('-')
  })

  it('小于 1KB 用字节，不补小数', () => {
    expect(formatBytes(0)).toBe('0 B')
    expect(formatBytes(1)).toBe('1 B')
    expect(formatBytes(1023)).toBe('1023 B')
  })

  it('按 1024 进制逐级进位', () => {
    expect(formatBytes(1024)).toBe('1.0 KB')
    expect(formatBytes(1536)).toBe('1.5 KB')
    expect(formatBytes(1024 * 1024)).toBe('1.0 MB')
    expect(formatBytes(1024 ** 3)).toBe('1.0 GB')
    expect(formatBytes(1024 ** 4)).toBe('1.0 TB')
    expect(formatBytes(1024 ** 5)).toBe('1.0 PB')
  })

  it('超过 PB 不再进位（单位到顶）', () => {
    expect(formatBytes(1024 ** 6)).toBe('1024 PB')
  })

  it('值 ≥100 时不保留小数，避免表格里过长', () => {
    expect(formatBytes(100 * 1024)).toBe('100 KB')
    expect(formatBytes(999 * 1024)).toBe('999 KB')
    expect(formatBytes(101.5 * 1024)).toBe('102 KB')
  })

  it('KB 档也保留一位小数（回归：曾因 i<=0 把 1.5 KB 显示成 2 KB）', () => {
    expect(formatBytes(1536)).toBe('1.5 KB')
    expect(formatBytes(1024 * 1.04)).toBe('1.0 KB')
    expect(formatBytes(1024 * 12.34)).toBe('12.3 KB')
  })
})

describe('formatPercent', () => {
  it('乘 100 并保留指定小数位', () => {
    expect(formatPercent(0)).toBe('0.0%')
    expect(formatPercent(0.1234)).toBe('12.3%')
    expect(formatPercent(1)).toBe('100.0%')
  })

  it('小数位可指定，0 位时四舍五入', () => {
    expect(formatPercent(0.125, 2)).toBe('12.50%')
    expect(formatPercent(0.126, 0)).toBe('13%')
  })
})

describe('formatTime', () => {
  it('空值显示为 -', () => {
    expect(formatTime(null)).toBe('-')
    expect(formatTime(undefined)).toBe('-')
    expect(formatTime('')).toBe('-')
  })

  it('非法时间原样返回，不显示 Invalid Date', () => {
    expect(formatTime('not-a-time')).toBe('not-a-time')
  })

  it('格式化为本地时区的 YYYY-MM-DD HH:mm，月日时分补零', () => {
    // 用本地时区构造，断言与运行机器时区无关
    const d = new Date(2026, 0, 5, 9, 7)
    expect(formatTime(d.toISOString())).toBe('2026-01-05 09:07')
  })

  it('跨年与 24 小时制边界', () => {
    expect(formatTime(new Date(2026, 11, 31, 23, 59).toISOString())).toBe('2026-12-31 23:59')
    expect(formatTime(new Date(2027, 0, 1, 0, 0).toISOString())).toBe('2027-01-01 00:00')
  })
})

describe('actionLabel', () => {
  it('已知动作给中文标签', () => {
    expect(actionLabel('user.login')).toBe('登录')
    expect(actionLabel('lock.break')).toBe('强制解锁')
    expect(actionLabel('maintenance.rebuild_refcount')).toBe('重建引用计数')
  })

  it('未知动作原样返回（新增审计动作不会被静默吞掉）', () => {
    expect(actionLabel('user.something_new')).toBe('user.something_new')
  })

  it('标签表覆盖服务端审计动作全集（与 §7.2 的 action 枚举对齐）', () => {
    // 少一条就会在审计页显示成裸英文，这里钉住关键几类
    for (const a of [
      'user.login',
      'user.create',
      'user.create_admin',
      'user.update',
      'user.delete',
      'user.password_reset',
      'group.create',
      'group.members',
      'repo.create',
      'repo.purge',
      'repo.settings',
      'acl.set',
      'acl.update',
      'acl.delete',
      'lock.acquire',
      'lock.release',
      'lock.break',
      'maintenance.gc',
    ]) {
      expect(ACTION_LABEL[a], `${a} 缺少中文标签`).toBeTruthy()
    }
  })
})

describe('basename', () => {
  it('取最后一段', () => {
    expect(basename('src/assets/logo.png')).toBe('logo.png')
    expect(basename('logo.png')).toBe('logo.png')
  })

  it('空值与根路径都显示为 /', () => {
    expect(basename('')).toBe('/')
    expect(basename('/')).toBe('/')
  })

  it('多余斜杠不影响结果', () => {
    expect(basename('a//b//')).toBe('b')
  })
})

describe('级别与主体标签表', () => {
  it('四个级别都有标签与 tag 颜色', () => {
    for (const lv of ['none', 'read', 'write', 'admin']) {
      expect(LEVEL_LABEL[lv]).toBeTruthy()
      expect(LEVEL_TAG[lv]).toBeTruthy()
    }
    expect(LEVEL_LABEL.none).toBe('无')
    expect(LEVEL_LABEL.write).toBe('读写')
  })

  it('三种主体都有标签', () => {
    expect(SUBJECT_LABEL.everyone).toBe('所有人')
    expect(SUBJECT_LABEL.group).toBe('用户组')
    expect(SUBJECT_LABEL.user).toBe('用户')
  })
})
