<template>
  <div class="page">
    <el-empty v-if="!store.wc" description="还没有打开工作副本">
      <el-button type="primary" data-testid="goto-repos" @click="$emit('goto', 'repos')">
        去仓库页检出
      </el-button>
      <el-button data-testid="open-existing-empty" @click="openDialog = true">打开已有副本</el-button>
    </el-empty>

    <template v-else>
      <div class="flex" style="margin-bottom: 12px">
        <div>
          <h3 class="page-title">
            {{ store.wc.repo }} <span class="muted mono">r{{ store.wc.rev }}</span>
          </h3>
          <p class="page-sub mono">{{ store.wc.root }}</p>
        </div>
        <div class="spacer" />
        <span class="muted" style="font-size: 12px; margin-right: 8px" data-testid="sync-state">
          {{ syncText }}
        </span>
        <el-button
          v-if="!store.wc.watching"
          size="small"
          :loading="store.busy"
          data-testid="wc-refresh"
          @click="store.refreshStatus(true)"
        >
          刷新状态
        </el-button>
        <el-button size="small" data-testid="wc-open-existing" @click="openDialog = true">
          打开已有副本
        </el-button>
        <el-button size="small" data-testid="wc-reveal" @click="reveal">打开目录</el-button>
        <el-button size="small" data-testid="wc-close" @click="store.closeWorkingCopy()">关闭</el-button>
      </div>

      <div class="flex" style="margin-bottom: 10px">
        <el-button
          size="small"
          type="primary"
          plain
          :loading="store.busy"
          data-testid="wc-update"
          @click="store.update()"
        >
          更新
        </el-button>
        <span class="muted" style="font-size: 12px" data-testid="update-hint">{{ updateHint }}</span>
        <el-divider direction="vertical" />
        <el-switch
          v-model="onlyChanged"
          size="small"
          active-text="只看变更"
          data-testid="wc-only-changed"
        />
        <el-input
          v-model="filter"
          size="small"
          placeholder="按名字过滤"
          data-testid="wc-tree-filter"
          style="max-width: 180px; margin-left: 8px"
        />
        <div class="spacer" />
        <el-button size="small" data-testid="wc-ignore" @click="openIgnore">忽略规则</el-button>
        <el-button size="small" :loading="store.busy" data-testid="wc-new-dir" @click="doNewDir">
          新建目录
        </el-button>
        <el-button
          size="small"
          :disabled="selection.length === 0"
          :loading="store.busy"
          data-testid="wc-delete"
          @click="doDelete"
        >
          删除{{ selection.length > 0 ? `（${selection.length}）` : '' }}
        </el-button>
        <el-button size="small" :disabled="!selection.length" data-testid="wc-revert" @click="doRevert">
          还原{{ selection.length > 0 ? `（${selection.length}）` : '' }}
        </el-button>
      </div>

      <!-- 动作目标必须一直看得见：曾经"选了目录却锁了某个文件"，就是因为
           树上高亮的节点和用户以为选中的对象不是同一个。另外这里也是"锁在哪"的说明 ——
           加锁/解锁/强制解锁只在右键菜单里（工具栏不再重复放一份）。 -->
      <div class="flex" style="margin-bottom: 6px">
        <span class="muted" style="font-size: 12px" data-testid="wc-current">
          当前：{{ currentNode ? currentNode.path : '（点选树上的文件或目录）' }}
        </span>
        <span class="muted" style="font-size: 12px" data-testid="wc-current-hint">
          右键节点可加锁 / 解锁 / 强制解锁<span v-if="currentNode?.kind === 'dir'">（目录按整棵子树生效）</span>
          · 删除 / 还原作用于**勾选**的项，新建落在当前节点
        </span>
      </div>

      <el-alert
        v-if="truncated"
        type="info"
        :closable="false"
        show-icon
        style="margin-bottom: 8px"
        data-testid="wc-tree-truncated"
        :title="`工作副本有 ${store.items.length} 个条目，已默认只显示变更（切换「只看变更」可看全部目录）`"
      />

      <WorkspaceTree
        :key="store.wc.root"
        ref="treeRef"
        :nodes="visibleNodes"
        :initial-checked="selection"
        :filter="filter"
        :empty-text="onlyChanged ? '没有变更' : '工作副本干净'"
        @update:checked="onCheck"
        @update:current="onCurrent"
        @context-menu="onContextMenu"
      />

      <!-- 右键菜单：锁动作的**唯一入口**（工具栏不再重复放一份） -->
      <div
        v-if="menu"
        ref="menuEl"
        class="ctx-menu"
        data-testid="wc-ctx-menu"
        :style="{ left: `${menu.x}px`, top: `${menu.y}px` }"
      >
        <div class="ctx-title mono">{{ menu.path }}</div>
        <button
          class="ctx-item"
          data-testid="wc-ctx-lock"
          :disabled="claimable.length === 0"
          @click="menuAction(doLock)"
        >
          加锁{{ claimable.length > 1 ? `（${claimable.length}）` : '' }}
        </button>
        <button
          class="ctx-item"
          data-testid="wc-ctx-unlock"
          :disabled="myLocks.length === 0"
          @click="menuAction(doUnlock)"
        >
          解锁{{ myLocks.length > 1 ? `（${myLocks.length}）` : '' }}
        </button>
        <button
          class="ctx-item danger"
          data-testid="wc-ctx-force-unlock"
          :disabled="otherLocks.length === 0"
          @click="menuAction(doForceUnlock)"
        >
          强制解锁{{ otherLocks.length > 1 ? `（${otherLocks.length}）` : '' }}
        </button>
      </div>

      <el-card shadow="never" style="margin-top: 12px">
        <div class="flex">
          <el-input
            v-model="message"
            data-testid="commit-message"
            placeholder="提交说明（可留空）"
            style="max-width: 520px"
          />
          <el-button
            type="primary"
            :loading="store.busy"
            :disabled="!!blocked"
            data-testid="commit-submit"
            @click="doCommit"
          >
            提交（{{ targets.length }}）
          </el-button>
          <span v-if="blocked" class="muted" style="font-size: 12px" data-testid="commit-blocked">
            {{ blocked }}
          </span>
          <span v-else class="muted" style="font-size: 12px" data-testid="commit-hint">
            {{ commitHint }}
          </span>
          <el-button
            v-if="store.conflicts.length"
            text
            type="danger"
            size="small"
            data-testid="goto-conflicts"
            @click="$emit('goto', 'conflicts')"
          >
            去解决冲突
          </el-button>
        </div>
      </el-card>

      <TransferQueue v-if="store.progress" :progress="store.progress" />
    </template>

    <!--
      忽略规则：**按工作副本**编辑（每个仓库要忽略的东西不一样，放公共设置里必然互相打扰）。
      内容写进副本根目录的 `.b-artifactignore` —— 它就是副本里的普通文件，可以像别的文件
      一样提交，让同仓库的同事共用同一套规则。
    -->
    <!-- `draggable`：弹窗会挡住后面的目录树，用户得能把它拖开（拖标题栏） -->
    <el-dialog
      v-model="ignoreDialog"
      title="忽略规则（本工作副本）"
      width="min(92vw, 640px)"
      draggable
    >
      <p class="muted" style="margin: 0 0 8px; font-size: 12px">
        gitignore 语法，一行一条；命中的文件不进状态列表、也不会被提交。规则保存在副本根目录的
        <span class="mono">.b-artifactignore</span>，可以像普通文件一样提交，让同仓库的同事共用。
      </p>
      <el-input
        v-model="ignoreText"
        type="textarea"
        :rows="10"
        class="mono"
        data-testid="wc-ignore-text"
        placeholder="例如：&#10;*.tmp&#10;build/&#10;素材软件缓存/"
      />
      <p v-if="ignoreLoadFailed" class="err" style="margin: 8px 0 0; font-size: 12px">
        读取现有规则失败，直接保存会覆盖原有内容 —— 请先关闭再重试。
      </p>
      <template #footer>
        <el-button @click="ignoreDialog = false">取消</el-button>
        <el-button type="primary" :loading="store.busy" data-testid="wc-ignore-save" @click="saveIgnore">
          保存
        </el-button>
      </template>
    </el-dialog>

    <!--
      宽度自适应用 `min(92vw, 760px)`：原先写死 620px，而四列最小宽度加起来 660px，
      「操作」列被挤进横向滚动区 —— 用户得拖滚动条才能看到「打开」（真实反馈）。
      三层一起治：① 宽度够 + 列宽收紧；② **整行可点即打开**（不必去找按钮）；
      ③ 操作列 `fixed="right"` 钉在右侧，无论多窄都不会被滚出视野。
    -->
    <el-dialog
      v-model="openDialog"
      title="打开已有工作副本"
      width="min(92vw, 760px)"
      draggable
    >
      <el-table
        :data="recents"
        size="small"
        data-testid="open-recent-table"
        @row-click="(row: RecentEntry) => openRecent(row.dir)"
      >
        <el-table-column label="目录" min-width="240" show-overflow-tooltip>
          <template #default="{ row }">
            <span class="mono">{{ row.dir }}</span>
          </template>
        </el-table-column>
        <el-table-column prop="repo" label="仓库" width="120" />
        <el-table-column label="最近打开" width="110">
          <template #default="{ row }">{{ formatRelative(row.lastOpenedAt) }}</template>
        </el-table-column>
        <el-table-column label="操作" width="90" fixed="right">
          <template #default="{ row }">
            <el-button
              text
              type="primary"
              size="small"
              data-testid="open-recent-open"
              @click.stop="openRecent(row.dir)"
            >
              打开
            </el-button>
          </template>
        </el-table-column>
      </el-table>
      <div v-if="recents.length === 0" class="muted" style="font-size: 13px">还没有检出过的工作副本</div>
      <template #footer>
        <el-button @click="openDialog = false">取消</el-button>
        <el-button data-testid="open-browse" @click="browse">浏览目录…</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<script setup lang="ts">
