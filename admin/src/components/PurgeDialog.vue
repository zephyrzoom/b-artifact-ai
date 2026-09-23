<template>
  <el-dialog v-model="visible" title="清除历史（不可恢复）" width="620px" :close-on-click-modal="false">
    <el-alert type="error" :closable="false" style="margin-bottom: 14px">
      <template #title>
        purge 会连同<b>全部历史修订</b>一起抹掉（L2 删除），不像普通删除那样留下 Tombstone 可供回溯。
        执行后该前缀在每一个历史修订里都"从未存在过"。
      </template>
    </el-alert>

    <el-form :model="form" label-width="100px">
      <el-form-item label="目录前缀">
        <el-input v-model="form.prefix" placeholder="secret/ 或 src/internal" class="mono" />
      </el-form-item>
      <el-form-item label="原因">
        <el-input
          v-model="form.reason"
          type="textarea"
          :rows="2"
          placeholder="必填，≥4 字，会完整写入审计日志"
        />
      </el-form-item>
      <el-form-item label="确认">
        <div>
          <div style="font-size: 12px; margin-bottom: 6px">
            请输入仓库名 <b class="mono">{{ repo }}</b> 以确认（防误触）
          </div>
          <el-input v-model="form.confirm_name" class="mono" :placeholder="repo" />
        </div>
      </el-form-item>
    </el-form>

    <el-collapse v-if="result">
      <el-collapse-item title="执行结果" name="r">
        <el-descriptions :column="2" size="small" border>
          <el-descriptions-item label="删除路径数">{{ result.paths_removed }}</el-descriptions-item>
          <el-descriptions-item label="受影响修订">{{ result.revisions_affected }}</el-descriptions-item>
          <el-descriptions-item label="修订区间">
            {{ revRangeText(result.rev_range) }}
          </el-descriptions-item>
          <el-descriptions-item label="释放锁">{{ result.locks_released }}</el-descriptions-item>
          <el-descriptions-item label="待回收 blob">{{ result.blobs_reclaimed }}</el-descriptions-item>
          <el-descriptions-item label="待回收字节">{{ formatBytes(result.bytes_reclaimed) }}</el-descriptions-item>
          <el-descriptions-item label="GC 计划时间" :span="2">
            {{ formatTime(result.gc_scheduled_at) }}
          </el-descriptions-item>
        </el-descriptions>
      </el-collapse-item>
    </el-collapse>

    <template #footer>
      <el-button @click="visible = false">取消</el-button>
      <el-button
        type="danger"
        :loading="running"
        :disabled="!canSubmit"
        @click="run"
      >
        我确认，清除历史
      </el-button>
    </template>
  </el-dialog>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { ElMessage } from 'element-plus'
import { adminApi } from '@/api'
import { ApiError } from '@/api/http'
import type { PurgeResult } from '@/api/types'
import { formatBytes, formatTime } from '@/utils/format'
import { canPurge, emptyPurgeForm, purgePayload, revRangeText, type PurgeForm } from '@/utils/purge'

const props = defineProps<{ modelValue: boolean; repo: string; prefix?: string }>()
const emit = defineEmits<{ (e: 'update:modelValue', v: boolean): void; (e: 'done'): void }>()

const form = ref<PurgeForm>(emptyPurgeForm())
const running = ref(false)
const result = ref<PurgeResult | null>(null)

const visible = ref(props.modelValue)
// immediate：父组件若以 v-if 或初始 true 挂载本对话框，也要把 prefix 预填上、把上一次
// 的结果清掉——否则会拿着一份空表单一副"什么都没发生过"的样子让人去点确认。
// （watch 必须声明在 form / result 之后：immediate 回调在 setup 期间就会执行。）
watch(
  () => props.modelValue,
  (v) => {
    visible.value = v
    if (v) {
      form.value = emptyPurgeForm(props.prefix ?? '')
      result.value = null
    }
  },
  { immediate: true },
)
watch(visible, (v) => emit('update:modelValue', v))

const canSubmit = computed(() => canPurge(form.value, props.repo))

async function run() {
  running.value = true
  try {
    result.value = await adminApi.purge(props.repo, purgePayload(form.value))
    ElMessage.success('历史已清除，blob 已排入 GC 队列')
    emit('done')
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '清除失败')
  } finally {
    running.value = false
  }
}
</script>
