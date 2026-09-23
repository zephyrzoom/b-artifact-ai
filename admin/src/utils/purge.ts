// purge 表单的纯逻辑（§3.6 / §8.2）。
//
// purge 是**不可恢复**的 L2 删除，所以提交条件必须写在一个地方、被单测钉住：
// 目录前缀非空、原因 ≥4 字（会原样进审计）、以及输入仓库名逐字符相等（防误触）。

export const PURGE_REASON_MIN = 4

export interface PurgeForm {
  prefix: string
  reason: string
  confirm_name: string
}

export function emptyPurgeForm(prefix = ''): PurgeForm {
  return { prefix, reason: '', confirm_name: '' }
}

/**
 * 是否允许提交。
 *
 * `confirm_name` 用**逐字符相等**判定，不做 trim / 忽略大小写——
 * 这个字段的全部意义就是拦住手滑，放宽等于没写。
 */
export function canPurge(form: PurgeForm, repo: string): boolean {
  return (
    form.prefix.trim().length > 0 &&
    form.reason.trim().length >= PURGE_REASON_MIN &&
    form.confirm_name === repo
  )
}

/** 提交体：prefix / reason 去首尾空白，confirm_name 原样送出由服务端复核。 */
export function purgePayload(form: PurgeForm): PurgeForm {
  return {
    prefix: form.prefix.trim(),
    reason: form.reason.trim(),
    confirm_name: form.confirm_name,
  }
}

/** 修订区间文案：`r1 – r20` / `—`。 */
export function revRangeText(range: { from: number; to: number } | null): string {
  return range ? `r${range.from} – r${range.to}` : '—'
}