/**
 * 工作副本视图（§6.5 M4.8 重做）。
 *
 * 与旧版的差别：
 *   - 平铺状态表 → **目录树**（`WorkspaceTree` + `utils/tree.ts`）；
 *   - 删掉「刷新状态」按钮（文件监听自动同步；只在降级时才露出来）；
 *   - 删掉「标记新增」（未纳管文件自动算新增，默认不勾选）；
 *   - 提交说明**可空**；
 *   - 树上直接新建目录/删除、直接加锁解锁（**新建文件已去掉**：资产文件应该由用户的
 *     创作工具产生，版本工具里"新建一个空文件"没有意义）；
 *   - 新增「打开已有副本」（不用再绕到仓库页）。
 *
 * 视图本身不含判定逻辑：树怎么建、默认勾谁、提交集会带上谁，全在 `utils/tree.ts` 里，
 * 那些规则是单测覆盖的重点。
 */
import { computed, onMounted, onUnmounted, ref, watch } from 'vue';
import { ElMessageBox } from 'element-plus';

import type { RecentEntry } from '@shared/dto';

import WorkspaceTree from '@/components/WorkspaceTree.vue';
import TransferQueue from '@/components/TransferQueue.vue';
import { api } from '@/api';
import { useAppStore, type BatchResult } from '@/stores/app';
import { formatRelative } from '@/utils/format';
import {
  buildTree,
  filesUnder,
  filterChanged,
  findNode,
  LARGE_TREE_LIMIT,
  myLocksUnder,
  otherLocksUnder,
  selectedPaths,
  type TreeNode,
} from '@/utils/tree';

