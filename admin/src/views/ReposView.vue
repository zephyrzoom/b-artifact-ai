<template>
  <div class="page">
    <div class="flex" style="margin-bottom: 12px">
      <div>
        <h3 class="page-title">仓库</h3>
        <p class="page-sub">按目录配置权限、按文件加锁的集中式资产库</p>
      </div>
      <div class="spacer" />
      <el-button type="primary" :icon="Plus" @click="createVisible = true">新建仓库</el-button>
    </div>

    <el-card shadow="never">
      <el-table :data="items" v-loading="loading" style="width: 100%">
        <el-table-column label="仓库" min-width="200">
          <template #default="{ row }">
            <el-link type="primary" :underline="false" @click="open(row.name)">
              {{ row.name }}
            </el-link>
            <div class="muted" style="font-size: 12px">{{ row.description }}</div>
          </template>
        </el-table-column>
        <el-table-column prop="owner" label="所有者" width="120" />
        <el-table-column prop="head_rev" label="修订" width="80" align="right" />
        <el-table-column label="策略" width="120">
          <template #default>
            <el-tag size="small" effect="plain" type="warning">先锁后提交</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="我的权限" width="180">
          <template #default="{ row }">
            <el-tag size="small" :type="row.my_permissions.admin ? 'danger' : row.my_permissions.write ? 'warning' : 'success'" effect="plain">
              {{ row.my_permissions.admin ? '管理' : row.my_permissions.write ? '读写' : '只读' }}
            </el-tag>
          </template>
        </el-table-column>
        <el-table-column label="创建时间" width="150">
          <template #default="{ row }">{{ formatTime(row.created_at) }}</template>
        </el-table-column>
        <el-table-column label="操作" width="120" fixed="right">
          <template #default="{ row }">
            <el-button text type="primary" size="small" @click="open(row.name)">管理</el-button>
            <el-button
              v-if="row.my_permissions.admin"
              text
              type="danger"
              size="small"
              @click="confirmDelete(row)"
            >
              删除
            </el-button>
          </template>
        </el-table-column>
      </el-table>
    </el-card>

    <el-dialog v-model="createVisible" title="新建仓库" width="460px">
      <el-form :model="form" label-width="100px">
        <el-form-item label="名称" required>
          <el-input v-model="form.name" placeholder="字母数字与 . _ -" />
        </el-form-item>
        <el-form-item label="描述">
          <el-input v-model="form.description" type="textarea" :rows="2" />
        </el-form-item>
        <el-form-item label="策略">
          <span class="muted" style="font-size: 13px">
            先锁后提交（唯一策略）：提交时每个文件都必须由本人持锁
          </span>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="createVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="submit">创建</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { useRouter } from 'vue-router'
import { ElMessage, ElMessageBox } from 'element-plus'
import { Plus } from '@element-plus/icons-vue'
import { adminApi, repoApi } from '@/api'
import { ApiError } from '@/api/http'
import type { Repo } from '@/api/types'
import { formatTime } from '@/utils/format'

const router = useRouter()
const items = ref<Repo[]>([])
const loading = ref(false)
const createVisible = ref(false)
const saving = ref(false)
const form = reactive({ name: '', description: '' })

async function load() {
  loading.value = true
  try {
    items.value = (await repoApi.list()).items
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载仓库失败')
  } finally {
    loading.value = false
  }
}

async function submit() {
  if (!form.name.trim()) {
    ElMessage.warning('请填写仓库名')
    return
  }
  saving.value = true
  try {
    await repoApi.create(form.name.trim(), form.description)
    ElMessage.success('仓库已创建')
    createVisible.value = false
    form.name = ''
    form.description = ''
    await load()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '创建失败')
  } finally {
    saving.value = false
  }
}

function open(name: string) {
  router.push({ name: 'repo-detail', params: { name } })
}

async function confirmDelete(row: Repo) {
  try {
    await ElMessageBox.confirm(
      `将删除仓库 ${row.name} 的全部修订、权限规则与锁记录，blob 进入 GC 队列（可撤销期 24 小时）。`,
      '删除仓库',
      { type: 'warning', confirmButtonText: '我确认删除', cancelButtonText: '取消' },
    )
  } catch {
    return
  }
  try {
    const r = await adminApi.deleteRepo(row.name)
    ElMessage.success(`仓库 ${row.name} 已删除`)
    void r
    await load()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '删除失败')
  }
}

onMounted(load)
</script>
