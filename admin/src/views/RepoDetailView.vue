<template>
  <div class="page">
    <div class="flex" style="margin-bottom: 10px">
      <div>
        <h3 class="page-title">
          <el-button text :icon="ArrowLeft" style="margin-right: 4px" @click="$router.push('/repos')" />
          {{ name }}
        </h3>
        <p class="page-sub">
          <span v-if="info">
            r{{ info.head_rev }} · {{ info.stats.file_count }} 个文件 ·
            {{ formatBytes(info.stats.total_size) }} · {{ info.stats.rev_count }} 个修订
          </span>
        </p>
      </div>
      <div class="spacer" />
      <el-button type="danger" plain :icon="Delete" @click="purgeVisible = true">
        清除历史…
      </el-button>
    </div>

    <el-tabs v-model="tab" @tab-change="onTab">
      <el-tab-pane label="浏览" name="browse">
        <div class="flex" style="margin-bottom: 10px">
          <el-breadcrumb separator="/">
            <el-breadcrumb-item>
              <a @click.prevent="gotoPath('')">/</a>
            </el-breadcrumb-item>
            <el-breadcrumb-item v-for="(seg, i) in segments" :key="i">
              <a @click.prevent="gotoPath(segments.slice(0, i + 1).join('/'))">{{ seg }}</a>
            </el-breadcrumb-item>
          </el-breadcrumb>
          <div class="spacer" />
          <el-input-number v-model="rev" :min="0" :max="info?.head_rev ?? 0" size="small" style="width: 130px" />
          <span class="muted" style="font-size: 12px; margin-left: 6px">修订（0=HEAD）</span>
        </div>
        <el-table :data="tree" v-loading="treeLoading" size="small" height="520">
          <el-table-column label="名称" min-width="260">
            <template #default="{ row }">
              <el-icon style="margin-right: 6px; vertical-align: -2px">
                <Folder v-if="row.kind === 'dir'" />
                <Document v-else />
              </el-icon>
              <el-link
                v-if="row.kind === 'dir'"
                type="primary"
                :underline="false"
                @click="gotoPath(row.path)"
              >
                {{ basename(row.path) }}
              </el-link>
              <span v-else class="mono">{{ basename(row.path) }}</span>
            </template>
          </el-table-column>
          <el-table-column label="大小" width="100" align="right">
            <template #default="{ row }">{{ row.kind === 'dir' ? '' : formatBytes(row.size) }}</template>
          </el-table-column>
          <el-table-column label="变更修订" width="100" align="right" prop="changed_rev" />
          <el-table-column label="blob" min-width="240">
            <template #default="{ row }">
              <span class="mono muted">{{ row.blob_hash ?? '' }}</span>
            </template>
          </el-table-column>
        </el-table>
      </el-tab-pane>

      <el-tab-pane label="权限" name="acl">
        <AclEditor v-if="tab === 'acl'" ref="aclRef" :repo="name" />
      </el-tab-pane>

      <el-tab-pane label="锁" name="locks">
        <div class="flex" style="margin-bottom: 10px">
          <el-checkbox v-model="includeBroken">显示已强制解锁的锁</el-checkbox>
          <div class="spacer" />
          <el-button size="small" :icon="Refresh" @click="loadLocks">刷新</el-button>
        </div>
        <el-table :data="locks" v-loading="locksLoading" size="small">
          <el-table-column label="路径" min-width="240" class-name="mono" prop="path" />
          <el-table-column label="类型" width="80">
            <template #default="{ row }">
              <el-tag size="small" :type="row.kind === 'dir' ? 'warning' : 'info'" effect="plain">
                {{ row.kind }}
              </el-tag>
            </template>
          </el-table-column>
          <el-table-column prop="owner" label="持有者" width="120" />
          <el-table-column prop="comment" label="备注" min-width="160" />
          <el-table-column label="加锁时间" width="150">
            <template #default="{ row }">{{ formatTime(row.created_at) }}</template>
          </el-table-column>
          <el-table-column label="过期" width="150">
            <template #default="{ row }">{{ row.expires_at ? formatTime(row.expires_at) : '不过期' }}</template>
          </el-table-column>
          <el-table-column label="操作" width="140" fixed="right">
            <template #default="{ row }">
              <el-button
                v-if="!row.broken_at"
                text
                type="danger"
                size="small"
                @click="breakLock(row)"
              >
                强制解锁
              </el-button>
              <span v-else class="muted" style="font-size: 12px">
                {{ row.broken_by_name }} 于 {{ formatTime(row.broken_at) }}
              </span>
            </template>
          </el-table-column>
        </el-table>
      </el-tab-pane>

      <el-tab-pane label="修订历史" name="log">
        <el-table :data="revisions" v-loading="logLoading" size="small" height="520">
          <el-table-column prop="rev" label="r" width="70" align="right" />
          <el-table-column prop="author" label="作者" width="120" />
          <el-table-column prop="message" label="说明" min-width="260" show-overflow-tooltip />
          <el-table-column label="文件数" width="90" align="right" prop="file_count" />
          <el-table-column label="字节增量" width="110" align="right">
            <template #default="{ row }">
              <span :style="{ color: row.byte_delta >= 0 ? '#f56c6c' : '#67c23a' }">
                {{ row.byte_delta >= 0 ? '+' : '' }}{{ formatBytes(Math.abs(row.byte_delta)) }}
              </span>
            </template>
          </el-table-column>
          <el-table-column label="时间" width="150">
            <template #default="{ row }">{{ formatTime(row.created_at) }}</template>
          </el-table-column>
        </el-table>
      </el-tab-pane>

      <el-tab-pane label="设置" name="settings">
        <el-form :model="settings" label-width="140px" style="max-width: 560px">
          <el-form-item label="描述">
            <el-input v-model="settings.description" type="textarea" :rows="2" />
          </el-form-item>
          <el-form-item label="提交策略">
            <span class="muted" style="font-size: 13px">
              先锁后提交（唯一策略，v0.4.17 起不可配置）：提交时每个文件都必须由本人持锁
            </span>
          </el-form-item>
          <el-form-item>
            <el-button type="primary" :loading="saving" @click="saveSettings">保存</el-button>
          </el-form-item>
        </el-form>
      </el-tab-pane>
    </el-tabs>

    <PurgeDialog v-model="purgeVisible" :repo="name" :prefix="currentPrefix" @done="afterPurge" />
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { useRoute } from 'vue-router'
import { ElMessage, ElMessageBox } from 'element-plus'
import { ArrowLeft, Delete, Document, Folder, Refresh } from '@element-plus/icons-vue'
import { adminApi, repoApi } from '@/api'
import { ApiError } from '@/api/http'
import type { Lock, RepoInfo, Revision, TreeEntry } from '@/api/types'
import { basename, formatBytes, formatTime } from '@/utils/format'
import AclEditor from '@/components/AclEditor.vue'
import PurgeDialog from '@/components/PurgeDialog.vue'