defineEmits<{ (e: 'goto', tab: string): void }>();

const store = useAppStore();
const message = ref('');

/**
 * 勾选集（提交选择）。
 *
 * **必须由父组件持有并实时回灌给树**（`:default-checked-keys`）：el-tree 的
 * `store.setData()` 在 `data` 引用变化时会**清空 nodesMap、重建全部节点** —— 勾选状态
 * 随之丢失，然后只用 `defaultCheckedKeys` 复原一遍。自动同步每推一次状态都会产生新的
 * 数组，如果 `defaultCheckedKeys` 还是"挂载时算的那一份"，用户刚勾上的东西就会被
 * **悄悄退回**成打开工作副本时的默认集合（用户报的就是这个：刷新时勾选被取消）。
 *
 * 所以这里让它始终等于"当前勾选"，重建时自然复原；用户手改过之后，也不再被
 * "默认勾选"覆盖（见 `selectionTouched`）。
 */
const selection = ref<string[]>([]);
/**
 * 用户手动动过勾选没有。
 *
 * 只在"用户自己把勾全清空"这一种情形下有用：那种情况算"我什么都不想提交"，
 * 而不是"未勾选 = 提交全部"（后者只在**用户从未动过**时成立）。见 `targets`。
 */
const selectionTouched = ref(false);
const onlyChanged = ref(false);
const filter = ref('');
const currentPath = ref<string | null>(null);

