import { defineStore } from 'pinia'
import { ref } from 'vue'
import { authApi } from '@/api'
import { clearToken, getToken, setToken } from '@/api/http'
import type { Me } from '@/api/types'

export const useAuthStore = defineStore('auth', () => {
  const user = ref<Me | null>(null)
  const ready = ref(false)

  async function load() {
    if (!getToken()) {
      user.value = null
      ready.value = true
      return
    }
    try {
      user.value = await authApi.me()
    } catch {
      user.value = null
      clearToken()
    }
    ready.value = true
  }

  async function login(username: string, password: string) {
    const r = await authApi.login(username, password)
    setToken(r.token)
    user.value = r.user
  }

  async function logout() {
    try {
      await authApi.logout()
    } catch {
      /* 令牌可能已失效，忽略 */
    }
    clearToken()
    user.value = null
  }

  return { user, ready, load, login, logout, isAdmin: () => !!user.value?.is_admin }
})
