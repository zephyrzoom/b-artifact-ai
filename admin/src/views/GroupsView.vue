<template>
  <div class="page">
    <div class="flex" style="margin-bottom: 12px">
      <div>
        <h3 class="page-title">用户组</h3>
        <p class="page-sub">组可作为权限主体；被权限规则引用的组不可删除</p>
      </div>
      <div class="spacer" />
      <el-button type="primary" :icon="Plus" @click="openCreate">新建组</el-button>
    </div>

    <el-card shadow="never">
      <el-table :data="items" v-loading="loading" size="small">
        <el-table-column prop="name" label="组名" min-width="180" />
        <el-table-column prop="comment" label="说明" min-width="220" />
        <el-table-column prop="members" label="成员数" width="90" align="right" />
        <el-table-column label="创建时间" width="160">
          <template #default="{ row }">{{ formatTime(row.created_at) }}</template>
        </el-table-column>
        <el-table-column label="操作" width="180" fixed="right">
          <template #default="{ row }">
            <el-button text type="primary" size="small" @click="openMembers(row)">成员</el-button>
            <el-button text type="primary" size="small" @click="openEdit(row)">重命名</el-button>
            <el-button text type="danger" size="small" @click="remove(row)">删除</el-button>
          </template>
        </el-table-column>
      </el-table>
    </el-card>

    <el-dialog v-model="dialog" :title="editing ? '编辑组' : '新建组'" width="440px">
      <el-form :model="form" label-width="72px">
        <el-form-item label="组名" required><el-input v-model="form.name" /></el-form-item>
        <el-form-item label="说明"><el-input v-model="form.comment" type="textarea" :rows="2" /></el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialog = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="submit">保存</el-button>
      </template>
    </el-dialog>

    <el-drawer v-model="memberDrawer" :title="`${current?.name ?? ''} 成员`" size="42%">
      <el-transfer
        v-if="memberDrawer"
        v-model="selected"
        :data="allUsers"
        :titles="['全部用户', '组内成员']"
        filterable
        :button-texts="['移出', '加入']"
        :props="{ key: 'id', label: 'label' }"
      />
      <div style="margin-top: 14px">
        <el-button type="primary" :loading="savingMembers" @click="saveMembers">保存成员</el-button>
      </div>
    </el-drawer>
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { Plus } from '@element-plus/icons-vue'
import { adminApi } from '@/api'
import { ApiError } from '@/api/http'
import type { GroupRow, UserRow } from '@/api/types'
import { formatTime } from '@/utils/format'

const items = ref<GroupRow[]>([])
const loading = ref(false)
const dialog = ref(false)
const editing = ref(false)
const saving = ref(false)
const form = ref({ id: 0, name: '', comment: '' })

const memberDrawer = ref(false)
const current = ref<GroupRow | null>(null)
const allUsers = ref<{ id: number; label: string; disabled: boolean }[]>([])
const selected = ref<number[]>([])
const savingMembers = ref(false)

async function load() {
  loading.value = true
  try {
    items.value = (await adminApi.groups()).items
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载组失败')
  } finally {
    loading.value = false
  }
}

function openCreate() {
  editing.value = false
  form.value = { id: 0, name: '', comment: '' }
  dialog.value = true
}

function openEdit(row: GroupRow) {
  editing.value = true
  form.value = { id: row.id, name: row.name, comment: row.comment }
  dialog.value = true
}

async function submit() {
  saving.value = true
  try {
    if (editing.value) await adminApi.updateGroup(form.value.id, form.value.name, form.value.comment)
    else await adminApi.createGroup(form.value.name, form.value.comment)
    ElMessage.success('已保存')
    dialog.value = false
    await load()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '保存失败')
  } finally {
    saving.value = false
  }
}

async function remove(row: GroupRow) {
  try {
    await ElMessageBox.confirm(`删除组 ${row.name}？`, '删除组', { type: 'warning' })
  } catch {
    return
  }
  try {
    await adminApi.deleteGroup(row.id)
    ElMessage.success('已删除')
    await load()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '删除失败')
  }
}

async function openMembers(row: GroupRow) {
  current.value = row
  memberDrawer.value = true
  try {
    const [users, members] = await Promise.all([adminApi.users(), adminApi.members(row.id)])
    allUsers.value = (users.items as UserRow[]).map((u) => ({
      id: u.id,
      label: u.display_name ? `${u.username}（${u.display_name}）` : u.username,
      disabled: false,
    }))
    selected.value = members.items.map((m) => m.id)
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载成员失败')
  }
}

async function saveMembers() {
  if (!current.value) return
  savingMembers.value = true
  try {
    await adminApi.setMembers(current.value.id, selected.value)
    ElMessage.success('成员已更新')
    await load()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '保存失败')
  } finally {
    savingMembers.value = false
  }
}

onMounted(load)
</script>