/**
 * 当前节点：**每次从当前树里现查**，不缓存节点对象。
 *
 * 自动同步每次都会重建整棵树（`buildTree` 产出全新对象），缓存下来的引用会立刻变成
 * 过期快照 —— 那样工具栏上的"可加锁 N / 已锁 N"会按旧数据算，用户看到的数字就是错的。
 */
const currentNode = computed<TreeNode | null>(() =>
  currentPath.value ? findNode(tree.value, currentPath.value) : null,
);
const openDialog = ref(false);
const treeRef = ref<InstanceType<typeof WorkspaceTree>>();

const recents = computed(() => store.config?.recent ?? []);

/** 当前工作副本对应的服务端 head（用来判断"能不能更新"）。 */
const serverHead = computed(() => {
  const name = store.wc?.repo;
  return store.repos.find((r) => r.name === name)?.head_rev ?? null;
});

const syncText = computed(() => {
  if (!store.wc?.watching) return '文件监听不可用，请手动刷新';
  return `自动同步中 · ${store.summary.committable} 个待提交`;
});

const updateHint = computed(() => {
  const head = serverHead.value;
  if (head === null) return '';
  if (!store.wc) return '';
  return head > store.wc.rev ? `服务端已到 r${head}` : '已是最新';
});

/** 整棵树（未过滤）。数据源就是一次 status 扫描的结果 + 锁列表。 */
const tree = computed(() =>
  buildTree(store.items, store.locks, { me: store.auth?.username ?? '' }),
);

/** 条目太多时默认只看变更：把整棵 24 万文件的树塞进 DOM 不现实。 */
const truncated = computed(() => store.items.length > LARGE_TREE_LIMIT);

const visibleNodes = computed(() => (onlyChanged.value ? filterChanged(tree.value) : tree.value));

/** 树上现有全部路径（**用完整树**判定，不受"只看变更"过滤影响）。 */
const treePaths = computed(() => {
  const out = new Set<string>();
  const walk = (list: readonly TreeNode[]): void => {
    for (const n of list) {
      out.add(n.path);
      walk(n.children);
    }
  };
  walk(tree.value);
  return out;
});

/**
 * 树的每次重建都校正一遍勾选集：**丢掉树上已经没有的路径**
 * （文件被提交 / 删除后没必要继续挂着）。
 *
 * ⚠️ 这里**不会**主动勾选任何东西：刚打开工作副本时树上是干净的（真实反馈：
 * "刚打开工作副本时，目录树都不应该选中"）。原先我按"全部本地变更"预勾，
 * 结果是打开就一片勾选，而且 el-tree 在子项全勾时会把**父目录**也带成勾选态，
 * 看起来就像"目录被选中了"。提交时若用户一个都没勾，按"未勾选 = 提交全部"处理（见 `targets`）。
 */