const route = useRoute()
const name = computed(() => (route.params.name as string) ?? '')

const tab = ref('browse')
const info = ref<RepoInfo | null>(null)

// ---- 浏览 ----
const prefix = ref('')
const rev = ref(0)
const tree = ref<TreeEntry[]>([])
const treeLoading = ref(false)
const segments = computed(() => prefix.value.split('/').filter(Boolean))
const currentPrefix = computed(() => prefix.value)

// ---- 锁 ----
const locks = ref<Lock[]>([])
const locksLoading = ref(false)
const includeBroken = ref(false)

// ---- 修订 ----
const revisions = ref<Revision[]>([])
const logLoading = ref(false)

// ---- 设置 ----
const saving = ref(false)
const settings = ref({ description: '' })

const purgeVisible = ref(false)

async function loadInfo() {
  try {
    info.value = await repoApi.info(name.value)
    settings.value = {
      description: info.value.description ?? '',
    }
    rev.value = 0
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载仓库信息失败')
  }
}

async function loadTree() {
  treeLoading.value = true
  try {
    const r = await repoApi.tree(name.value, { prefix: prefix.value, rev: rev.value, depth: 1 })
    tree.value = r.items.filter((i) => i.path !== prefix.value)
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载目录失败')
  } finally {
    treeLoading.value = false
  }
}

async function loadLocks() {
  locksLoading.value = true
  try {
    locks.value = (await repoApi.locks(name.value, { include_broken: includeBroken.value })).items
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载锁失败')
  } finally {
    locksLoading.value = false
  }
}

async function loadLog() {
  logLoading.value = true
  try {
    revisions.value = (await repoApi.log(name.value, { limit: 200 })).items
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载历史失败')
  } finally {
    logLoading.value = false
  }
}

function gotoPath(p: string) {
  prefix.value = p
  loadTree()
}

function onTab(t: string) {
  if (t === 'locks') loadLocks()
  if (t === 'log') loadLog()
}

async function breakLock(row: Lock) {
  let reason = ''
  try {
    const r = await ElMessageBox.prompt(
      `强制解锁 ${row.owner} 持有的锁 ${row.path}？原因必填并写进审计。`,
      '强制解锁',
      { inputPattern: /\S{4,}/, inputErrorMessage: '原因至少 4 个字符', type: 'warning' },
    )
    reason = r.value
  } catch {
    return
  }
  try {
    await repoApi.releaseLock(name.value, row.path, { force: true, reason })
    ElMessage.success('已强制解锁')
    await loadLocks()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '强制解锁失败')
  }
}

async function saveSettings() {
  saving.value = true
  try {
    await adminApi.updateRepoSettings(name.value, settings.value)
    ElMessage.success('已保存')
    await loadInfo()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '保存失败')
  } finally {
    saving.value = false
  }
}

function afterPurge() {
  loadInfo()
  if (tab.value === 'log') loadLog()
  if (tab.value === 'locks') loadLocks()
  if (tab.value === 'browse') loadTree()
}

watch(rev, () => loadTree())
watch(includeBroken, () => loadLocks())
watch(name, () => {
  prefix.value = ''
  loadInfo()
  loadTree()
})

onMounted(() => {
  loadInfo()
  loadTree()
})
</script>
