<template>
  <div class="ws-tree" data-testid="wc-tree">
    <div v-if="nodes.length === 0" class="empty muted" data-testid="wc-tree-empty">
      {{ emptyText }}
    </div>

    <el-tree
      v-else
      ref="treeRef"
      class="tree"
      :data="nodes"
      :props="treeProps"
      node-key="path"
      show-checkbox
      highlight-current
      :expand-on-click-node="false"
      :check-on-click-leaf="false"
      :default-expanded-keys="expanded"
      :default-checked-keys="checked"
      :filter-node-method="filterNode"
      data-testid="wc-tree-nodes"
      @check="onCheck"
      @node-click="onCurrent"
      @current-change="onCurrent"
      @node-expand="onNodeExpand"
      @node-collapse="onNodeCollapse"
      @node-contextmenu="onContextMenu"
    >
      <template #default="{ data }">
        <!-- 双击目录 = 展开 / 收起。el-tree 没有 dblclick 事件，所以在自己的插槽内容上监听；
             单击仍然是"选中"（expand-on-click-node 保持 false），两种手势不打架。 -->
        <span class="node" @dblclick.stop="onDblClick(data, $event)">
          <el-icon v-if="data.kind === 'dir'"><Folder /></el-icon>
          <el-icon v-else><Document /></el-icon>
          <span class="mono name" :data-testid="`wc-node-${data.path}`">{{ data.name }}</span>

          <el-tag
            v-if="data.kind === 'dir' && data.changed > 0"
            size="small"
            effect="plain"
            type="warning"
            :data-testid="`wc-dir-count-${data.path}`"
          >
            变更 {{ data.changed }}
          </el-tag>
          <el-tag
            v-else-if="data.kind === 'file'"
            size="small"
            effect="plain"
            :type="tagOf(data.status)"
            :data-testid="`wc-status-${data.path}`"
          >
            {{ labelOf(data.status) }}
          </el-tag>

          <el-tag
            v-if="data.locked"
            size="small"
            effect="plain"
            :type="data.locked === 'mine' ? 'success' : 'danger'"
            :data-testid="`wc-lock-${data.path}`"
          >
            {{ data.locked === 'mine' ? '已锁·我' : `已锁·${data.lockOwner}` }}
          </el-tag>

          <!-- 目录：把子树里的锁数出来。用户最需要知道的两件事——"为什么加不上锁"
               （他人锁）与"我该去哪儿解锁"（我的锁）——都在这两个标记里 -->
          <el-tag
            v-if="data.kind === 'dir' && data.locksOther > 0"
            size="small"
            effect="plain"
            type="danger"
            :data-testid="`wc-dirlock-other-${data.path}`"
          >
            他人锁 {{ data.locksOther }}
          </el-tag>
          <el-tag
            v-if="data.kind === 'dir' && data.locksMine > 0"
            size="small"
            effect="plain"
            type="success"
            :data-testid="`wc-dirlock-mine-${data.path}`"
          >
            已锁 {{ data.locksMine }}
          </el-tag>
        </span>
      </template>
    </el-tree>
  </div>
</template>

<script setup lang="ts">
/**
 * 工作副本目录树（§6.5 M4.8）。
 *
 * 组件刻意"笨"：树的构建、过滤、默认展开/勾选都由父组件经 `utils/tree.ts` 算好传进来，
 * 这里只负责渲染与把用户的勾选/当前节点回抛。这样 `utils/tree.ts` 的全部规则都能被单测钉住，
 * 而组件本身只跑 E2E。
 *
 * 两个交互约定：
 *   - **勾选 = 提交选择集**（含未纳管文件——它们默认不勾，但勾上就算新增）；
 *   - **高亮 = 当前节点**，新建/删除/加锁这些"作用在单个路径上"的动作看它。
 */
import { computed, nextTick, ref, watch } from 'vue';
import { Document, Folder } from '@element-plus/icons-vue';
import type { ElTree } from 'element-plus';

import type { StatusCode } from '@shared/dto';

import { STATUS_LABEL, STATUS_TAG, type TagType } from '@/utils/status';
import { expandKeys, type TreeNode } from '@/utils/tree';

const props = defineProps<{
  nodes: readonly TreeNode[];
  /**
   * **当前**勾选集（父组件持有）。不是"只算一次的初始值"——
   * el-tree 每次 `data` 变化都会重建节点、勾选状态全部丢失，只能靠 `default-checked-keys`
   * 复原，所以父组件必须把"当前勾选"实时传进来，否则自动同步一刷新就把用户的勾选冲掉了。
   */
  initialChecked?: readonly string[];
  /** 关键词过滤（按名字匹配）。 */
  filter?: string;
  emptyText?: string;
}>();