watch(
  tree,
  () => {
    if (selection.value.length === 0) return;
    const kept = selection.value.filter((p) => treePaths.value.has(p));
    if (kept.length !== selection.value.length) selection.value = kept;
  },
  { immediate: true },
);

/**
 * 本次提交会带上的路径（与引擎的筛选规则一致）。
 *
 * 一个例外：**用户手动把勾选全清空**时算"什么都没选"，而不是"提交全部"——
 * 否则界面上一个勾都没有、按钮却说"提交（3）"，是明确的误导。
 * "空勾选 = 提交全部"只在**用户从未动过勾选**时成立（刚打开工作副本就是这种状态）。
 */
const targets = computed(() => {
  if (selectionTouched.value && selection.value.length === 0) return [];
  return selectedPaths(store.items, selection.value);
});

/**
 * 提交前的阻断原因。
 *
 * **必须按"本次提交集"判定，而不是全量状态**：未纳管文件默认不勾，按全量口径算
 * "没有可提交的变更"会把用户勾上的新文件一起挡掉 —— 这条是真实 Electron E2E 抓出来的
 * 真 bug，界面上的表现是"勾了新增文件，提交按钮还是灰的"。
 */
const blocked = computed(() => {
  if (!store.wc) return null;
  const conflicts = store.items.filter((it) => it.status === 'conflicted').length;
  if (conflicts > 0) return `有 ${conflicts} 个冲突未解决，请先处理再提交`;
  if (targets.value.length === 0) return '没有可提交的变更';
  return null;
});

const commitHint = computed(() => {
  const unversioned = store.summary.unversioned;
  if (unversioned > 0) return `另有 ${unversioned} 个新增文件（勾选后才会提交）`;
  return '未勾选时提交全部变更';
});

onMounted(() => {
  // 打开工作副本时同步一次锁列表（树上要显示锁标记）
  void store.refreshLocks();
  void store.refreshRepos();
  // 菜单的关闭时机：点别处（捕获阶段，见 onDocPointer 的说明）/ 别处右键 / Esc / 滚动 / 改窗口大小
  document.addEventListener('mousedown', onDocPointer, true);
  document.addEventListener('click', onDocPointer, true);
  document.addEventListener('contextmenu', onDocPointer, true);
  document.addEventListener('keydown', onDocKey);
  window.addEventListener('scroll', onDocPointer, true);
  window.addEventListener('resize', onDocPointer);
});

onUnmounted(() => {
  document.removeEventListener('mousedown', onDocPointer, true);
  document.removeEventListener('click', onDocPointer, true);
  document.removeEventListener('contextmenu', onDocPointer, true);
  document.removeEventListener('keydown', onDocKey);
  window.removeEventListener('scroll', onDocPointer, true);
  window.removeEventListener('resize', onDocPointer);
});

/** 树报告勾选变化：以它为准（含勾目录带出的整棵子树），并标记用户动过手。 */
function onCheck(keys: string[]): void {
  selection.value = keys;
  selectionTouched.value = true;
}

function onCurrent(path: string | null): void {
  currentPath.value = path;
}

/** 新建目录的落点：选中目录 → 目录内；选中文件 → 同级；没选 → 仓库根。 */
function targetDir(): string {
  const n = currentNode.value;
  if (!n) return '';
  return n.kind === 'dir' ? n.path : (n.path.split('/').slice(0, -1).join('/') ?? '');
}

const ignoreDialog = ref(false);
const ignoreText = ref('');
const ignoreLoadFailed = ref(false);

/** 打开忽略规则弹窗：先把当前文件内容读进来（读失败要明确提示，别让保存覆盖掉原有规则）。 */
async function openIgnore(): Promise<void> {
  ignoreLoadFailed.value = false;
  ignoreText.value = '';
  ignoreDialog.value = true;
  const content = await store.loadIgnoreRules();
  if (content === null) {
    ignoreLoadFailed.value = true;
    return;
  }
  ignoreText.value = content;
}

