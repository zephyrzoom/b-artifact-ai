<template>
  <div class="page">
    <el-empty v-if="!store.wc" description="还没有打开工作副本" />

    <template v-else>
      <div class="flex" style="margin-bottom: 12px">
        <div>
          <h3 class="page-title">冲突</h3>
          <p class="page-sub">
            更新时双方都改过的文件会被登记在这里：本地版本存成 <code>.mine</code>，
            工作文件取服务端版本。解决之前不允许提交（§6.2 / §6.5）。
          </p>
        </div>
        <div class="spacer" />
        <el-button size="small" :loading="store.busy" data-testid="conflicts-refresh" @click="reload">
          刷新
        </el-button>
      </div>

      <el-alert
        v-if="store.conflicts.length === 0"
        title="没有未解决的冲突"
        type="success"
        :closable="false"
        show-icon
        data-testid="conflicts-empty"
      />

      <template v-else>
        <el-card shadow="never" style="margin-bottom: 12px">
          <el-table
            :data="store.conflicts"
            size="small"
            highlight-current-row
            :current-row-key="selected"
            row-key="path"
            data-testid="conflict-table"
            @current-change="onPick"
          >
            <el-table-column label="路径" min-width="280">
              <template #default="{ row }">
                <span class="mono">{{ row.path }}</span>
              </template>
            </el-table-column>
            <el-table-column label="成因" width="130">
              <template #default="{ row }">
                <el-tag size="small" :type="row.reason === 'both-modified' ? 'warning' : 'danger'" effect="plain">
                  {{ row.reason === 'both-modified' ? '双方都改' : '服务端已删除' }}
                </el-tag>
              </template>
            </el-table-column>
            <el-table-column label="可文本合并" width="120">
              <template #default="{ row }">
                <el-tag size="small" :type="row.mergeable ? 'success' : 'info'" effect="plain">
                  {{ row.mergeable ? '可以' : '不可' }}
                </el-tag>
              </template>
            </el-table-column>
            <el-table-column label="本地版本" width="100">
              <template #default="{ row }">{{ row.has_mine ? '在' : '无' }}</template>
            </el-table-column>
            <el-table-column label="服务端" width="110">
              <template #default="{ row }">
                {{ row.has_theirs ? `r${row.theirs_rev}` : '已删除' }}
              </template>
            </el-table-column>
          </el-table>
        </el-card>

        <ConflictResolver
          v-if="sides && current"
          :info="current"
          :sides="sides"
          :busy="resolving"
          :solved-count="solved"
          :total="solved + store.conflicts.length"
          @resolve="onResolve"
        />
      </template>
    </template>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';

import type { ConflictInfo, ConflictResolution, ConflictSides } from '@shared/dto';

import ConflictResolver from '@/components/ConflictResolver.vue';
import { useAppStore } from '@/stores/app';

const store = useAppStore();
const selected = ref('');
const sides = ref<ConflictSides | null>(null);
const resolving = ref(false);
const solved = ref(0);

const current = computed<ConflictInfo | null>(
  () => store.conflicts.find((c) => c.path === selected.value) ?? null,
);

async function reload(): Promise<void> {
  await store.refreshConflicts();
  // 列表变了就重新选一个（优先保留当前选中项）
  if (store.conflicts.length === 0) {
    selected.value = '';
    sides.value = null;
    return;
  }
  if (!store.conflicts.some((c) => c.path === selected.value)) {
    selected.value = store.conflicts[0]!.path;
  }
}

watch(selected, async (p) => {
  sides.value = null;
  if (!p) return;
  const r = await store.run(() => store.loadConflictSides(p));
  if (r) sides.value = r;
});

async function onPick(row: ConflictInfo | null): Promise<void> {
  if (row) selected.value = row.path;
}

async function onResolve(choice: ConflictResolution, content?: string): Promise<void> {
  const path = selected.value;
  if (!path) return;
  resolving.value = true;
  try {
    const ok = await store.resolveConflict(path, choice, content);
    if (ok) {
      solved.value += 1;
      await reload();
      await store.refreshStatus();
    }
  } finally {
    resolving.value = false;
  }
}

onMounted(reload);
</script>