const emit = defineEmits<{
  (e: 'update:checked', paths: string[]): void;
  (e: 'update:current', path: string | null, node: TreeNode | null): void;
  /** 右键某个节点：视图据此弹菜单（坐标是视口坐标）。**锁动作的唯一入口**。 */
  (e: 'context-menu', payload: { path: string; kind: 'file' | 'dir'; x: number; y: number }): void;
}>();

const treeRef = ref<InstanceType<typeof ElTree>>();
const treeProps = { label: 'name', children: 'children' };

/**
 * 展开集合（**受控**）。
 *
 * 为什么要受控：`el-tree` 在 `data` 引用变化时会清空 nodesMap、**重建全部节点**，
 * 展开状态随之丢失 —— 而它重建时只会按 `default-expanded-keys` 复原一遍
 * （`Node` 构造时若 key 命中就 `expand()`）。所以这里维护一份"应该展开的集合"，
 * 并监听 `node-expand` / `node-collapse` 跟住用户的手动操作；重建时自动复原。
 *
 * 勾选同理（见 `checked`）：两者都是"每次 data 变化就丢"的状态，都必须受控。
 */
const expanded = ref<string[]>([...expandKeys(props.nodes)]);
const checked = computed(() => [...(props.initialChecked ?? [])]);

/** 用户手动展开 / 收起：把集合同步上，这样重建之后不会被"顶回去"。 */
function onNodeExpand(data: TreeNode): void {
  if (!expanded.value.includes(data.path)) expanded.value = [...expanded.value, data.path];
}

function onNodeCollapse(data: TreeNode): void {
  if (expanded.value.includes(data.path)) {
    expanded.value = expanded.value.filter((p) => p !== data.path);
  }
}

/**
 * 已经自动展开过的目录（含挂载时就展开的那批）。
 *
 * 自动同步会把新出现的目录塞进树里（用户在系统文件夹里新建了一个目录），而它不在
 * 初始集合里 —— 不补的话就是"文件明明变了，界面却什么都没露出来"。
 * 记在 `autoExpanded` 里是为了**只补一次**：用户之后手动把它折叠起来，
 * 不该被反复顶开。
 */
const autoExpanded = new Set<string>(expanded.value);

/**
 * 把"新出现的、有变更的"目录补进展开集合。
 *
 * 注意这里**不动 DOM 也不碰内部 store**，只改集合：改完之后的同一次刷新里，
 * `el-tree` 就会按新的 `default-expanded-keys` 重建 —— 顺序天然正确
 * （本组件的 watcher 先于 el-tree 的 watcher 执行），比"重建完再回头补展开"可靠得多。
 */
function noteNewChangedDirs(list: readonly TreeNode[]): void {
  const add: string[] = [];
  const walk = (items: readonly TreeNode[]): void => {
    for (const n of items) {
      if (n.kind !== 'dir') continue;
      if (n.changed > 0 && !autoExpanded.has(n.path)) {
        autoExpanded.add(n.path);
        add.push(n.path);
      }
      walk(n.children);
    }
  };
  walk(list);
  if (add.length > 0) expanded.value = [...expanded.value, ...add];
}

watch(() => props.nodes, noteNewChangedDirs);

watch(
  () => props.filter,
  (v) => treeRef.value?.filter(v ?? ''),
);

function filterNode(value: string, data: TreeNode): boolean {
  if (!value) return true;
  return data.name.toLowerCase().includes(value.toLowerCase());
}

/**
 * 模板里 `#default="{ data }"` 的 data 是 `any`，直接拿它索引
 * `Record<StatusCode, …>` 会在 `noImplicitAny` 下报错（`StatusTable` 踩过同一个坑）。
 * 用带类型的取值函数收口，映射也集中在一处。
 */
function tagOf(status: StatusCode | null): TagType {
  return status ? STATUS_TAG[status] : 'info';
}

function labelOf(status: StatusCode | null): string {
  return status ? STATUS_LABEL[status] : '';
}

function onCheck(): void {
  const keys = (treeRef.value?.getCheckedKeys?.() ?? []) as string[];
  emit('update:checked', keys);
}

