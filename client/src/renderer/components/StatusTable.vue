<template>
  <el-card shadow="never" data-testid="status-card">
    <template #header>
      <div class="flex">
        <span>工作副本状态</span>
        <div class="spacer" />
        <span class="muted" style="font-size: 12px" data-testid="status-summary">
          {{ summaryText(summary) }}
        </span>
      </div>
    </template>

    <el-table
      :data="rows"
      size="small"
      height="360"
      row-key="path"
      data-testid="status-table"
      @selection-change="onSelection"
    >
      <el-table-column type="selection" width="42" :selectable="selectable" />
      <el-table-column label="路径" min-width="300">
        <template #default="{ row }">
          <span class="mono">{{ row.path }}</span>
        </template>
      </el-table-column>
      <el-table-column label="状态" width="110">
        <template #default="{ row }">
          <el-tag size="small" :type="tagOf(row.status)" effect="plain" :data-testid="`st-${row.path}`">
            {{ labelOf(row.status) }}
          </el-tag>
        </template>
      </el-table-column>
      <el-table-column label="类型" width="70">
        <template #default="{ row }">{{ row.kind === 'dir' ? '目录' : '文件' }}</template>
      </el-table-column>
      <el-table-column label="大小" width="100" align="right">
        <template #default="{ row }">{{ row.kind === 'dir' ? '-' : formatBytes(row.size) }}</template>
      </el-table-column>
      <el-table-column label="基线" width="80" align="right">
        <template #default="{ row }">r{{ row.base_rev }}</template>
      </el-table-column>
    </el-table>
  </el-card>
</template>

<script setup lang="ts">
import { computed } from 'vue';

import type { StatusCode, StatusItem } from '@shared/dto';

import { formatBytes } from '@/utils/format';
import {
  isActionable,
  sortStatus,
  STATUS_LABEL,
  STATUS_TAG,
  summarize,
  summaryText,
  type TagType,
} from '@/utils/status';

const props = defineProps<{ items: readonly StatusItem[] }>();
const emit = defineEmits<{ (e: 'update:selection', paths: string[]): void }>();

const rows = computed(() => sortStatus(props.items));
const summary = computed(() => summarize(props.items));

/**
 * 可勾选 = "这一步有东西可做"：本地变更、未纳管（要 add）、冲突（要 revert）。
 * 别用 `isCommittable` 当这个判断——那会把 unversioned 也置灰，
 * 用户就没法把一个新文件标记新增了。
 */
function selectable(row: StatusItem): boolean {
  return isActionable(row.status);
}

function onSelection(picked: StatusItem[]): void {
  emit('update:selection', picked.map((p) => p.path));
}

/**
 * 模板里 `#default="{ row }"` 的 row 是 `any`，直接拿它索引 `Record<StatusCode, …>`
 * 会在 `noImplicitAny` 下报错。用带类型的取值函数收口，既消掉 any、也把映射集中在一处。
 */
function tagOf(status: StatusCode): TagType {
  return STATUS_TAG[status];
}

function labelOf(status: StatusCode): string {
  return STATUS_LABEL[status];
}
</script>
