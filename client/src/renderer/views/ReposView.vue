<template>
  <div class="page">
    <div class="flex" style="margin-bottom: 12px">
      <div>
        <h3 class="page-title">仓库</h3>
        <p class="page-sub">
          选择仓库与目标目录后检出；勾选目录即可只取这些目录（部分检出，其余留在服务端，§6.5）
        </p>
      </div>
      <div class="spacer" />
      <el-button size="small" :loading="store.busy" data-testid="repos-refresh" @click="store.refreshRepos()">
        刷新
      </el-button>
    </div>

    <el-card shadow="never" style="margin-bottom: 12px">
      <el-table
        :data="store.repos"
        size="small"
        highlight-current-row
        :current-row-key="selected"
        row-key="name"
        data-testid="repo-table"
        @current-change="onPick"
      >
        <el-table-column prop="name" label="仓库" min-width="180">
          <template #default="{ row }">
            <span class="mono">{{ row.name }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="description" label="说明" min-width="180" />
        <el-table-column label="HEAD" width="90">
          <template #default="{ row }">r{{ row.head_rev }}</template>
        </el-table-column>
        <el-table-column label="我的角色" width="110">
          <template #default="{ row }">
            <el-tag size="small" effect="plain">{{ row.my_role }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="权限" width="110">
          <template #default="{ row }">
            <span class="mono">
              {{ row.my_permissions.read ? 'r' : '-' }}{{ row.my_permissions.write ? 'w' : '-' }}{{
                row.my_permissions.admin ? 'a' : '-'
              }}
            </span>
          </template>
        </el-table-column>
      </el-table>
    </el-card>

    <el-card shadow="never" style="margin-bottom: 12px">
      <el-form label-width="110px">
        <el-form-item label="选中仓库">
          <span class="mono" data-testid="checkout-repo">{{ selected || '（未选择）' }}</span>
        </el-form-item>
        <el-form-item label="检出目录">
          <div class="flex" style="width: 100%">
            <el-input
              v-model="dir"
              data-testid="checkout-dir"
              placeholder="/path/to/workspace"
              class="mono"
              @input="dirEdited = true"
            />
            <el-button data-testid="checkout-browse" @click="browse">浏览…</el-button>
          </div>
          <div class="muted mono" style="font-size: 12px; margin-top: 4px" data-testid="checkout-dir-hint">
            {{ dir || '（未设置）' }}
          </div>
        </el-form-item>
        <el-form-item label="部分检出">
          <FileTree
            v-if="selected"
            ref="treeRef"
            v-model="sparsePrefixes"
            :repo="selected"
          />
          <span v-else class="muted" style="font-size: 13px">先选中仓库，再勾选要检出的目录</span>
        </el-form-item>
        <el-form-item>
          <el-button
            type="primary"
            :loading="store.busy"
            data-testid="checkout-submit"
            @click="submit"
          >
            检出
          </el-button>
        </el-form-item>
      </el-form>
    </el-card>

    <el-card shadow="never">
      <template #header>
        <span>最近打开的工作副本</span>
      </template>
      <!-- 同「打开已有工作副本」弹窗的处理：整行可点 + 操作列钉右 + 目录列省略号 -->
      <el-table
        :data="store.config?.recent ?? []"
        size="small"
        data-testid="recent-table"
        @row-click="(row: RecentEntry) => openRecent(row.dir)"
      >
        <el-table-column label="目录" min-width="260" show-overflow-tooltip>
          <template #default="{ row }">
            <span class="mono">{{ row.dir }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="repo" label="仓库" width="140" />
        <el-table-column label="最近打开" width="120">
          <template #default="{ row }">{{ formatRelative(row.lastOpenedAt) }}</template>
        </el-table-column>
        <el-table-column label="操作" width="90" fixed="right">
          <template #default="{ row }">
            <el-button
              text
              type="primary"
              size="small"
              data-testid="recent-open"
              @click.stop="openRecent(row.dir)"
            >
              打开
            </el-button>
          </template>
        </el-table-column>
      </el-table>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { ref, watch } from 'vue';

import type { RecentEntry, RepoSummary } from '@shared/dto';

import FileTree from '@/components/FileTree.vue';
import { api } from '@/api';
import { useAppStore } from '@/stores/app';
import { formatRelative } from '@/utils/format';

/** 检出 / 打开已有副本成功后，通知外层切到「工作副本」页（用户下一步一定是在那儿干活）。 */
const emit = defineEmits<{ (e: 'goto', tab: string): void }>();

const store = useAppStore();
const selected = ref('');
const dir = ref('');
const sparsePrefixes = ref<string[]>([]);
const treeRef = ref<InstanceType<typeof FileTree>>();

/**
 * 选中仓库后算出默认目录 = **设置里的「检出目录的默认路径」+ 仓库名**。
 *
 * 两个细节：
 *   - **用户自己改过就不再覆盖**（`dirEdited`）：他在检出页粘了一个路径，切仓库时把它冲掉
 *     是最惹人烦的一类交互；
 *   - 没配默认路径时留空（而不是沿用上一个仓库的路径），提示行会显示"（未设置）"。
 */
const dirEdited = ref(false);

watch(selected, (name, prev) => {
  if (prev !== undefined && name !== prev) {
    // 换仓库了：勾选的前缀属于上一个仓库，必须清掉（否则会拿着别的仓库的路径去检出）
    sparsePrefixes.value = [];
    treeRef.value?.clearChecked?.();
  }
  if (!name || dirEdited.value) return;
  const parent = store.config?.defaultCheckoutParent || '';
  dir.value = parent ? joinPath(parent, name) : '';
});


function joinPath(parent: string, name: string): string {
  const sep = parent.includes('\\') ? '\\' : '/';
  return parent.endsWith(sep) ? `${parent}${name}` : `${parent}${sep}${name}`;
}

function onPick(row: RepoSummary | null): void {
  selected.value = row?.name ?? '';
}

async function browse(): Promise<void> {
  const picked = await store.run(() => api.pickDir('选择检出目录'));
  if (picked) {
    dir.value = picked;
    dirEdited.value = true; // 用户明确选了目录，之后切仓库不再覆盖
  }
}

async function submit(): Promise<void> {
  if (await store.checkout(selected.value, dir.value, sparsePrefixes.value)) {
    emit('goto', 'workspace');
  }
}

async function openRecent(path: string): Promise<void> {
  if (await store.openWorkingCopy(path)) {
    emit('goto', 'workspace');
  }
}
</script>