async function saveIgnore(): Promise<void> {
  if (await store.saveIgnoreRules(ignoreText.value)) ignoreDialog.value = false;
}

async function doNewDir(): Promise<void> {
  const name = await askName('新建目录', '目录名');
  if (!name) return;
  const path = targetDir() ? `${targetDir()}/${name}` : name;
  await store.mkdir(path);
}

async function askName(title: string, placeholder: string): Promise<string | null> {
  try {
    const r = await ElMessageBox.prompt('', title, {
      inputPlaceholder: placeholder,
      inputPattern: /^[^/\\]{1,200}$/,
      inputErrorMessage: '不能为空，且不能包含路径分隔符',
      confirmButtonText: '创建',
      cancelButtonText: '取消',
    });
    return r.value.trim() || null;
  } catch {
    return null;
  }
}

/**
 * 删除**勾选**的项（与提交 / 还原一致），而不是"当前高亮那一行"。
 *
 * 为什么必须是勾选集：界面上"我选了什么"的唯一表达就是复选框 —— 高亮只是"操作目标"
 * （新建落点、右键加锁的作用对象）。用高亮行做删除会让人删错东西：用户勾了三项，
 * 删除却只动了最后点过的那一行（真实反馈）。
 *
 * 勾选目录时它的子项也都在勾选集里（el-tree 勾父连带勾子），所以目录删除天然是整棵子树；
 * `store.remove` 对目录是递归删除 + 记一条目录的待删除。
 *
 * **删完清空勾选**（与「还原」一致）：删除/还原这类"作用于一批已勾选对象"的动作做完之后，
 * 勾选集就完成了它的使命 —— 继续挂着只剩风险（下一次误点删除会再删一遍同一批路径）。
 */
async function doDelete(): Promise<void> {
  const paths = [...selection.value];
  if (paths.length === 0) {
    store.note('请先勾选要删除的文件或目录', 'err');
    return;
  }
  const head = paths.slice(0, 2).join('、');
  const more = paths.length > 2 ? ` 等 ${paths.length} 项` : '';
  try {
    await ElMessageBox.confirm(
      `删除勾选的 ${paths.length} 项（${head}${more}）？文件会从工作副本移除并记为待删除 —— 基线仍在，随时可以用「还原」取回。`,
      '删除',
      { type: 'warning', confirmButtonText: '删除', cancelButtonText: '取消' },
    );
  } catch {
    return;
  }
  await store.remove(paths);
  resetSelection();
  // 当前节点若正好被删掉就不必留着了（节点可能整个从树上消失）
  if (currentPath.value && !treePaths.value.has(currentPath.value)) currentPath.value = null;
}

/** 当前节点涉及的文件：文件 → 自身；目录 → 整棵子树（目录不是变更实体，不加锁）。 */
const currentFiles = computed(() => (currentNode.value ? filesUnder(currentNode.value) : []));

/**
 * 当前节点范围内**还能加锁**的文件。
 *
 * 已被锁（无论本人还是他人）的文件不再计入：本人锁着它说明已经锁定，他人锁着它
 * 则注定失败。两种情况下"加锁"都不是用户想做的事 —— 「解锁 / 强制解锁」才是。
 */
const claimable = computed(() =>
  currentFiles.value.filter((p) => !store.locks.some((l) => l.path === p)),
);

/** 当前节点范围内、**本人持有**的锁。 */
const myLocks = computed(() =>
  currentNode.value ? myLocksUnder(currentNode.value, store.locks, me()) : [],
);

/** 当前节点范围内、**他人持有**的锁（用来解释"为什么加不上锁"，也是强制解锁的入口）。 */
const otherLocks = computed(() =>
  currentNode.value ? otherLocksUnder(currentNode.value, store.locks, me()) : [],
);

