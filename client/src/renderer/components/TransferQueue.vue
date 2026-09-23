<template>
  <el-card shadow="never" style="margin-top: 12px" data-testid="transfer-queue">
    <template #header>
      <div class="flex">
        <span>传输队列</span>
        <el-tag size="small" effect="plain">{{ phaseLabel }}</el-tag>
        <div class="spacer" />
        <span class="muted mono" style="font-size: 12px" data-testid="transfer-text">
          {{ progressText(progress.done, progress.total) }}
        </span>
      </div>
    </template>
    <el-progress
      :percentage="percent(progress.done, progress.total)"
      :stroke-width="10"
      data-testid="transfer-bar"
    />
    <div v-if="progress.current" class="mono muted" style="margin-top: 6px; font-size: 12px">
      {{ progress.current }}
    </div>
  </el-card>
</template>

<script setup lang="ts">
import { computed } from 'vue';

import type { ProgressEvent } from '@shared/dto';

import { percent, progressText } from '@/utils/format';

const props = defineProps<{ progress: ProgressEvent }>();

const PHASE_LABEL: Record<string, string> = {
  hash: '计算哈希',
  upload: '上传',
  download: '下载',
  commit: '提交',
  update: '更新',
};

const phaseLabel = computed(() => PHASE_LABEL[props.progress.phase] ?? props.progress.phase);
</script>
