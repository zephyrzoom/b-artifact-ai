<template>
  <div class="page">
    <div>
      <h3 class="page-title">审计日志</h3>
      <p class="page-sub">所有管理动作与登录事件的完整留痕（含强制解锁原因、purge 原因）</p>
    </div>

    <el-card shadow="never" style="margin-bottom: 12px">
      <el-form :inline="true" size="small">
        <el-form-item label="时间">
          <el-date-picker
            v-model="range"
            type="datetimerange"
            value-format="YYYY-MM-DDTHH:mm:ss"
            start-placeholder="开始"
            end-placeholder="结束"
            style="width: 340px"
          />
        </el-form-item>
        <el-form-item label="用户">
          <el-input v-model="q.user" placeholder="用户名或 id" style="width: 140px" />
        </el-form-item>
        <el-form-item label="动作">
          <el-select v-model="q.action" clearable filterable style="width: 170px">
            <el-option v-for="a in actions" :key="a" :label="`${actionLabel(a)}（${a}）`" :value="a" />
          </el-select>
        </el-form-item>
        <el-form-item label="仓库">
          <el-input v-model="q.repo" placeholder="仓库名" style="width: 140px" />
        </el-form-item>
        <el-form-item>
          <el-button type="primary" :icon="Search" @click="search">查询</el-button>
          <el-button :icon="Download" @click="exportCsv">导出 CSV</el-button>
        </el-form-item>
      </el-form>
    </el-card>

    <el-card shadow="never">
      <el-table :data="items" v-loading="loading" size="small" height="560">
        <el-table-column label="时间" width="150">
          <template #default="{ row }">{{ formatTime(row.ts) }}</template>
        </el-table-column>
        <el-table-column prop="username" label="用户" width="120" />
        <el-table-column label="动作" width="150">
          <template #default="{ row }">
            <el-tag size="small" effect="plain">{{ actionLabel(row.action) }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column prop="repo" label="仓库" width="120" />
        <el-table-column prop="target" label="对象" min-width="180" class-name="mono" />
        <el-table-column prop="detail" label="详情" min-width="260" show-overflow-tooltip />
        <el-table-column prop="ip" label="IP" width="120" />
      </el-table>
      <div class="flex" style="margin-top: 10px">
        <!-- 服务端只回当页条数，用"是否满页"推断还有没有下一页 -->
        <el-pagination
          v-model:current-page="page"
          :page-size="limit"
          :total="totalDisplay"
          layout="prev, pager, next"
          :pager-count="7"
          @current-change="load"
        />
        <div class="spacer" />
        <span class="muted" style="font-size: 12px">最多浏览最近 1000 条，导出 CSV 无此限制</span>
      </div>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { Download, Search } from '@element-plus/icons-vue'
import { adminApi } from '@/api'
import { ApiError } from '@/api/http'
import type { AuditRow } from '@/api/types'
import { actionLabel, formatTime } from '@/utils/format'

const actions = [
  'user.login',
  'user.logout',
  'user.create',
  'user.update',
  'user.delete',
  'user.password_reset',
  'group.create',
  'group.update',
  'group.delete',
  'group.members',
  'repo.create',
  'repo.delete',
  'repo.settings',
  'repo.purge',
  'repo.commit',
  'acl.set',
  'acl.update',
  'acl.delete',
  'lock.acquire',
  'lock.release',
  'lock.break',
  'maintenance.gc',
  'maintenance.rebuild_refcount',
]

const items = ref<AuditRow[]>([])
const loading = ref(false)
const page = ref(1)
const limit = 50
/** 满页说明可能还有下一页，据此给分页组件一个可翻页的上界 */
const totalDisplay = computed(() => (page.value - 1) * limit + items.value.length + (items.value.length === limit ? limit : 0))
const range = ref<[string, string] | null>(null)
const q = reactive({ user: '', action: '', repo: '' })

function buildQuery(extra: Record<string, unknown> = {}) {
  return {
    from: range.value?.[0] ?? '',
    to: range.value?.[1] ?? '',
    user: q.user.trim(),
    action: q.action,
    repo: q.repo.trim(),
    limit: extra.limit ?? limit,
    offset: ((page.value - 1) * limit).toString(),
  }
}

async function load() {
  loading.value = true
  try {
    const r = await adminApi.audit(buildQuery())
    items.value = r.items
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载审计失败')
  } finally {
    loading.value = false
  }
}

function search() {
  page.value = 1
  load()
}

async function exportCsv() {
  try {
    await adminApi.exportAudit(buildQuery({ limit: 100000 }))
    ElMessage.success('已导出')
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '导出失败')
  }
}

onMounted(load)
</script>