/**
 * 右键菜单（锁动作的唯一入口）。
 *
 * 与"当前节点"共用同一套判定（`claimable` / `myLocks` / `otherLocks`）：右键即选中该节点，
 * 菜单项的可用性与计数都按它算。工具栏不再重复放一份按钮 —— 锁是低频动作，放在右键里
 * 既够用又不占位（真实反馈：那一行按钮太挤）。
 */
const menu = ref<{ x: number; y: number; path: string } | null>(null);
const menuEl = ref<HTMLElement | null>(null);

function onContextMenu(payload: { path: string; kind: 'file' | 'dir'; x: number; y: number }): void {
  currentPath.value = payload.path;
  // 贴边时往回收一点，别让菜单跑出窗口
  const x = Math.min(payload.x, window.innerWidth - 220);
  const y = Math.min(payload.y, window.innerHeight - 140);
  menu.value = { x, y, path: payload.path };
}

function closeMenu(): void {
  menu.value = null;
}

/** 菜单项 → 先关菜单再执行动作（避免菜单压在对话框上）。 */
async function menuAction(fn: () => Promise<void>): Promise<void> {
  closeMenu();
  await fn();
}

/**
 * 点别处就关菜单。
 *
 * **必须在捕获阶段监听 `mousedown`**，两条都是被真实反馈逼出来的：
 *
 * 1. `el-tree` 的节点内容与复选框都带 `.stop`（`withModifiers(["stop"])` / `@click.stop`），
 *    冒泡阶段的 document 监听**收不到**树里的点击 —— 菜单会一直挂在那儿（用户的原话是
 *    "左键点击目录树的其他位置时，右键菜单没有关闭"）。
 * 2. `mousedown` **和** `click` 都挂捕获：
 *    - 真实用户点击必然产生 `mousedown`（先到先关，最跟手）；
 *    - 键盘激活 / 程序化 `.click()` 只产生 `click`（没有 mousedown），少了这条就漏（踩过）。
 *    两条都用同一个"点在菜单内就跳过"的判定，所以点菜单项不会被自己关掉。
 */
function onDocPointer(e: Event): void {
  if (!menu.value) return;
  // 点在菜单自己身上不算"点别处"（交给菜单项的 click 处理）
  if (menuEl.value && e.target instanceof Node && menuEl.value.contains(e.target)) return;
  closeMenu();
}

function onDocKey(e: KeyboardEvent): void {
  if (e.key === 'Escape') closeMenu();
}

function me(): string {
  return store.auth?.username ?? '';
}

/** 批量结果 → 一句话；部分失败时点名第一条原因（其余大概率同理）。 */
function reportBatch(action: string, r: BatchResult): void {
  if (r.failed.length === 0) {
    store.note(`已${action} ${r.ok} 个文件`);
    return;
  }
  const first = r.failed[0]!;
  const extra = r.failed.length > 1 ? ` 等 ${r.failed.length} 个` : '';
  const detail = `${first.path}${extra}：${first.message}`;
  store.note(
    r.ok > 0 ? `已${action} ${r.ok} 个，${r.failed.length} 个失败 —— ${detail}` : `未能${action}：${detail}`,
    'err',
  );
}

/**
 * 加锁：文件锁它自己；**目录则对子树内每个文件加锁**（§5.1"批量加锁由客户端展开为 N 个文件锁"）。
 *
 * **不弹备注框，点了就锁**（真实反馈：加锁是个高频动作，每次都要面对一个输入框太啰嗦）。
 * 备注字段在协议与数据模型里都还在（CLI `-m`、管理端仍可看到），只是客户端这个入口不再采。
 *
 * 这里也刻意**不把目录路径发给服务端**：服务端只接受文件锁，对目录路径会返回 400，
 * 所以目录在客户端展开为一批文件锁。
 */
async function doLock(): Promise<void> {
  const node = currentNode.value;
  if (!node) {
    store.note('请先在目录树里点选一个文件或目录', 'err');
    return;
  }
  const targets = claimable.value;
  if (targets.length === 0) {
    store.note(
      node.kind === 'dir' ? `${node.path} 下的文件都已有锁` : `${node.path} 已经锁着了`,
      'err',
    );
    return;
  }

  if (node.kind === 'file') {
    await store.lock(node.path);
    return;
  }
  const res = await store.lockMany(targets);
  reportBatch('锁定', res);
}

