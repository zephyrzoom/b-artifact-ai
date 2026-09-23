import { createPinia, setActivePinia } from 'pinia'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import { authApi } from '@/api'
import { getToken, setToken } from '@/api/http'
import type { Me } from '@/api/types'
import { useAuthStore } from '@/stores/auth'

vi.mock('@/api', () => ({
  authApi: { me: vi.fn(), login: vi.fn(), logout: vi.fn() },
}))

const me: Me = {
  id: 1,
  username: 'admin',
  display_name: '管理员',
  email: 'a@b.c',
  source: 'local',
  is_admin: true,
}

beforeEach(() => {
  setActivePinia(createPinia())
  localStorage.clear()
  vi.mocked(authApi.me).mockReset()
  vi.mocked(authApi.login).mockReset()
  vi.mocked(authApi.logout).mockReset()
})

describe('load', () => {
  it('没有 token 时不发请求，直接 ready', async () => {
    const s = useAuthStore()
    await s.load()
    expect(s.user).toBeNull()
    expect(s.ready).toBe(true)
    expect(authApi.me).not.toHaveBeenCalled()
  })

  it('有 token 且 /auth/me 成功 → 填充用户', async () => {
    setToken('tok')
    vi.mocked(authApi.me).mockResolvedValue(me)
    const s = useAuthStore()
    await s.load()
    expect(s.user).toEqual(me)
    expect(s.ready).toBe(true)
    expect(getToken()).toBe('tok')
  })

  it('有 token 但 /auth/me 失败 → 清 token、用户置空（不抛给调用方）', async () => {
    setToken('stale')
    vi.mocked(authApi.me).mockRejectedValue(new Error('401'))
    const s = useAuthStore()
    await s.load()
    expect(s.user).toBeNull()
    expect(s.ready).toBe(true)
    expect(getToken()).toBe('')
  })

  it('无论成败 ready 都会置位（否则路由守卫会永久卡在加载态）', async () => {
    setToken('stale')
    vi.mocked(authApi.me).mockRejectedValue(new Error('boom'))
    const s = useAuthStore()
    expect(s.ready).toBe(false)
    await s.load()
    expect(s.ready).toBe(true)
  })
})

describe('login', () => {
  it('成功后把 token 写入 localStorage 并设置用户', async () => {
    vi.mocked(authApi.login).mockResolvedValue({
      token: 'new-token',
      expires_at: '2026-10-15T00:00:00Z',
      user: me,
    })
    const s = useAuthStore()
    await s.login('admin', 'pw')
    expect(authApi.login).toHaveBeenCalledWith('admin', 'pw')
    expect(getToken()).toBe('new-token')
    expect(s.user).toEqual(me)
    expect(s.isAdmin()).toBe(true)
  })

  it('失败时抛给调用方，且不写入 token（登录页要显示错误）', async () => {
    vi.mocked(authApi.login).mockRejectedValue(new Error('bad credentials'))
    const s = useAuthStore()
    await expect(s.login('admin', 'wrong')).rejects.toThrow('bad credentials')
    expect(getToken()).toBe('')
    expect(s.user).toBeNull()
  })
})

describe('logout', () => {
  it('调用 /auth/logout 并清空本地状态', async () => {
    setToken('tok')
    vi.mocked(authApi.logout).mockResolvedValue(undefined)
    const s = useAuthStore()
    await s.logout()
    expect(authApi.logout).toHaveBeenCalled()
    expect(getToken()).toBe('')
    expect(s.user).toBeNull()
  })

  it('令牌已失效导致 logout 报错时仍然清空本地状态', async () => {
    setToken('stale')
    vi.mocked(authApi.logout).mockRejectedValue(new Error('401'))
    const s = useAuthStore()
    await s.logout()
    expect(getToken()).toBe('')
    expect(s.user).toBeNull()
  })
})

describe('isAdmin', () => {
  it('未登录时为 false', () => {
    expect(useAuthStore().isAdmin()).toBe(false)
  })

  it('普通用户为 false', async () => {
    setToken('tok')
    vi.mocked(authApi.me).mockResolvedValue({ ...me, is_admin: false })
    const s = useAuthStore()
    await s.load()
    expect(s.isAdmin()).toBe(false)
  })
})
