<template>
  <div>
    <div class="flex" style="margin-bottom: 10px">
      <el-button type="primary" size="small" :icon="Plus" @click="openCreate">新增规则</el-button>
      <el-button size="small" :icon="Refresh" @click="load">刷新</el-button>
      <el-tag v-if="shadowedCount" type="warning" effect="plain" size="small">
        {{ shadowedCount }} 条规则被同层更具体的规则压过（永不生效）
      </el-tag>
      <div class="spacer" />
      <el-button size="small" :icon="View" @click="previewVisible = true">有效权限预览器</el-button>
      <el-button size="small" :icon="User" @click="whoVisible = true">谁有权限</el-button>
    </div>

    <el-table :data="rules" v-loading="loading" size="small" border>
      <el-table-column label="目录" min-width="180">
        <template #default="{ row }">
          <span class="mono">{{ row.path_prefix || '/' }}</span>
          <el-tag v-if="!row.inherit" type="danger" size="small" effect="dark" style="margin-left: 6px">
            屏障
          </el-tag>
        </template>
      </el-table-column>
      <el-table-column label="主体" min-width="180">
        <template #default="{ row }">
          {{ row.subject_label }}
          <el-tag size="small" effect="plain" style="margin-left: 6px">
            {{ SUBJECT_LABEL[row.subject_type] }}
          </el-tag>
        </template>
      </el-table-column>
      <el-table-column label="级别" width="90">
        <template #default="{ row }">
          <el-tag :type="LEVEL_TAG[row.level]" size="small" :effect="row.shadowed ? 'plain' : 'dark'">
            {{ LEVEL_LABEL[row.level] }}
          </el-tag>
        </template>
      </el-table-column>
      <el-table-column label="继承" width="90">
        <template #default="{ row }">
          <el-switch
            :model-value="row.inherit"
            size="small"
            active-text="是"
            inactive-text="否"
            @change="(v: boolean) => toggleInherit(row, v)"
          />
        </template>
      </el-table-column>
      <el-table-column label="状态" width="150">
        <template #default="{ row }">
          <el-tag v-if="row.shadowed" type="warning" size="small" effect="plain">
            被压过 · 永不生效
          </el-tag>
          <el-tag v-else type="success" size="small" effect="plain">本层生效</el-tag>
        </template>
      </el-table-column>
      <el-table-column label="操作" width="140" fixed="right">
        <template #default="{ row }">
          <el-button text type="primary" size="small" @click="openEdit(row)">编辑</el-button>
          <el-button text type="danger" size="small" @click="remove(row)">删除</el-button>
        </template>
      </el-table-column>
    </el-table>

    <el-alert type="info" :closable="false" style="margin-top: 12px">
      <template #title>
        解析规则（§4.2）：自下而上回溯到第一个"命中"的目录；同层内按
        <b>主体具体度</b>（用户 &gt; 组 &gt; 所有人）取最优，同具体度取<b>级别最低</b>的。
        勾选<b>屏障</b>（inherit=false）后，未命中即截断回溯，结果为「无」。系统管理员穿透一切。
      </template>
    </el-alert>

    <!-- 新增/编辑 -->
    <el-dialog v-model="dialogVisible" :title="editing ? '编辑规则' : '新增规则'" width="520px">
      <el-form :model="form" label-width="92px">
        <el-form-item label="目录前缀">
          <el-select
            v-if="!editing"
            v-model="form.path_prefix"
            filterable
            allow-create
            default-first-option
            placeholder="/ 或 src/assets"
            style="width: 100%"
          >
            <el-option v-for="d in dirs" :key="d.path" :label="d.path || '/'" :value="d.path">
              <span class="mono">{{ d.path || '/' }}</span>
              <el-tag v-if="d.has_rules" size="small" type="success" effect="plain" style="margin-left: 8px">
                已配规则
              </el-tag>
            </el-option>
          </el-select>
          <span v-else class="mono">{{ form.path_prefix || '/' }}</span>
        </el-form-item>
        <el-form-item label="主体类型">
          <el-radio-group
            v-model="form.subject_type"
            :disabled="editing"
            @change="form.subject_id = undefined"
          >
            <el-radio value="everyone">所有人</el-radio>
            <el-radio value="group">用户组</el-radio>
            <el-radio value="user">用户</el-radio>
          </el-radio-group>
        </el-form-item>
        <el-form-item v-if="form.subject_type === 'group'" label="用户组">
          <el-select v-model="form.subject_id" filterable :disabled="editing" style="width: 100%">
            <el-option v-for="g in groups" :key="g.id" :label="g.name" :value="g.id" />
          </el-select>
        </el-form-item>
        <el-form-item v-if="form.subject_type === 'user'" label="用户">
          <el-select v-model="form.subject_id" filterable :disabled="editing" style="width: 100%">
            <el-option
              v-for="u in users"
              :key="u.id"
              :label="u.display_name ? `${u.username}（${u.display_name}）` : u.username"
              :value="u.id"
            />
          </el-select>
        </el-form-item>
        <el-form-item label="级别">
          <el-select v-model="form.level" style="width: 100%">
            <el-option v-for="lv in levels" :key="lv" :label="LEVEL_LABEL[lv]" :value="lv" />
          </el-select>
        </el-form-item>
        <el-form-item label="允许继承">
          <el-switch v-model="form.inherit" />
          <span class="muted" style="margin-left: 8px; font-size: 12px">
            关闭 = 继承屏障，未命中时截断回溯
          </span>
        </el-form-item>
      </el-form>
      <template #footer>
        <el-button @click="dialogVisible = false">取消</el-button>
        <el-button type="primary" :loading="saving" @click="submit">保存</el-button>
      </template>
    </el-dialog>

    <!-- 有效权限预览器 -->
    <el-drawer v-model="previewVisible" title="有效权限预览器（§8.3）" size="46%">
      <el-form label-width="80px" size="small">
        <el-form-item label="用户">
          <el-select v-model="previewUser" filterable style="width: 100%">
            <el-option v-for="u in users" :key="u.id" :label="u.username" :value="u.id" />
          </el-select>
        </el-form-item>
        <el-form-item label="路径">
          <el-input v-model="previewPath" placeholder="src/assets/logo.png（留空=仓库根）" />
        </el-form-item>
        <el-form-item>
          <el-button type="primary" size="small" :loading="previewing" @click="runPreview">
            解析
          </el-button>
        </el-form-item>
      </el-form>

      <template v-if="preview && summary">
        <el-result
          :icon="summary.icon"
          :title="summary.title"
          :sub-title="summary.subTitle"
          style="padding: 8px 0"
        />
        <div class="muted" style="font-size: 12px; margin-bottom: 10px">
          主体：{{ preview.user.username }}
          <el-tag v-if="preview.user.is_admin" size="small" type="danger" effect="plain">
            系统管理员
          </el-tag>
          所属组：{{ groupListText(preview.user.groups) }}
        </div>

        <el-steps direction="vertical" :space="60" :active="preview.steps.length">
          <el-step
            v-for="(s, i) in preview.steps"
            :key="i"
            :title="`${s.display} — ${outcomeLabel(s.outcome)}`"
            :status="stepStatusFor(s.outcome)"
            :description="s.reason"
          >
            <template #description>
              <div>{{ s.reason }}</div>
              <div v-if="s.rules?.length" style="margin-top: 6px">
                <el-tag
                  v-for="r in s.rules"
                  :key="r.id"
                  size="small"
                  :type="r.id === pickedRuleId(s) ? 'success' : 'info'"
                  :effect="r.id === pickedRuleId(s) ? 'dark' : 'plain'"
                  style="margin: 2px 4px 2px 0"
                >
                  {{ r.subject_type === 'everyone' ? '所有人' : `#${r.id}` }} ·
                  {{ LEVEL_LABEL[r.level] }}{{ r.inherit ? '' : ' · 屏障' }}
                </el-tag>
              </div>
            </template>
          </el-step>
        </el-steps>
      </template>
    </el-drawer>

    <!-- 谁有权限 -->
    <el-drawer v-model="whoVisible" title="谁有权限（反查）" size="42%">
      <el-form label-width="70px" size="small">
        <el-form-item label="路径">
          <el-input v-model="whoPath" placeholder="留空=仓库根" />
        </el-form-item>
        <el-form-item label="级别">
          <el-select v-model="whoLevel" style="width: 140px">
            <el-option v-for="lv in levels" :key="lv" :label="LEVEL_LABEL[lv]" :value="lv" />
          </el-select>
          <el-button type="primary" size="small" style="margin-left: 8px" :loading="whoing" @click="runWho">
            查询
          </el-button>
        </el-form-item>
      </el-form>
      <el-table :data="whoItems" size="small" max-height="520">
        <el-table-column prop="username" label="用户" width="140" />
        <el-table-column prop="display_name" label="显示名" min-width="120" />
        <el-table-column label="级别" width="90">
          <template #default="{ row }">
            <el-tag :type="LEVEL_TAG[row.level]" size="small">{{ LEVEL_LABEL[row.level] }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column prop="via" label="来源" min-width="160" />
      </el-table>
    </el-drawer>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { ElMessage, ElMessageBox } from 'element-plus'
import { Plus, Refresh, User, View } from '@element-plus/icons-vue'
import { adminApi } from '@/api'
import { ApiError } from '@/api/http'
import type { AclPreview, AclRule, AclWhoItem, DirNode, GroupRow, UserRow } from '@/api/types'
import { LEVEL_LABEL, LEVEL_TAG, SUBJECT_LABEL } from '@/utils/format'
import {
  aclFormFromRule,
  createAclPayload,
  emptyAclForm,
  groupListText,
  LEVELS,
  outcomeLabel,
  pickedRuleId,
  previewSummary,
  ruleDeletePrompt,
  shadowedRuleCount,
  stepStatusFor,
  updateAclPayload,
  validateAclForm,
  type AclForm,
} from '@/utils/acl'
import { collectDirs } from '@/utils/dirs'

const props = defineProps<{ repo: string }>()

const rules = ref<AclRule[]>([])
const dirs = ref<DirNode[]>([])
const users = ref<UserRow[]>([])
const groups = ref<GroupRow[]>([])
const loading = ref(false)

const levels = LEVELS

const dialogVisible = ref(false)
const editing = ref(false)
const saving = ref(false)
const form = ref<AclForm>(emptyAclForm())

const previewVisible = ref(false)
const previewUser = ref<number>()
const previewPath = ref('')
const preview = ref<AclPreview | null>(null)
const previewing = ref(false)
const summary = computed(() => (preview.value ? previewSummary(preview.value) : null))

const whoVisible = ref(false)
const whoPath = ref('')
const whoLevel = ref<(typeof LEVELS)[number]>('read')
const whoItems = ref<AclWhoItem[]>([])
const whoing = ref(false)

const shadowedCount = computed(() => shadowedRuleCount(rules.value))

async function load() {
  loading.value = true
  try {
    const [acl, u, g] = await Promise.all([
      adminApi.acl(props.repo),
      adminApi.users(),
      adminApi.groups(),
    ])
    rules.value = acl.items
    users.value = u.items
    groups.value = g.items
    dirs.value = await collectDirs((p) => adminApi.dirs(props.repo, p))
    if (!previewUser.value && u.items.length) previewUser.value = u.items[0].id
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载权限规则失败')
  } finally {
    loading.value = false
  }
}

function openCreate() {
  editing.value = false
  form.value = emptyAclForm()
  dialogVisible.value = true
}

function openEdit(row: AclRule) {
  editing.value = true
  form.value = aclFormFromRule(row)
  dialogVisible.value = true
}

async function submit() {
  const invalid = validateAclForm(form.value, editing.value)
  if (invalid) {
    ElMessage.warning(invalid)
    return
  }
  saving.value = true
  try {
    if (editing.value && form.value.id) {
      await adminApi.updateAcl(props.repo, updateAclPayload(form.value))
    } else {
      await adminApi.setAcl(props.repo, createAclPayload(form.value))
    }
    ElMessage.success('已保存')
    dialogVisible.value = false
    await load()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '保存失败')
  } finally {
    saving.value = false
  }
}

async function toggleInherit(row: AclRule, v: boolean) {
  try {
    await adminApi.updateAcl(props.repo, { id: row.id, inherit: v })
    row.inherit = v
    await load()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '更新失败')
  }
}

async function remove(row: AclRule) {
  try {
    await ElMessageBox.confirm(ruleDeletePrompt(row), '删除规则', { type: 'warning' })
  } catch {
    return
  }
  try {
    await adminApi.deleteAcl(props.repo, row.id)
    ElMessage.success('已删除')
    await load()
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '删除失败')
  }
}

async function runPreview() {
  if (!previewUser.value) return
  previewing.value = true
  try {
    preview.value = await adminApi.aclPreview(props.repo, previewUser.value, previewPath.value)
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '解析失败')
  } finally {
    previewing.value = false
  }
}

async function runWho() {
  whoing.value = true
  try {
    const r = await adminApi.aclWho(props.repo, whoPath.value, whoLevel.value)
    whoItems.value = r.items
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '查询失败')
  } finally {
    whoing.value = false
  }
}

onMounted(load)
defineExpose({ load })
</script>
