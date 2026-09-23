import { describe, expect, it } from 'vitest'

import type { AclPreview, AclRule, TraceStep } from '@/api/types'
import {
  aclFormFromRule,
  createAclPayload,
  emptyAclForm,
  groupListText,
  LEVELS,
  outcomeLabel,
  pickedRuleId,
  previewSummary,
  ruleDeletePrompt,
  shadowedRuleCount,
  stepStatusFor,
  updateAclPayload,
  validateAclForm,
} from '@/utils/acl'

function rule(over: Partial<AclRule> = {}): AclRule {
  return {
    id: 1,
    path_prefix: 'src',
    subject_type: 'everyone',
    subject_id: 0,
    level: 'read',
    inherit: true,
    ...over,
  }
}

describe('LEVELS 顺序（§4.1 从低到高）', () => {
  it('依次为 none → read → write → admin，下拉与"谁有权限"都依赖这个顺序', () => {
    expect(LEVELS).toEqual(['none', 'read', 'write', 'admin'])
  })
})

describe('emptyAclForm / aclFormFromRule', () => {
  it('新建默认：仓库根、所有人、只读、允许继承', () => {
    expect(emptyAclForm()).toEqual({
      path_prefix: '',
      subject_type: 'everyone',
      level: 'read',
      inherit: true,
    })
  })

  it('每次调用都是新对象（避免多个对话框共享同一份状态）', () => {
    const a = emptyAclForm()
    a.level = 'admin'
    expect(emptyAclForm().level).toBe('read')
  })

  it('编辑既有规则时字段原样搬过来，含 id 与 subject_id', () => {
    const row = rule({ id: 9, path_prefix: 'vault', subject_type: 'group', subject_id: 3, level: 'write', inherit: false })
    expect(aclFormFromRule(row)).toEqual({
      id: 9,
      path_prefix: 'vault',
      subject_type: 'group',
      subject_id: 3,
      level: 'write',
      inherit: false,
    })
  })
})

describe('validateAclForm', () => {
  it('仓库根（path_prefix === ""）是合法取值，不能当成"没填"', () => {
    expect(validateAclForm({ ...emptyAclForm(), path_prefix: '' }, false)).toBeNull()
  })

  it('新增时 path_prefix 不是字符串才报错', () => {
    const form = emptyAclForm()
    ;(form as { path_prefix: unknown }).path_prefix = undefined
    expect(validateAclForm(form, false)).toBe('请填写目录前缀')
  })

  it('编辑时不再校验路径（路径不可改）', () => {
    const form = { ...emptyAclForm(), id: 5 }
    ;(form as { path_prefix: unknown }).path_prefix = undefined
    expect(validateAclForm(form, true)).toBeNull()
  })

  it('非「所有人」必须选主体', () => {
    const base = emptyAclForm()
    expect(validateAclForm({ ...base, subject_type: 'user' }, false)).toBe('请选择主体')
    expect(validateAclForm({ ...base, subject_type: 'group' }, false)).toBe('请选择主体')
    expect(validateAclForm({ ...base, subject_type: 'user', subject_id: 7 }, false)).toBeNull()
  })

  it('「所有人」不需要主体（subject_id 为 0）', () => {
    expect(validateAclForm({ ...emptyAclForm(), subject_id: 0 }, false)).toBeNull()
  })
})

describe('请求体组装', () => {
  it('新增：subject_id 缺省补 0（服务端 everyone 用 0）', () => {
    expect(createAclPayload({ ...emptyAclForm(), path_prefix: 'a/b', level: 'write', inherit: false })).toEqual({
      path_prefix: 'a/b',
      subject_type: 'everyone',
      subject_id: 0,
      level: 'write',
      inherit: false,
    })
  })

  it('新增：带主体时原样送 subject_id', () => {
    expect(
      createAclPayload({ ...emptyAclForm(), subject_type: 'group', subject_id: 12 }).subject_id,
    ).toBe(12)
  })

  it('编辑：只送可改字段（id / level / inherit），不送路径与主体', () => {
    const payload = updateAclPayload({ ...emptyAclForm(), id: 42, level: 'none', inherit: false })
    expect(payload).toEqual({ id: 42, level: 'none', inherit: false })
    expect(Object.keys(payload).sort()).toEqual(['id', 'inherit', 'level'])
  })
})

