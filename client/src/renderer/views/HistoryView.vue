<template>
  <div class="page">
    <el-empty v-if="!store.wc" description="还没有打开工作副本（历史按工作副本所属仓库展示）" />

    <template v-else>
      <div class="flex" style="margin-bottom: 12px">
        <div>
          <h3 class="page-title">历史</h3>
          <p class="page-sub">
            <span class="mono">{{ store.wc.repo }}</span> 的修订记录 ·
            {{ summaryText }} · 选中某条可浏览当时的文件并下载旧版本
          </p>
        </div>
        <div class="spacer" />
        <el-button size="small" :loading="loading" data-testid="history-refresh" @click="reload">
          刷新
        </el-button>
      </div>

      <div class="layout">
        <el-card shadow="never" class="list-card" data-testid="history-card">
          <template #header>
            <div class="flex">
              <span>修订列表</span>
              <div class="spacer" />
              <span class="muted" style="font-size: 12px" data-testid="history-count">
                {{ revisions.length }} 条
              </span>
            </div>
          </template>

          <el-table
            :data="revisions"
            size="small"
            height="460"
            highlight-current-row
            :current-row-key="selectedRev"
            row-key="rev"
            data-testid="history-table"
            @current-change="onPick"
          >
            <!-- 表头写全称：「r」单独摆在那儿没人看得懂（真实反馈），单元格里已经是 r5 这种编号 -->
            <el-table-column label="修订" width="80" align="right">
              <template #default="{ row }">
                <span class="mono" :data-testid="`rev-${row.rev}`">{{ revisionLabel(row.rev) }}</span>
              </template>
            </el-table-column>
            <el-table-column label="说明" min-width="132">
              <template #default="{ row }">
                <div>{{ revisionMessage(row) }}</div>
                <div class="muted" style="font-size: 12px">{{ revisionSubtitle(row) }}</div>
              </template>
            </el-table-column>
            <el-table-column label="时间" width="132">
              <template #default="{ row }">{{ formatTime(row.created_at) }}</template>
            </el-table-column>
            <el-table-column label="增量" width="86" align="right">
              <template #default="{ row }">
                <span :style="{ color: deltaColor(row.byte_delta) }">{{ formatByteDelta(row.byte_delta) }}</span>
              </template>
            </el-table-column>
          </el-table>

          <div class="flex" style="margin-top: 10px">
            <el-button
              v-if="moreOffset !== null"
              size="small"
              :loading="loading"
              data-testid="history-load-more"
              @click="loadMore"
            >
              加载更早的修订
            </el-button>
            <span v-else class="muted" style="font-size: 12px" data-testid="history-end">
              已经到最早一条
            </span>
          </div>
        </el-card>

        <el-card shadow="never" class="detail-card" data-testid="history-detail">
          <template #header>
            <div class="flex">
              <span>
                修订内容
                <span class="mono" style="margin-left: 6px" data-testid="detail-rev">
                  {{ selectedRev > 0 ? revisionLabel(selectedRev) : '（未选择）' }}
                </span>
              </span>
              <div class="spacer" />
              <!-- 目录既可点按钮选（原生对话框），也可直接粘贴路径——后者也让冒烟能自动化 -->
              <el-input
                v-model="targetDir"
                size="small"
                class="mono dir-input"
                data-testid="download-dir"
                placeholder="下载到哪个目录"
              />
              <el-button size="small" data-testid="download-target" @click="pickTarget">浏览…</el-button>
            </div>
          </template>

          <el-breadcrumb v-if="selectedRev > 0" separator="/" style="margin-bottom: 8px">
            <el-breadcrumb-item>
              <a @click.prevent="goto('')">/</a>
            </el-breadcrumb-item>
            <el-breadcrumb-item v-for="(seg, i) in segments" :key="i">
              <a @click.prevent="goto(segments.slice(0, i + 1).join('/'))">{{ seg }}</a>
            </el-breadcrumb-item>
          </el-breadcrumb>

          <el-alert
            v-if="selectedRev > 0 && selectedRev !== store.wc.rev"
            type="warning"
            :closable="false"
            show-icon
            style="margin-bottom: 10px"
            data-testid="history-slow-hint"
          >
            浏览历史修订需要服务端重建当时的目录树，大仓库上会明显变慢（§15.2 `list_dir_at_rev`）。
            只按需展开当前这一层，不做预取。
          </el-alert>

          <el-table
            v-if="selectedRev > 0"
            :data="entries"
            size="small"
            height="380"
            v-loading="detailLoading"
            data-testid="detail-table"
          >
            <el-table-column label="名称" min-width="240">
              <template #default="{ row }">
                <el-link
                  v-if="row.kind === 'dir'"
                  type="primary"
                  :underline="false"
                  @click="goto(row.path)"
                >
                  {{ basename(row.path) }}
                </el-link>
                <span v-else class="mono">{{ basename(row.path) }}</span>
              </template>
            </el-table-column>
            <el-table-column label="类型" width="70">
              <template #default="{ row }">{{ row.kind === 'dir' ? '目录' : '文件' }}</template>
            </el-table-column>
            <el-table-column label="大小" width="100" align="right">
              <template #default="{ row }">
                {{ row.kind === 'dir' ? '-' : formatBytes(row.size) }}
              </template>
            </el-table-column>
            <el-table-column label="最后变更" width="100" align="right">
              <template #default="{ row }">{{ revisionLabel(row.changed_rev) }}</template>
            </el-table-column>
            <el-table-column label="操作" width="130" fixed="right">
              <template #default="{ row }">
                <el-button
                  v-if="row.kind === 'file'"
                  text
                  type="primary"
                  size="small"
                  :data-testid="`download-${row.path}`"
                  @click="download(row)"
                >
                  下载此版本
                </el-button>
              </template>
            </el-table-column>
          </el-table>

          <el-empty v-else description="选择左侧任一修订查看当时的文件" />
        </el-card>
      </div>
    </template>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';

