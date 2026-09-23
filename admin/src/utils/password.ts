/**
 * 口令强度策略的前端镜像（v0.4.17，方案 §9.2）。
 *
 * 规则与服务端 `server/src/auth/password.rs::validate_policy()` **必须一致**：
 * 长度按**字符** ≥8 且 ≤256，且数字 / 大写字母 / 小写字母 / 符号四类齐备。
 *
 * 为什么在前端也做一遍：服务端一定会校验（那是权威），但让用户在提交前就看见
 * "还缺什么"，比提交后被 400 弹回来友好得多。两边规则一旦漂移，前端就会变成
 * "提示通过但服务端拒绝"的误导源 —— 所以这里只允许是一份**逐字对应**的镜像，
 * 单测里也用同一张用例表钉住。
 */

export const MIN_LEN = 8
export const MAX_LEN = 256

/** 返回 null 表示通过；否则返回人话原因（可直接展示）。 */
export function passwordPolicyError(password: string): string | null {
  const n = [...password].length
  if (n < MIN_LEN) return `密码至少 ${MIN_LEN} 个字符（当前 ${n} 个）`
  if (n > MAX_LEN) return `密码过长（${n} 字符，上限 ${MAX_LEN}）`

  let hasDigit = false
  let hasUpper = false
  let hasLower = false
  let hasSymbol = false
  for (const c of password) {
    if (/[0-9]/.test(c)) hasDigit = true
    else if (/[A-Z]/.test(c)) hasUpper = true
    else if (/[a-z]/.test(c)) hasLower = true
    // 剩下的都算符号：ASCII 标点 + 非 ASCII（中文等）
    else hasSymbol = true
  }

  const missing: string[] = []
  if (!hasDigit) missing.push('数字')
  if (!hasUpper) missing.push('大写字母')
  if (!hasLower) missing.push('小写字母')
  if (!hasSymbol) missing.push('符号')
  if (missing.length > 0) {
    return `密码必须同时包含数字、大写字母、小写字母、符号（缺少：${missing.join('、')}）`
  }
  return null
}

/** 规则的一句话说明，用于表单 placeholder / 提示。 */
export const PASSWORD_HINT = '≥8 位，且同时包含数字、大写字母、小写字母、符号'
