<template>
  <div class="page">
    <div class="flex" style="margin-bottom: 12px">
      <div>
        <h3 class="page-title">用户</h3>
        <p class="page-sub">本地账号与 LDAP 账号（LDAP 密码由目录服务托管，此处不可重置）</p>
      </div>
      <div class="spacer" />
      <el-button type="primary" :icon="Plus" @click="openCreate">新建用户</el-button>
    </div>

    <el-card shadow="never">
      <el-table :data="items" v-loading="loading" size="small">
        <el-table-column prop="username" label="用户名" min-width="140" />
        <el-table-column prop="display_name" label="显示名" min-width="140" />
        <el-table-column label="来源" width="90">
          <template #default="{ row }">
            <el-tag size="small" :type="row.source === 'local' ? 'success' : 'warning'" effect="plain">
              {{ row.source }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="组" min-width="160">
          <template #default="{ row }">
            <el-tag
              v-for="g in row.groups"
              :key="g.id"
              size="small"
              effect="plain"
              style="margin-right: 4px"
            >
              {{ g.name }}
            </el-tag>
            <span v-if="!row.groups?.length" class="muted">—</span>
          </template>
        </el-table-column>
        <el-table-column label="管理员" width="90">
          <template #default="{ row }">
            <el-switch
              :model-value="row.is_admin"
              size="small"
              :disabled="row.id === me?.id"
              @change="(v: boolean) => setFlag(row, 'is_admin', v)"
            />
          </template>
        </el-table-column>
        <el-table-column label="启用" width="90">
          <template #default="{ row }">
            <el-switch
              :model-value="!row.disabled"
              size="small"
              :disabled="row.id === me?.id"
              @change="(v: boolean) => setFlag(row, 'disabled', !v)"
            />
          </template>
        </el-table-column>
        <el-table-column label="最后登录" width="150">
          <template #default="{ row }">{{ formatTime(row.last_login_at) }}</template>
        </el-table-column>
        <el-table-column label="操作" width="180" fixed="right">
          <template #default="{ row }">
            <el-button text type="primary" size="small" @click="openEdit(row)">编辑</el-button>
            <el-button
              v-if="row.source === 'local'"
              text
              type="warning"
              size="small"
              @click="resetPwd(row)"
            >
              重置密码
            </el-button>
            <el-button
              text
              type="danger"
              size="small"
              :disabled="row.id === me?.id"
              @click="remove(row)"
            >
              删除
            </el-button>
          </template>
        </el-table-column>
      </el-table>
    </el-card>

    <el-dialog v-model="dialog" :title="editing ? '编辑用户' : '新建用户'" width="480px">
      <el-form :model="form" label-width="92px">
        <el-form-item label="用户名" required>
          <el-input v-model="form.username" :disabled="editing" />
        </el-form-item>
        <el-form-item v-if="!editing" label="密码" required>
          <el-input
            v-model="form.password"
            type="password"
            show-password
            :placeholder="PASSWORD_HINT"
          />
        </el-form-item>
        <el-form-item label="显示名">
          <el-input v-model="form.display_name" />
        </el-form-item>
        <el-form-item v-if="!editing" label="系统管理员">
          <el-switch v-model="form.is_admin" />
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialog = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="submit">保存</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { Plus } from '@element-plus/icons-vue'
import { adminApi } from '@/api'
import { ApiError } from '@/api/http'
import { useAuthStore } from '@/stores/auth'
import type { UserRow } from '@/api/types'
import { formatTime } from '@/utils/format'
import { PASSWORD_HINT, passwordPolicyError } from '@/utils/password'

const auth = useAuthStore()
const me = computed(() => auth.user)

const items = ref<UserRow[]>([])
const loading = ref(false)
const dialog = ref(false)
const editing = ref(false)
const saving = ref(false)
const form = ref({
  id: 0,
  username: '',
  password: '',
  display_name: '',
  is_admin: false,
})

async function load() {
  loading.value = true
  try {
    items.value = (await adminApi.users()).items
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载用户失败')
  } finally {
    loading.value = false
  }
}

function openCreate() {
  editing.value = false
  form.value = { id: 0, username: '', password: '', display_name: '', is_admin: false }
  dialog.value = true
}

function openEdit(row: UserRow) {
  editing.value = true
  form.value = {
    id: row.id,
    username: row.username,
    password: '',
    display_name: row.display_name,
    is_admin: row.is_admin,
  }
  dialog.value = true
}

async function submit() {
  // 口令强度（§9.2）：前端先拦一道，省掉一次必然失败的请求；服务端仍是权威
  if (!editing.value) {
    const weak = passwordPolicyError(form.value.password)
    if (weak) {
      ElMessage.warning(weak)
      return
    }
  }
  saving.value = true
  try {
    if (editing.value) {
      await adminApi.updateUser(form.value.id, {
        display_name: form.value.display_name,
      })
    } else {
      await adminApi.createUser({
        username: form.value.username,
        password: form.value.password,
        display_name: form.value.display_name,
        is_admin: form.value.is_admin,
      })
    }
    ElMessage.success('已保存')
    dialog.value = false
    await load()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '保存失败')
  } finally {
    saving.value = false
  }
}

async function setFlag(row: UserRow, key: 'is_admin' | 'disabled', v: boolean) {
  const old = row[key]
  row[key] = v
  try {
    await adminApi.updateUser(row.id, { [key]: v })
    ElMessage.success('已更新')
  } catch (e) {
    row[key] = old
    ElMessage.error(e instanceof ApiError ? e.message : '更新失败')
  }
}

async function resetPwd(row: UserRow) {
  let pwd = ''
  try {
    const r = await ElMessageBox.prompt(`为 ${row.username} 设置新密码（至少 8 位）`, '重置密码', {
      inputType: 'password',
      inputPattern: /.{8,}/,
      inputErrorMessage: '至少 8 个字符',
    })
    pwd = r.value
  } catch {
    return
  }
  try {
    await adminApi.resetPassword(row.id, pwd)
    ElMessage.success('密码已重置，该用户现有会话已吊销')
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '重置失败')
  }
}

async function remove(row: UserRow) {
  try {
    await ElMessageBox.confirm(
      `删除用户 ${row.username}？其拥有的仓库会自动转归当前操作者。`,
      '删除用户',
      { type: 'warning' },
    )
  } catch {
    return
  }
  try {
    await adminApi.deleteUser(row.id)
    ElMessage.success('已删除')
    await load()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '删除失败')
  }
}

onMounted(load)
</script>
