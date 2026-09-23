/**
 * 口令强度策略的前端镜像（§9.2 v0.4.17）。
 *
 * 这张用例表与服务端 `auth::password::tests` **刻意写成同一组边界**：
 * 两边规则一旦漂移，前端就会变成"提示通过、服务端拒绝"的误导源。
 */

import { describe, expect, it } from 'vitest'

import { MAX_LEN, MIN_LEN, passwordPolicyError } from '../src/utils/password'

describe('passwordPolicyError', () => {
  it('四类齐备且够长 → 通过', () => {
    for (const ok of ['Abcdefg1!', 'P@ssw0rd', 'aB3!aB3!', 'Aa1!aaaa']) {
      expect(passwordPolicyError(ok), ok).toBeNull()
    }
  })

  it('长度边界：7 拒 / 8 过 / 超 256 拒', () => {
    expect(passwordPolicyError('Ab1!abc')).toContain(String(MIN_LEN))
    expect(passwordPolicyError('Ab1!abcd')).toBeNull()
    const long = 'aB1!' + 'a'.repeat(MAX_LEN)
    expect(passwordPolicyError(long)).toContain('过长')
  })

  it('四类缺一都要点名缺的是哪一类', () => {
    expect(passwordPolicyError('abcdefg1')).toContain('大写字母')
    expect(passwordPolicyError('ABCDEFG1')).toContain('小写字母')
    expect(passwordPolicyError('Abcdefg!')).toContain('数字')
    expect(passwordPolicyError('Abcdefg1')).toContain('符号')
  })

  it('**按字符计数**：5 个汉字是 5 个字符（10 字节），不能因为字节够就放行', () => {
    expect(passwordPolicyError('密码密码密')).toContain('当前 5 个')
    expect(passwordPolicyError('密码密码密码Ab1')).toBeNull()
  })
})
