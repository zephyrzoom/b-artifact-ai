import { describe, expect, it } from 'vitest'

import {
  canPurge,
  emptyPurgeForm,
  PURGE_REASON_MIN,
  purgePayload,
  revRangeText,
} from '@/utils/purge'

const REPO = 'art-assets'

function form(over: Partial<ReturnType<typeof emptyPurgeForm>> = {}) {
  return { prefix: 'secret', reason: '合规要求', confirm_name: REPO, ...over }
}

describe('emptyPurgeForm', () => {
  it('原因与确认名清空，前缀可预填', () => {
    expect(emptyPurgeForm()).toEqual({ prefix: '', reason: '', confirm_name: '' })
    expect(emptyPurgeForm('secret/').prefix).toBe('secret/')
  })
})

describe('canPurge（不可恢复操作的三道闸）', () => {
  it('三个条件齐备才允许提交', () => {
    expect(canPurge(form(), REPO)).toBe(true)
  })

  it('目录前缀为空即拒绝（空前缀会清掉整个仓库）', () => {
    expect(canPurge(form({ prefix: '' }), REPO)).toBe(false)
    expect(canPurge(form({ prefix: '   ' }), REPO)).toBe(false)
  })

  it(`原因不足 ${PURGE_REASON_MIN} 字即拒绝（原因会原样进审计）`, () => {
    expect(canPurge(form({ reason: '误删' }), REPO)).toBe(false) // 2 字
    expect(canPurge(form({ reason: '误删补' }), REPO)).toBe(false) // 3 字（边界下）
    expect(canPurge(form({ reason: '误删补回' }), REPO)).toBe(true) // 4 字（边界）
    expect(canPurge(form({ reason: ' 误删 ' }), REPO)).toBe(false) // 去空白后不足
  })

  it('确认名逐字符相等才放行，不做 trim / 忽略大小写', () => {
    expect(canPurge(form({ confirm_name: ' art-assets' }), REPO)).toBe(false)
    expect(canPurge(form({ confirm_name: 'ART-ASSETS' }), REPO)).toBe(false)
    expect(canPurge(form({ confirm_name: 'art-asset' }), REPO)).toBe(false)
    expect(canPurge(form({ confirm_name: '' }), REPO)).toBe(false)
  })

  it('仓库名本身含空格时也按逐字符比较', () => {
    expect(canPurge(form({ confirm_name: 'my repo' }), 'my repo')).toBe(true)
  })
})

describe('purgePayload', () => {
  it('prefix / reason 去首尾空白，confirm_name 原样送出由服务端复核', () => {
    expect(purgePayload({ prefix: ' secret/ ', reason: ' 合规要求 ', confirm_name: REPO })).toEqual({
      prefix: 'secret/',
      reason: '合规要求',
      confirm_name: REPO,
    })
  })
})

describe('revRangeText', () => {
  it('有区间时显示 rN – rM', () => {
    expect(revRangeText({ from: 1, to: 20 })).toBe('r1 – r20')
  })

  it('没有受影响修订时显示 —', () => {
    expect(revRangeText(null)).toBe('—')
  })
})