/** 解锁：文件解它自己；目录解子树内**我的**锁（别人的锁走强制解锁）。 */
async function doUnlock(): Promise<void> {
  const paths = myLocks.value;
  if (paths.length === 0) {
    store.note('当前范围内没有你持有的锁', 'err');
    return;
  }
  const res = await store.unlockMany(paths);
  reportBatch('解锁', res);
}

/**
 * 强制解锁（底层动作是 `?break=true`，语义见 §5.5）：解开当前范围内**他人**持有的锁。
 *
 * 与普通解锁的区别只有一条 —— 它**不是自己的锁**：所以需要该文件所在目录的 `admin`
 * 权限，且原因必填并进审计。
 */
async function doForceUnlock(): Promise<void> {
  const paths = otherLocks.value;
  if (paths.length === 0) {
    store.note('当前范围内没有他人持有的锁', 'err');
    return;
  }
  const who = [...new Set(paths.map((l) => l.owner))].join('、');
  let reason = '';
  try {
    const r = await ElMessageBox.prompt(
      `强制解锁 ${who} 持有的 ${paths.length} 把锁？原因必填并写进审计。`,
      '强制解锁',
      {
        inputPattern: /\S{4,}/,
        inputErrorMessage: '原因至少 4 个字符',
        confirmButtonText: '强制解锁',
        cancelButtonText: '取消',
      },
    );
    reason = r.value;
  } catch {
    return;
  }
  const res = await store.unlockMany(paths.map((l) => l.path), reason);
  reportBatch('强制解锁', res);
}

async function doRevert(): Promise<void> {
  await store.revert(selection.value);
  resetSelection();
}

/** 清空勾选（提交 / 还原 / 删除这类"整批动作"做完之后都调用它）。 */
function resetSelection(): void {
  selection.value = [];
  selectionTouched.value = false;
  treeRef.value?.clearChecked();
}

async function doCommit(): Promise<void> {
  // 提交说明可空（§6.5）
  const paths = selection.value.length > 0 ? selection.value : undefined;
  const ok = await store.commit(message.value, paths);
  if (ok) {
    message.value = '';
    resetSelection();
  }
}

async function reveal(): Promise<void> {
  if (store.wc) await store.run(() => api.revealPath(store.wc!.root));
}

async function openRecent(dir: string): Promise<void> {
  const ok = await store.openWorkingCopy(dir);
  if (ok) openDialog.value = false;
}

async function browse(): Promise<void> {
  const picked = await store.run(() => api.pickDir('选择工作副本目录'));
  if (!picked) return;
  await openRecent(picked);
}

// 条目太多（>2 万）时不给整棵树：直接切到"只看变更"，并在界面上说明原因
if (truncated.value) onlyChanged.value = true;
</script>

<style scoped>
.ctx-menu {
  position: fixed;
  z-index: 3000;
  min-width: 180px;
  padding: 4px;
  background: #fff;
  border: 1px solid var(--ba-border);
  border-radius: 6px;
  box-shadow: 0 6px 20px rgb(0 0 0 / 12%);
}
.ctx-title {
  padding: 4px 10px 6px;
  font-size: 11px;
  color: var(--ba-muted);
  border-bottom: 1px solid var(--ba-border);
  margin-bottom: 4px;
  word-break: break-all;
}
.ctx-item {
  display: block;
  width: 100%;
  padding: 6px 10px;
  text-align: left;
  font-size: 13px;
  background: none;
  border: 0;
  border-radius: 4px;
  cursor: pointer;
}
.ctx-item:hover:not(:disabled) {
  background: #f2f6ff;
}
.ctx-item:disabled {
  color: var(--ba-muted);
  cursor: not-allowed;
}
.ctx-item.danger {
  color: #d9534f;
}
</style>
