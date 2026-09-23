<template>
  <div class="page">
    <div class="flex" style="margin-bottom: 12px">
      <div>
        <h3 class="page-title">设置</h3>
        <p class="page-sub">
          配置落 <span class="mono">~/.b-artifact/config.json</span>（`B_ARTIFACT_HOME` 可重定向）
        </p>
      </div>
      <div class="spacer" />
      <el-button
        type="primary"
        size="small"
        :loading="saving"
        data-testid="settings-save"
        @click="save"
      >
        保存
      </el-button>
    </div>

    <el-card shadow="never" style="margin-bottom: 12px" data-testid="settings-general">
      <template #header><span>传输与忽略规则</span></template>
      <el-form label-width="120px" style="max-width: 620px">
        <el-form-item label="传输并发">
          <el-input-number
            v-model="form.concurrency"
            :min="1"
            :max="32"
            data-testid="settings-concurrency"
          />
          <span class="muted" style="margin-left: 10px; font-size: 12px">
            同时上传/下载的文件数（1–32）
          </span>
        </el-form-item>
        <el-form-item label="检出目录的默认路径">
          <div class="flex" style="width: 100%">
            <el-input
              v-model="form.defaultCheckoutParent"
              data-testid="settings-checkout-dir"
              placeholder="/path/to/workspaces"
              class="mono"
            />
            <el-button data-testid="settings-checkout-browse" @click="pickCheckoutDir">浏览…</el-button>
          </div>
          <div class="muted" style="font-size: 12px; margin-top: 6px">
            仓库检出时的默认目录 = 这个路径 + 仓库名（例：<span class="mono">{{
              form.defaultCheckoutParent || '/path/to/workspaces'
            }}/art</span>）；检出页仍可临时改。
          </div>
        </el-form-item>
      </el-form>
    </el-card>

    <el-card shadow="never" style="margin-bottom: 12px" data-testid="settings-cache">
      <template #header>
        <div class="flex">
          <span>全局 blob 缓存</span>
          <div class="spacer" />
          <el-button size="small" :loading="statsLoading" data-testid="cache-refresh" @click="loadStats">
            刷新
          </el-button>
        </div>
      </template>

      <el-descriptions :column="2" border size="small" style="max-width: 720px">
        <el-descriptions-item label="缓存目录" :span="2">
          <span class="mono">{{ stats?.dir ?? form.cacheDir ?? '-' }}</span>
        </el-descriptions-item>
        <el-descriptions-item label="已缓存 blob">
          <span data-testid="cache-blobs">{{ stats?.blobs ?? '-' }}</span> 个
        </el-descriptions-item>
        <el-descriptions-item label="占用空间">
          <span data-testid="cache-bytes">{{ stats ? formatBytes(stats.bytes) : '-' }}</span>
        </el-descriptions-item>
        <el-descriptions-item label="残留临时文件">
          <span data-testid="cache-tmp">{{ stats?.tmpFiles ?? '-' }}</span> 个 ·
          {{ stats ? formatBytes(stats.tmpBytes) : '-' }}
        </el-descriptions-item>
        <el-descriptions-item label="最近一次清理">
          <span data-testid="cache-last-clear">{{ lastClearText }}</span>
        </el-descriptions-item>
      </el-descriptions>

      <div class="flex" style="margin-top: 12px">
        <el-button
          size="small"
          :disabled="!stats?.tmpFiles"
          :loading="clearing"
          data-testid="cache-clear-tmp"
          @click="clear('tmp')"
        >
          清理残留临时文件
        </el-button>
        <el-button
          type="danger"
          plain
          size="small"
          :loading="clearing"
          data-testid="cache-clear-all"
          @click="clear('all')"
        >
          清空整个缓存
        </el-button>
        <el-button size="small" data-testid="cache-reveal" @click="reveal">
          在文件管理器中打开
        </el-button>
      </div>

      <el-alert
        type="info"
        :closable="false"
        show-icon
        style="margin-top: 12px"
        title="清理是安全的：工作副本的 pristine 是与缓存硬链接的另一份目录项，删掉缓存只是下次需要重新下载，不会损坏工作副本。"
      />
    </el-card>

  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { ElMessageBox } from 'element-plus';

import type { CacheClearResult, CacheStats } from '@shared/dto';

import { api } from '@/api';
import { useAppStore } from '@/stores/app';
import { formatBytes } from '@/utils/format';

const store = useAppStore();
const form = ref({ concurrency: 4, cacheDir: '', defaultCheckoutParent: '' });
const saving = ref(false);
const stats = ref<CacheStats | null>(null);
const statsLoading = ref(false);
const clearing = ref(false);
const lastClear = ref<CacheClearResult | null>(null);

const lastClearText = computed(() => {
  const r = lastClear.value;
  if (!r) return '本次会话还没清理过';
  return `删除 ${r.removed} 个文件 · 释放 ${formatBytes(r.bytes)}`;
});

function syncForm(): void {
  const c = store.config;
  form.value = {
    concurrency: c?.concurrency ?? 4,
    cacheDir: c?.cacheDir ?? '',
    defaultCheckoutParent: c?.defaultCheckoutParent ?? '',
  };
}

async function loadStats(): Promise<void> {
  statsLoading.value = true;
  try {
    const r = await store.run(() => api.cacheStats());
    if (r) stats.value = r;
  } finally {
    statsLoading.value = false;
  }
}

async function save(): Promise<void> {
  saving.value = true;
  try {
    const r = await store.run(
      () =>
        api.setConfig({
          concurrency: form.value.concurrency,
          defaultCheckoutParent: form.value.defaultCheckoutParent,
        }),
      '设置已保存',
    );
    if (r) {
      // 配置是主进程的事实源，保存后用返回值回填（别让界面自己拼一份不一样的）
      store.config = r;
      syncForm();
    }
  } finally {
    saving.value = false;
  }
}

async function clear(scope: 'tmp' | 'all'): Promise<void> {
  if (scope === 'all') {
    try {
      await ElMessageBox.confirm(
        '清空整个 blob 缓存？已检出的工作副本不受影响（pristine 是硬链接），只是下次需要重新下载。',
        '清空缓存',
        { type: 'warning', confirmButtonText: '清空', cancelButtonText: '取消' },
      );
    } catch {
      return;
    }
  }
  clearing.value = true;
  try {
    const r = await store.run(
      () => api.clearCache(scope === 'tmp'),
      scope === 'tmp' ? '残留临时文件已清理' : '缓存已清空',
    );
    if (r) lastClear.value = r;
    await loadStats();
  } finally {
    clearing.value = false;
  }
}

/**
 * 选检出默认路径。
 *
 * 这是**两段式**：主进程只负责弹系统目录选择框并返回路径（`app:pickDir` 不再自作主张写配置），
 * 写不写、写什么由这个表单决定 —— 用户点"保存"才算数，与页面别的字段一致。
 */
async function pickCheckoutDir(): Promise<void> {
  const picked = await store.run(() => api.pickDir('选择检出目录的默认路径'));
  if (picked) form.value.defaultCheckoutParent = picked;
}

async function reveal(): Promise<void> {
  const dir = stats.value?.dir ?? form.value.cacheDir;
  if (dir) await store.run(() => api.revealPath(dir));
}

onMounted(async () => {
  if (!store.config) await new Promise((r) => setTimeout(r, 50));
  syncForm();
  await loadStats();
});
</script>
