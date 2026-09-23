<template>
  <div class="login-wrap">
    <el-card class="login-card">
      <div class="head">
        <span class="logo">b</span>
        <div>
          <h2>b-artifact</h2>
          <p class="muted">集中式二进制资产版本管理</p>
        </div>
      </div>

      <el-form :model="form" size="large" @keyup.enter="submit">
        <el-form-item>
          <el-input v-model="form.username" placeholder="用户名" autofocus>
            <template #prefix><el-icon><User /></el-icon></template>
          </el-input>
        </el-form-item>
        <el-form-item>
          <el-input v-model="form.password" type="password" placeholder="密码" show-password>
            <template #prefix><el-icon><Lock /></el-icon></template>
          </el-input>
        </el-form-item>
        <el-button type="primary" size="large" style="width: 100%" :loading="loading" @click="submit">
          登录
        </el-button>
      </el-form>

      <el-alert
        v-if="providers.ldap && !providers.local"
        type="info"
        :closable="false"
        style="margin-top: 14px"
        title="本系统使用 LDAP 认证，请输入目录服务账号"
      />
      <p class="muted hint">
        首次登录：users 表为空时，第一个成功登录的用户自动成为系统管理员（§9.1）。
      </p>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { useAuthStore } from '@/stores/auth'
import { authApi } from '@/api'
import { ApiError } from '@/api/http'
import type { Providers } from '@/api/types'

const auth = useAuthStore()
const router = useRouter()
const route = useRoute()

const form = reactive({ username: '', password: '' })
const loading = ref(false)
const providers = ref<Providers>({ local: true, ldap: false })

onMounted(async () => {
  try {
    providers.value = await authApi.providers()
  } catch {
    /* 忽略 */
  }
})

async function submit() {
  if (!form.username || !form.password) {
    ElMessage.warning('请输入用户名和密码')
    return
  }
  loading.value = true
  try {
    await auth.login(form.username, form.password)
    const redirect = (route.query.redirect as string) || '/'
    router.replace(redirect)
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '登录失败')
  } finally {
    loading.value = false
  }
}
</script>

<style scoped>
.login-wrap {
  height: 100%;
  display: grid;
  place-items: center;
  background: radial-gradient(1000px 500px at 50% -10%, #2f6fed22, transparent);
}
.login-card {
  width: 380px;
  padding: 8px 8px 4px;
}
.head {
  display: flex;
  align-items: center;
  gap: 12px;
  margin-bottom: 18px;
}
.head h2 {
  margin: 0;
  font-size: 19px;
}
.head p {
  margin: 2px 0 0;
  font-size: 12px;
}
.logo {
  width: 38px;
  height: 38px;
  border-radius: 10px;
  background: #2f6fed;
  color: #fff;
  display: grid;
  place-items: center;
  font-weight: 700;
  font-size: 19px;
}
.hint {
  font-size: 12px;
  margin: 14px 0 0;
  line-height: 1.6;
}
</style>
