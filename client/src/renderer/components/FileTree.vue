<template>
  <div class="file-tree" data-testid="file-tree">
    <div class="flex" style="margin-bottom: 8px">
      <el-input
        v-model="filter"
        size="small"
        placeholder="过滤已加载的目录名"
        data-testid="tree-filter"
        style="max-width: 240px"
      />
      <el-button size="small" :loading="busy" data-testid="tree-reload" @click="reload">重新加载</el-button>
      <div class="spacer" />
      <span class="muted" style="font-size: 12px">
        只显示你有读取权限的条目（服务端已按 ACL 过滤）
      </span>
    </div>

    <el-alert
      v-if="error"
      :title="error"
      type="error"
      :closable="false"
      show-icon
      style="margin-bottom: 8px"
      data-testid="tree-error"
    />

    <el-tree
      :key="treeKey"
      ref="treeRef"
      class="tree"
      :props="treeProps"
      :load="loadNode"
      :filter-node-method="filterNode"
      lazy
      node-key="path"
      show-checkbox
      check-strictly
      :expand-on-click-node="false"
      :default-expand-all="false"
      data-testid="tree-nodes"
      @check="onCheck"
    >
      <template #default="{ data }">
        <span class="node">
          <el-icon v-if="data.kind === 'dir'"><Folder /></el-icon>
          <el-icon v-else><Document /></el-icon>
          <span class="mono" :data-testid="`node-${data.path}`">{{ data.name }}</span>
        </span>
      </template>
    </el-tree>

    <div class="picked">
      <div class="muted" style="font-size: 12px; margin-bottom: 6px" data-testid="sparse-desc">
        {{ describeSparse(modelValue) }}
      </div>
      <div v-if="modelValue.length > 0" class="flex" style="flex-wrap: wrap">
        <el-tag
          v-for="p in modelValue"
          :key="p"
          class="picked-tag"
          size="small"
          closable
          data-testid="sparse-tag"
          @close="remove(p)"
        >
          {{ p }}
        </el-tag>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, watch } from 'vue';
import { Document, Folder } from '@element-plus/icons-vue';
import type { ElTree } from 'element-plus';

import type { TreeEntryDto } from '@shared/dto';

import { api } from '@/api';
import { useAppStore } from '@/stores/app';
import { describeSparse, normalizeSparse, removeSparse, toTreeNode, type TreeNode } from '@/utils/sparse';

const props = defineProps<{ repo: string; modelValue: string[]; rev?: number }>();
const emit = defineEmits<{ (e: 'update:modelValue', v: string[]): void }>();

const store = useAppStore();
const treeRef = ref<InstanceType<typeof ElTree>>();
const filter = ref('');
const busy = ref(false);
const error = ref('');
const treeKey = ref(0);

const treeProps = { label: 'name', isLeaf: 'leaf' };

watch(filter, (v) => treeRef.value?.filter(v));

function filterNode(value: string, data: TreeNode): boolean {
  if (!value) return true;
  return data.name.includes(value);
}

/** 懒加载一层：只拉某个前缀的直接子节点（大仓库上不做任何预取）。 */
async function loadNode(node: { level: number; data?: TreeNode }, resolve: (nodes: TreeNode[]) => void): Promise<void> {
  const prefix = node.level === 0 ? '' : (node.data?.path ?? '');
  busy.value = true;
  try {
    // **仓库由 props 给**（仓库页选中的仓库）——不依赖当前有没有打开工作副本
    const r = await store.run(() => api.treeAt(props.repo, props.rev ?? 0, prefix, 1));
    const items: TreeEntryDto[] = (r?.items ?? []).filter((i) => i.path !== prefix);
    resolve(items.map(toTreeNode));
  } finally {
    busy.value = false;
  }
}

/**
 * 重新加载：换 key 强制重建。
 *
 * 懒加载模式下树的数据由 `load` 回调自己管，`setData` 只对静态数据有意义；
 * 换 key 让组件整棵重建，是这里最省心也最不容易出错的刷新方式。
 */
async function reload(): Promise<void> {
  error.value = '';
  busy.value = true;
  treeKey.value += 1;
  await new Promise((r) => setTimeout(r, 0));
  busy.value = false;
}

function onCheck(): void {
  const keys = (treeRef.value?.getCheckedKeys?.() ?? []) as string[];
  // 规范化在这里做一次：父组件拿到的永远是"最小且无歧义"的集合
  emit('update:modelValue', normalizeSparse(keys));
}

function remove(path: string): void {
  const next = removeSparse(props.modelValue, path);
  // 标签删除后要把树上对应的勾去掉，否则界面上两处状态会打架
  treeRef.value?.setChecked?.(path, false, false);
  emit('update:modelValue', next);
}

/** 供父组件在"换仓库"时清空勾选。 */
function clearChecked(): void {
  for (const p of props.modelValue) treeRef.value?.setChecked?.(p, false, false);
}
defineExpose({ clearChecked });
</script>

<style scoped>
.tree {
  max-height: 280px;
  overflow: auto;
  border: 1px solid var(--ba-border);
  border-radius: 6px;
  padding: 6px;
}
.node {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}
.picked {
  margin-top: 10px;
}
.picked-tag {
  margin: 0 6px 6px 0;
}
</style>
