<template>
  <el-container style="height: 100%">
    <el-aside width="208px" class="aside">
      <div class="brand">
        <span class="logo">b</span>
        <div>
          <div class="brand-name">b-artifact</div>
          <div class="brand-sub">资产管理</div>
        </div>
      </div>
      <el-menu :default-active="active" router class="menu">
        <el-menu-item index="/">
          <el-icon><DataLine /></el-icon><span>概览</span>
        </el-menu-item>
        <el-menu-item index="/repos">
          <el-icon><Folder /></el-icon><span>仓库</span>
        </el-menu-item>
        <el-menu-item index="/users">
          <el-icon><User /></el-icon><span>用户</span>
        </el-menu-item>
        <el-menu-item index="/groups">
          <el-icon><UserFilled /></el-icon><span>用户组</span>
        </el-menu-item>
        <el-menu-item index="/audit">
          <el-icon><Document /></el-icon><span>审计日志</span>
        </el-menu-item>
        <el-menu-item index="/settings">
          <el-icon><Setting /></el-icon><span>系统</span>
        </el-menu-item>
      </el-menu>
      <div class="aside-foot">
        <el-button text size="small" @click="toggleDark">
          <el-icon><component :is="isDark ? 'Sunny' : 'Moon'" /></el-icon>
          {{ isDark ? '浅色' : '深色' }}
        </el-button>
      </div>
    </el-aside>

    <el-container>
      <el-header class="header">
        <div class="muted" style="font-size: 13px">
          {{ serverVersion ? `服务端 v${serverVersion}` : '' }}
        </div>
        <div class="spacer" />
        <el-dropdown trigger="click" @command="onCommand">
          <span class="user-chip">
            <el-avatar :size="26">{{ initial }}</el-avatar>
            <span>{{ auth.user?.username }}</span>
            <el-tag v-if="auth.user?.is_admin" type="danger" size="small" effect="plain">
              管理员
            </el-tag>
            <el-icon><ArrowDown /></el-icon>
          </span>
          <template #dropdown>
            <el-dropdown-menu>
              <el-dropdown-item command="password">修改密码</el-dropdown-item>
              <el-dropdown-item command="logout" divided>退出登录</el-dropdown-item>
            </el-dropdown-menu>
          </template>
        </el-dropdown>
      </el-header>

      <el-main style="padding: 0; overflow: auto">
        <router-view />
      </el-main>
    </el-container>

    <el-dialog v-model="pwVisible" title="修改密码" width="420px">
      <el-form :model="pwForm" label-width="90px">
        <el-form-item label="当前密码">
          <el-input v-model="pwForm.old_password" type="password" show-password />
        </el-form-item>
        <el-form-item label="新密码">
          <el-input
            v-model="pwForm.new_password"
            type="password"
            show-password
            :placeholder="PASSWORD_HINT"
          />
        </el-form-item>
        <el-form-item label="确认新密码">
          <el-input v-model="pwForm.confirm" type="password" show-password />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="pwVisible = false">取消</el-button>
        <el-button type="primary" :loading="pwLoading" @click="submitPassword">确定</el-button>
      </template>
    </el-dialog>
  </el-container>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import { useAuthStore } from '@/stores/auth'
import { authApi, adminApi } from '@/api'
import { ApiError } from '@/api/http'
import { PASSWORD_HINT, passwordPolicyError } from '@/utils/password'

const auth = useAuthStore()
const route = useRoute()
const router = useRouter()

const active = computed(() => {
  const p = route.path
  if (p.startsWith('/repos')) return '/repos'
  if (p.startsWith('/users')) return '/users'
  if (p.startsWith('/groups')) return '/groups'
  if (p.startsWith('/audit')) return '/audit'
  if (p.startsWith('/settings')) return '/settings'
  return '/'
})

const initial = computed(() => (auth.user?.username ?? '?').slice(0, 1).toUpperCase())

const isDark = ref(document.documentElement.classList.contains('dark'))
function toggleDark() {
  isDark.value = !isDark.value
  document.documentElement.classList.toggle('dark', isDark.value)
  localStorage.setItem('b-artifact.theme', isDark.value ? 'dark' : 'light')
}
onMounted(() => {
  const saved = localStorage.getItem('b-artifact.theme')
  if (saved === 'dark') {
    isDark.value = true
    document.documentElement.classList.add('dark')
  }
})

const serverVersion = ref('')
onMounted(async () => {
  try {
    const s = await adminApi.settings()
    serverVersion.value = s.version
  } catch {
    /* 非管理员或接口不可用，忽略 */
  }
})

const pwVisible = ref(false)
const pwLoading = ref(false)
const pwForm = ref({ old_password: '', new_password: '', confirm: '' })

async function submitPassword() {
  if (pwForm.value.new_password !== pwForm.value.confirm) {
    ElMessage.warning('两次输入的新密码不一致')
    return
  }
  // 口令强度（§9.2）：同一份规则镜像，服务端仍是权威
  const weak = passwordPolicyError(pwForm.value.new_password)
  if (weak) {
    ElMessage.warning(weak)
    return
  }
  pwLoading.value = true
  try {
    await authApi.changePassword(pwForm.value.old_password, pwForm.value.new_password)
    ElMessage.success('密码已修改')
    pwVisible.value = false
    pwForm.value = { old_password: '', new_password: '', confirm: '' }
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '修改失败')
  } finally {
    pwLoading.value = false
  }
}

async function onCommand(cmd: string) {
  if (cmd === 'password') {
    pwVisible.value = true
    return
  }
  if (cmd === 'logout') {
    await auth.logout()
    router.push({ name: 'login' })
  }
}
</script>

<style scoped>
.aside {
  background: var(--ba-panel);
  border-right: 1px solid var(--ba-border);
  display: flex;
  flex-direction: column;
}
.brand {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 16px 16px 12px;
}
.logo {
  width: 30px;
  height: 30px;
  border-radius: 8px;
  background: #2f6fed;
  color: #fff;
  display: grid;
  place-items: center;
  font-weight: 700;
}
.brand-name {
  font-weight: 600;
  font-size: 15px;
}
.brand-sub {
  font-size: 11px;
  color: var(--ba-muted);
}
.menu {
  border-right: none;
  flex: 1;
}
.aside-foot {
  padding: 8px;
  border-top: 1px solid var(--ba-border);
}
.header {
  display: flex;
  align-items: center;
  gap: 12px;
  background: var(--ba-panel);
  border-bottom: 1px solid var(--ba-border);
  height: 56px;
}
.user-chip {
  display: flex;
  align-items: center;
  gap: 8px;
  cursor: pointer;
  outline: none;
}
</style>
