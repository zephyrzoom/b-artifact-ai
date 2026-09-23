<template>
  <div class="page">
    <div>
      <h3 class="page-title">系统</h3>
      <p class="page-sub">运行配置（只读，配置文件修改后需重启服务端）与存储维护</p>
    </div>

    <el-row :gutter="12">
      <el-col :span="14">
        <el-card shadow="never">
          <template #header><span>运行配置</span></template>
          <el-skeleton v-if="loading" :rows="8" animated />
          <el-descriptions v-else :column="2" border size="small">
            <el-descriptions-item label="版本">{{ s?.version }}</el-descriptions-item>
            <el-descriptions-item label="监听">{{ s?.server.listen }}</el-descriptions-item>
            <el-descriptions-item label="数据目录" :span="2" class-name="mono">
              {{ s?.server.data_dir }}
            </el-descriptions-item>
            <el-descriptions-item label="压缩">{{ s?.storage.compression }}</el-descriptions-item>
            <el-descriptions-item label="单文件上限">
              {{ s?.storage.max_file_size_mb }} MB
            </el-descriptions-item>
            <el-descriptions-item label="分块阈值">
              {{ s?.storage.chunk_threshold_mb }} MB
            </el-descriptions-item>
            <el-descriptions-item label="连接池">{{ s?.db.pool_size }}</el-descriptions-item>
            <el-descriptions-item label="本地认证">
              <el-tag size="small" :type="s?.auth.local_enabled ? 'success' : 'info'" effect="plain">
                {{ s?.auth.local_enabled ? '启用' : '关闭' }}
              </el-tag>
            </el-descriptions-item>
            <el-descriptions-item label="首登管理员">
              {{ s?.auth.first_user_admin ? '启用' : '关闭' }}
            </el-descriptions-item>
            <el-descriptions-item label="会话有效期">
              {{ s?.auth.session_ttl_days }} 天
            </el-descriptions-item>
            <el-descriptions-item label="登录限流">
              失败 {{ s?.auth.login_max_fails }} 次锁 {{ s?.auth.login_lockout_secs }} 秒
            </el-descriptions-item>
            <el-descriptions-item label="LDAP">
              <el-tag size="small" :type="s?.auth.ldap.enabled ? 'success' : 'info'" effect="plain">
                {{ s?.auth.ldap.enabled ? '启用' : '关闭' }}
              </el-tag>
            </el-descriptions-item>
            <el-descriptions-item label="LDAP URL" :span="2" class-name="mono">
              {{ s?.auth.ldap.url || '—' }}
            </el-descriptions-item>
            <el-descriptions-item label="bind DN" :span="2" class-name="mono">
              {{ s?.auth.ldap.bind_dn || '—' }}
            </el-descriptions-item>
            <el-descriptions-item label="bind 口令">
              <el-tag size="small" :type="s?.auth.ldap.bind_password_set ? 'success' : 'warning'" effect="plain">
                {{ s?.auth.ldap.bind_password_set ? '已配置（不回显）' : '未配置' }}
              </el-tag>
            </el-descriptions-item>
            <el-descriptions-item label="用户过滤器" class-name="mono">
              {{ s?.auth.ldap.user_filter }}
            </el-descriptions-item>
          </el-descriptions>
        </el-card>
      </el-col>

      <el-col :span="10">
        <el-card shadow="never">
          <template #header><span>存储维护（§3.7）</span></template>
          <el-descriptions v-if="m" :column="1" border size="small" style="margin-bottom: 12px">
            <el-descriptions-item label="待回收">{{ m.queued }} 个 blob</el-descriptions-item>
            <el-descriptions-item label="已到期">{{ m.due_now }} 个</el-descriptions-item>
            <el-descriptions-item label="待回收体积">{{ formatBytes(m.queued_bytes) }}</el-descriptions-item>
            <el-descriptions-item label="孤儿 blob">{{ m.orphan_blobs }} 个</el-descriptions-item>
            <el-descriptions-item label="撤销宽限期">
              {{ Math.round(m.grace_secs / 3600) }} 小时
            </el-descriptions-item>
          </el-descriptions>

          <el-button :loading="rebuilding" style="width: 100%; margin-bottom: 8px" @click="rebuild">
            重建 refcount（自愈）
          </el-button>
          <el-button
            type="warning"
            plain
            :loading="gcing"
            style="width: 100%"
            @click="gc"
          >
            立即执行 GC（只处理已到期项）
          </el-button>

          <el-alert type="info" :closable="false" style="margin-top: 14px">
            <template #title>
              refcount 是「引用该 blob 的 (仓库, 修订) 去重数」，可从 changes 表完全重建。
              清零后进入 GC 队列并保留 24 小时撤销期，期间上传相同内容可直接复活、自动出队。
              GC 执行前会复查 pending_commits，避免删掉正在提交中的内容。
            </template>
          </el-alert>

          <el-result
            v-if="lastResult"
            :title="lastResult.title"
            :sub-title="lastResult.sub"
            :icon="lastResult.icon"
            style="padding: 8px 0 0"
          />
        </el-card>
      </el-col>
    </el-row>
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { ElMessage } from 'element-plus'
import { adminApi } from '@/api'
import { ApiError } from '@/api/http'
import type { Maintenance, SystemSettings } from '@/api/types'
import { formatBytes } from '@/utils/format'

const s = ref<SystemSettings | null>(null)
const m = ref<Maintenance | null>(null)
const loading = ref(false)
const rebuilding = ref(false)
const gcing = ref(false)
const lastResult = ref<{ title: string; sub: string; icon: 'success' | 'info' | 'error' } | null>(null)

async function load() {
  loading.value = true
  try {
    const [a, b] = await Promise.all([adminApi.settings(), adminApi.maintenance()])
    s.value = a
    m.value = b
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载设置失败')
  } finally {
    loading.value = false
  }
}

async function rebuild() {
  rebuilding.value = true
  try {
    const r = await adminApi.rebuildRefcount()
    lastResult.value = {
      title: `重建完成：${r.blobs} 个 blob`,
      sub: `其中 ${r.zero_ref} 个引用归零，${r.queued} 个已排入 GC 队列，可回收 ${formatBytes(
        r.reclaimable_bytes,
      )}`,
      icon: 'success',
    }
    await load()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '重建失败')
  } finally {
    rebuilding.value = false
  }
}

async function gc() {
  gcing.value = true
  try {
    const r = await adminApi.runGc()
    lastResult.value = {
      title: `GC 完成：删除 ${r.deleted} 个 blob（${formatBytes(r.bytes)}）`,
      sub: r.skipped_pending ? `跳过 ${r.skipped_pending} 个仍在提交中的 blob` : '无跳过项',
      icon: 'success',
    }
    await load()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : 'GC 失败')
  } finally {
    gcing.value = false
  }
}

onMounted(load)
</script>
