// 权限矩阵（ACL）的纯逻辑：判定、文案映射、表单校验、请求体组装。
//
// 组件只负责渲染与调用，这里全部是纯函数，便于单测（§12.4：前端逻辑抽成
// composable / 纯函数测）。规则语义见方案 §4.2，预览器语义见 §8.3。

import type { AclPreview, AclRule, Level, SubjectType, TraceStep } from '@/api/types'
import { LEVEL_LABEL } from './format'

/** 级别从低到高，与 §4.1 的枚举一致。 */
export const LEVELS: Level[] = ['none', 'read', 'write', 'admin']

export interface AclForm {
  id?: number
  path_prefix: string
  subject_type: SubjectType
  subject_id?: number
  level: Level
  inherit: boolean
}

export function emptyAclForm(): AclForm {
  return { path_prefix: '', subject_type: 'everyone', level: 'read', inherit: true }
}

/** 编辑既有规则：主体与路径都不可改（改主体等于换一条规则，改路径等于换一层）。 */
export function aclFormFromRule(row: AclRule): AclForm {
  return {
    id: row.id,
    path_prefix: row.path_prefix,
    subject_type: row.subject_type,
    subject_id: row.subject_id,
    level: row.level,
    inherit: row.inherit,
  }
}

/**
 * 表单校验：通过返回 null，否则返回提示文案。
 *
 * 注意 `path_prefix === ''` 是**合法**的——它表示仓库根（界面上显示为 `/`），
 * 所以不能按"空字符串即未填"来处理。
 */
export function validateAclForm(form: AclForm, editing: boolean): string | null {
  if (!editing && typeof form.path_prefix !== 'string') return '请填写目录前缀'
  if (form.subject_type !== 'everyone' && !form.subject_id) return '请选择主体'
  return null
}

/** 新增请求体（编辑走 updateAcl，只送 level / inherit）。 */
export function createAclPayload(form: AclForm) {
  return {
    path_prefix: form.path_prefix,
    subject_type: form.subject_type,
    subject_id: form.subject_id ?? 0,
    level: form.level,
    inherit: form.inherit,
  }
}

/** 编辑请求体：只带可改字段。 */
export function updateAclPayload(form: AclForm) {
  return { id: form.id as number, level: form.level, inherit: form.inherit }
}

export function ruleDeletePrompt(row: AclRule): string {
  return `删除规则 #${row.id}（${row.path_prefix || '/'} · ${row.subject_label ?? ''}）？`
}

/** 被同层更具体规则压过、永不生效的规则条数（顶部那条警示 tag）。 */
export function shadowedRuleCount(rules: readonly AclRule[]): number {
  return rules.filter((r) => r.shadowed).length
}

const OUTCOME_LABEL: Record<string, string> = {
  hit: '命中',
  barrier: '屏障截断',
  miss: '未命中',
  skip: '无规则',
}

/** 回溯步骤的 outcome → 中文说明（未知取值原样返回，避免静默丢信息）。 */
export function outcomeLabel(o: string): string {
  return OUTCOME_LABEL[o] ?? o
}

/** 回溯步骤的 outcome → el-step 的 status。屏障单独标红，是排障时最需要一眼看到的。 */
export function stepStatusFor(outcome: string): 'success' | 'error' | 'wait' {
  if (outcome === 'hit') return 'success'
  if (outcome === 'barrier') return 'error'
  return 'wait'
}

export interface PreviewSummary {
  icon: 'success' | 'error'
  title: string
  subTitle: string
}

/** 预览器顶部结论：`none` 用错误图标，其余用成功（§8.3）。 */
export function previewSummary(preview: AclPreview): PreviewSummary {
  return {
    icon: preview.level === 'none' ? 'error' : 'success',
    title: `最终权限：${LEVEL_LABEL[preview.level]}（${preview.level}）`,
    subTitle: preview.reason,
  }
}

/** 预览器的"所属组"文案。 */
export function groupListText(groups: readonly string[]): string {
  return groups.join('、') || '（无）'
}

/**
 * 回溯链上某一步的候选规则里，哪一条是最终被 `pick` 选中的。
 * 用于高亮（选中深色 tag、其余浅色）。
 */
export function pickedRuleId(step: TraceStep): number | undefined {
  return step.outcome === 'hit' ? step.rule_id : undefined
}