import type { DownloadOutcome, LogEntry, TreeEntryDto } from '@shared/dto';

import { api } from '@/api';
import { useAppStore } from '@/stores/app';
import { basename, formatBytes, formatTime } from '@/utils/format';
import {
  deltaColor,
  formatByteDelta,
  LOG_PAGE_SIZE,
  nextOffset,
  revisionLabel,
  revisionMessage,
  revisionSubtitle,
  summarizeRevisions,
} from '@/utils/history';

const store = useAppStore();
const revisions = ref<LogEntry[]>([]);
const loading = ref(false);
const moreOffset = ref<number | null>(null);

const selectedRev = ref(0);
const prefix = ref('');
const entries = ref<TreeEntryDto[]>([]);
const detailLoading = ref(false);
const targetDir = ref('');
const lastDownload = ref<DownloadOutcome | null>(null);

const segments = computed(() => prefix.value.split('/').filter(Boolean));
const summary = computed(() => summarizeRevisions(revisions.value));
const summaryText = computed(
  () =>
    `${summary.value.count} 条修订 · ${summary.value.authors.length} 位作者 · 累计 ${formatByteDelta(summary.value.added)}`,
);

async function loadPage(offset: number): Promise<void> {
  // 没有工作副本就没有仓库可看（主进程也会以 NO_WORKING_COPY 拒绝）
  if (!store.wc) return;
  loading.value = true;
  try {
    // 历史是"当前工作副本所在仓库"的视角 —— 仓库由这里显式传上去（服务端不再猜）
    const repo = store.wc?.repo;
    if (!repo) return;
    const r = await store.run(() => api.log(repo, { limit: LOG_PAGE_SIZE, offset }));
    if (!r) return;
    revisions.value = offset === 0 ? r.items : [...revisions.value, ...r.items];
    moreOffset.value = nextOffset(r.items.length, offset);
    if (offset === 0 && r.items.length > 0 && selectedRev.value === 0) {
      selectedRev.value = r.items[0]!.rev;
    }
  } finally {
    loading.value = false;
  }
}

async function reload(): Promise<void> {
  selectedRev.value = 0;
  prefix.value = '';
  entries.value = [];
  await loadPage(0);
}

async function loadMore(): Promise<void> {
  if (moreOffset.value === null) return;
  await loadPage(moreOffset.value);
}

async function loadDetail(): Promise<void> {
  if (selectedRev.value <= 0) return;
  detailLoading.value = true;
  try {
    const repo = store.wc?.repo;
    if (!repo) return;
    const r = await store.run(() => api.treeAt(repo, selectedRev.value, prefix.value, 1));
    entries.value = (r?.items ?? []).filter((i) => i.path !== prefix.value);
  } finally {
    detailLoading.value = false;
  }
}

function onPick(row: LogEntry | null): void {
  if (!row) return;
  selectedRev.value = row.rev;
  prefix.value = '';
}

function goto(path: string): void {
  prefix.value = path;
  void loadDetail();
}

async function pickTarget(): Promise<void> {
  const picked = await store.run(() => api.pickDir('选择下载目录'));
  if (picked) targetDir.value = picked;
}

async function download(row: TreeEntryDto): Promise<void> {
  if (!targetDir.value) {
    store.note('请先设置下载目录', 'err');
    return;
  }
  const r = await store.run(() =>
    api.downloadRevision(store.wc?.repo ?? '', row.path, selectedRev.value, targetDir.value),
  );
  if (!r) return;
  lastDownload.value = r;
  store.note(`已保存 ${r.saved}（r${r.rev} · ${formatBytes(r.size)}）`);
}

watch(selectedRev, () => void loadDetail());
watch(prefix, () => void 0);

onMounted(reload);
</script>

<style scoped>
.dir-input {
  width: 240px;
}
.layout {
  display: grid;
  /* 左列略宽：修订列表有四列，太窄会把最后一列表头挤掉（真实截图里出现过） */
  grid-template-columns: minmax(0, 1.05fr) minmax(0, 1fr);
  gap: 12px;
  align-items: start;
}
@media (max-width: 1100px) {
  .layout {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