/**
 * 当前节点 = "加锁/解锁/新建/删除"的作用目标。
 *
 * 用 `node-click` 而不是只靠 `current-change`：Element Plus 的**复选框带
 * `@click.stop`**，点复选框根本不会冒泡到节点内容区 —— 于是"用复选框选中一个目录、
 * 再点加锁"会作用在**上一次点过的那个文件**上（用户看到的就是"选了目录却锁了某个文件"）。
 * `node-click` 在点击节点内容区（标签 / 图标）时都会触发，语义与用户直觉一致。
 *
 * 手势分工（三条都显式写死，不依赖 Element Plus 的默认值）：
 *
 *   - 点**节点标签** = 只"选中"（`expand-on-click-node=false`）；
 *   - 点**复选框** = 勾选（`check-on-click-node=false`）；
 *   - **双击目录** = 展开 / 收起。
 *
 * ⚠️ `checkOnClickLeaf` 的**默认值是 true** —— 只关 `check-on-click-node` 是不够的：
 * 那样点**文件**标签仍会悄悄切换它的勾选（目录因为是"非叶子"才不受影响，行为不对称）。
 * 提交步骤里"点标签选中文件"因此把勾选取消掉了、提交集变空（E2E 抓出来的真 bug）。
 */
function onCurrent(data: TreeNode | null): void {
  emit('update:current', data?.path ?? null, data);
}

/** el-tree 的节点实例（只在内部 API 上有，取不到就静默跳过）。 */
function nodeInstance(path: string): { expanded?: boolean; expand?: () => void; collapse?: () => void } | null {
  const store = (
    treeRef.value as unknown as {
      store?: { nodesMap?: Record<string, { expanded?: boolean; expand?: () => void; collapse?: () => void }> };
    }
  )?.store;
  return store?.nodesMap?.[path] ?? null;
}

/** 右键 → 交给父组件弹菜单（`@node-contextmenu` 会 preventDefault 掉浏览器默认菜单）。 */
function onContextMenu(event: MouseEvent, data: TreeNode): void {
  emit('context-menu', { path: data.path, kind: data.kind, x: event.clientX, y: event.clientY });
}

/**
 * 双击目录 → 展开 / 收起（文件双击不做事）。
 *
 * 这里用节点实例的 `expand()/collapse()`（公开 API 没有"按路径展开"）——
 * 安全前提是**用户刚点过这个节点，它必然已渲染**，`nodesMap[path]` 一定在。
 *
 * **走"点展开箭头"这条公开交互路径**（从双击事件里找到同一个节点上的箭头再点它）：
 *   - 它就是用户点箭头时执行的同一段逻辑，行为天然一致（含 `node-expand/collapse` 事件）；
 *   - 不依赖 el-tree 的内部 `store.nodesMap`（那是私有 API，生产构建下不该被指望）。
 *
 * 兜底才用内部节点实例；两条路径都**同时更新自己的展开集合** —— 因为
 * `Node.collapse()/expand()` 是纯状态赋值、**不发事件**，只靠事件同步的话，双击收起的目录
 * 会在下一次数据重建（自动同步 / 锁轮询）时被 `default-expanded-keys` 又展开回来
 * （表现是"双击收起没反应"，真 bug）。
 */
function onDblClick(data: TreeNode, e: MouseEvent): void {
  if (data.kind !== 'dir') return;
  const caret = (e.target as HTMLElement | null)
    ?.closest?.('.el-tree-node__content')
    ?.querySelector<HTMLElement>('.el-tree-node__expand-icon');
  if (caret) {
    caret.click(); // 与用户点箭头完全同一条路径
    return;
  }
  const n = nodeInstance(data.path);
  const willCollapse = n?.expanded ?? expanded.value.includes(data.path);
  if (willCollapse) {
    expanded.value = expanded.value.filter((p) => p !== data.path);
    n?.collapse?.();
  } else {
    if (!expanded.value.includes(data.path)) {
      expanded.value = [...expanded.value, data.path];
    }
    n?.expand?.();
  }
}

/** 右键 → 交给父组件弹菜单（`@node-contextmenu` 已经 preventDefault 掉浏览器的默认菜单）。 */
/** 供父组件在还原/提交后收回勾选。 */
function clearChecked(): void {
  treeRef.value?.setCheckedKeys?.([]);
}
defineExpose({ clearChecked });
</script>

<style scoped>
.tree {
  max-height: 420px;
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
.name {
  margin-right: 2px;
}
.empty {
  padding: 24px;
  text-align: center;
  border: 1px dashed var(--ba-border);
  border-radius: 6px;
}
</style>