describe('文案映射', () => {
  it('删除确认文案带 id / 路径 / 主体，根路径显示为 /', () => {
    expect(ruleDeletePrompt(rule({ id: 3, path_prefix: 'vault', subject_label: 'artists' }))).toBe(
      '删除规则 #3（vault · artists）？',
    )
    expect(ruleDeletePrompt(rule({ id: 4, path_prefix: '', subject_label: '所有人' }))).toBe(
      '删除规则 #4（/ · 所有人）？',
    )
  })

  it('subject_label 缺失时不显示 undefined', () => {
    expect(ruleDeletePrompt(rule({ id: 5, subject_label: undefined }))).not.toContain('undefined')
  })

  it('outcome 四种取值都翻译，未知取值原样返回', () => {
    expect(outcomeLabel('hit')).toBe('命中')
    expect(outcomeLabel('barrier')).toBe('屏障截断')
    expect(outcomeLabel('miss')).toBe('未命中')
    expect(outcomeLabel('skip')).toBe('无规则')
    expect(outcomeLabel('brand_new')).toBe('brand_new')
  })

  it('步骤状态：命中=success、屏障=error、其余=wait', () => {
    expect(stepStatusFor('hit')).toBe('success')
    expect(stepStatusFor('barrier')).toBe('error')
    expect(stepStatusFor('miss')).toBe('wait')
    expect(stepStatusFor('skip')).toBe('wait')
  })

  it('所属组文案：空数组显示为（无）', () => {
    expect(groupListText(['artists', 'reviewers'])).toBe('artists、reviewers')
    expect(groupListText([])).toBe('（无）')
  })
})

describe('shadowedRuleCount', () => {
  it('只统计被压过的规则', () => {
    expect(
      shadowedRuleCount([
        rule({ id: 1, shadowed: true }),
        rule({ id: 2, shadowed: false }),
        rule({ id: 3, shadowed: true }),
        rule({ id: 4 }),
      ]),
    ).toBe(2)
  })

  it('空列表为 0，且不因 shadowed 未定义而算错', () => {
    expect(shadowedRuleCount([])).toBe(0)
    expect(shadowedRuleCount([rule()])).toBe(0)
  })
})

describe('previewSummary（§8.3）', () => {
  function preview(over: Partial<AclPreview> = {}): AclPreview {
    return {
      path: 'vault/a.bin',
      level: 'read',
      reason: '命中 vault 的规则 #2',
      steps: [],
      user: { id: 1, username: 'alice', is_admin: false, groups: [] },
      ...over,
    }
  }

  it('非 none 级别用成功图标，标题带中文标签与原始枚举', () => {
    expect(previewSummary(preview({ level: 'write' }))).toEqual({
      icon: 'success',
      title: '最终权限：读写（write）',
      subTitle: '命中 vault 的规则 #2',
    })
  })

  it('none 用错误图标（界面上需要一眼看出"无权限"）', () => {
    const s = previewSummary(preview({ level: 'none', reason: '屏障截断于 vault' }))
    expect(s.icon).toBe('error')
    expect(s.title).toBe('最终权限：无（none）')
    expect(s.subTitle).toBe('屏障截断于 vault')
  })

  it('admin 也用成功图标', () => {
    expect(previewSummary(preview({ level: 'admin' })).icon).toBe('success')
  })
})

describe('pickedRuleId', () => {
  function step(over: Partial<TraceStep> = {}): TraceStep {
    return { prefix: 'vault', display: 'vault', outcome: 'hit', level: 'read', reason: '', ...over }
  }

  it('命中时返回被选中的 rule_id（供高亮）', () => {
    expect(pickedRuleId(step({ outcome: 'hit', rule_id: 8 }))).toBe(8)
  })

  it('屏障 / 未命中 / 无规则时不高亮任何规则，即使带了 rule_id', () => {
    expect(pickedRuleId(step({ outcome: 'barrier', rule_id: 8 }))).toBeUndefined()
    expect(pickedRuleId(step({ outcome: 'miss', rule_id: 8 }))).toBeUndefined()
    expect(pickedRuleId(step({ outcome: 'skip' }))).toBeUndefined()
  })
})
