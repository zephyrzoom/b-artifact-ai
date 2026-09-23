import { flushPromises, mount } from '@vue/test-utils';
import { reactive } from 'vue';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { LockInfo, RecentEntry, RepoSummary, StatusItem } from '../../src/shared/dto.js';
import StatusTable from '../../src/renderer/components/StatusTable.vue';
import TransferQueue from '../../src/renderer/components/TransferQueue.vue';
import LoginView from '../../src/renderer/views/LoginView.vue';
import WorkspaceTree from '../../src/renderer/components/WorkspaceTree.vue';
import ReposView from '../../src/renderer/views/ReposView.vue';
import WorkspaceView from '../../src/renderer/views/WorkspaceView.vue';
import { commonStubs, resetTreeStub, setTreeStubExpanded, treeStubCalls } from './stubs.js';

// 全部视图都从 store 取状态：这里给一个最小可用的替身，专测视图自己的判断逻辑。
// **必须是 reactive**：否则测试里改 `store.items` 不会触发视图重算，
// "自动同步刷新后勾选还在不在"这类用例根本测不到（踩过）。
const store = reactive({
  ready: true,
  info: { version: '0.1.0', electron: '44.3.0', node: '22', chrome: '130', platform: 'darwin', arch: 'x64' },
  config: {
    servers: ['http://a'],
    recent: [] as RecentEntry[],
    concurrency: 4,
    cacheDir: '/c',
    defaultCheckoutParent: '',
  },
  auth: { server: 'http://a', username: 'alice' },
  repos: [] as RepoSummary[],
  wc: null as null | { root: string; repo: string; rev: number; sparse_paths: string[] },
  items: [] as StatusItem[],
  locks: [] as LockInfo[],
  conflicts: [] as unknown as ConflictInfo[],
  progress: null,
  busy: false,
  flash: null as null | { kind: 'ok' | 'err'; text: string },
  summary: { total: 0, committable: 0, conflicts: 0, unversioned: 0, ignored: 0 },
  candidates: [],
  loggedIn: true,
  note: vi.fn(),
  run: vi.fn(async (fn: () => Promise<unknown>) => fn()),
  init: vi.fn(),
  login: vi.fn(async () => true),
  logout: vi.fn(),
  refreshRepos: vi.fn(),
  checkout: vi.fn(async () => true),
  openWorkingCopy: vi.fn(async () => true),
  closeWorkingCopy: vi.fn(),
  refreshStatus: vi.fn(),
  add: vi.fn(),
  remove: vi.fn(),
  revert: vi.fn(),
  commit: vi.fn(async () => true),
  update: vi.fn(),
  refreshLocks: vi.fn(),
  refreshConflicts: vi.fn(async () => {}),
  loadConflictSides: vi.fn(async (): Promise<ConflictSides | null> => null),
  resolveConflict: vi.fn(async () => true),
  loadIgnoreRules: vi.fn(async (): Promise<string | null> => '*.tmp'),
  saveIgnoreRules: vi.fn(async () => true),
  lock: vi.fn(async () => true),
  lockMany: vi.fn(async () => ({ ok: 0, failed: [] })),
  unlock: vi.fn(),
  unlockMany: vi.fn(async () => ({ ok: 0, failed: [] })),
});

vi.mock('../../src/renderer/stores/app.js', () => ({ useAppStore: () => store }));

function item(path: string, status: StatusItem['status'], over: Partial<StatusItem> = {}): StatusItem {
  return { path, kind: 'file', status, base_rev: 1, base_hash: null, size: 0, needs_update: false, ...over };
}

function lock(path: string, owner: string) {
  return {
    id: 1,
    path,
    kind: 'file' as const,
    owner_id: 1,
    owner,
    comment: null,
    created_at: '',
    expires_at: null,
  };
}

beforeEach(() => {
  resetTreeStub();
  // ElMessageBox 是**命令式**挂到 body 上的，不会随组件卸载消失 —— 上一个用例的确认框
  // 会污染下一个用例（"应该没有弹框"的断言直接红）。每条用例开始前清干净。
  document.querySelectorAll('.el-message-box, .el-overlay').forEach((el) => el.remove());
  store.wc = null;
  store.items = [];
  store.locks = [];
  store.summary = { total: 0, committable: 0, conflicts: 0, unversioned: 0, ignored: 0 };
  store.flash = null;
  store.busy = false;
  for (const k of [
    'note',
    'commit',
    'refreshStatus',
    'closeWorkingCopy',
    'login',
    'lock',
    'lockMany',
    'openWorkingCopy',
    'loadIgnoreRules',
    'saveIgnoreRules',
  ] as const) {
    (store[k] as ReturnType<typeof vi.fn>).mockClear();
  }
});

// ---------------------------------------------------------------- StatusTable

describe('StatusTable', () => {
  function mountTable(items: StatusItem[]) {
    return mount(StatusTable, { props: { items }, global: { stubs: commonStubs } });
  }

  it('表头显示摘要文案；空列表显示"工作副本干净"', () => {
    expect(mountTable([]).find('[data-testid="status-summary"]').text()).toBe('工作副本干净');
  });

  it('摘要按冲突优先的口径统计', () => {
    const w = mountTable([item('a', 'modified'), item('b', 'conflicted'), item('c', 'unversioned')]);
    expect(w.find('[data-testid="status-summary"]').text()).toBe('1 个待提交 · 1 个冲突 · 1 个新增');
  });

  it('行数据按"先处理冲突"的顺序给表格', async () => {
    const w = mountTable([item('z', 'normal'), item('c', 'conflicted'), item('m', 'modified')]);
    const rows = (w.vm as unknown as { rows: StatusItem[] }).rows;
    expect(rows.map((r) => r.path)).toEqual(['c', 'm', 'z']);
  });

  it('selectable：可操作的都能勾（含未纳管与冲突），纯 normal/ignored 不给勾', () => {
    const vm = mountTable([]).vm as unknown as { selectable: (r: StatusItem) => boolean };
    expect(vm.selectable(item('a', 'modified'))).toBe(true);
    expect(vm.selectable(item('a', 'added'))).toBe(true);
    expect(vm.selectable(item('a', 'deleted'))).toBe(true);
    // 回归：未纳管必须可勾，否则"标记新增"永远点不动
    expect(vm.selectable(item('a', 'unversioned'))).toBe(true);
    expect(vm.selectable(item('a', 'conflicted'))).toBe(true);
    expect(vm.selectable(item('a', 'normal'))).toBe(false);
    expect(vm.selectable(item('a', 'ignored'))).toBe(false);
  });

  it('勾选后把路径数组抛给父组件（父组件只关心路径）', () => {
    const w = mountTable([]);
    const vm = w.vm as unknown as { onSelection: (rows: StatusItem[]) => void };
    vm.onSelection([item('a.psd', 'modified'), item('b.psd', 'added')]);
    expect(w.emitted('update:selection')).toEqual([[['a.psd', 'b.psd']]]);
  });
});

// ---------------------------------------------------------------- TransferQueue

describe('TransferQueue', () => {
  function mountQueue(progress: { phase: string; done: number; total: number; current?: string }) {
    return mount(TransferQueue, { props: { progress }, global: { stubs: commonStubs } });
  }

  it('阶段名翻成中文，未知阶段原样显示（不吞信息）', () => {
    expect(mountQueue({ phase: 'hash', done: 1, total: 2 }).text()).toContain('计算哈希');
    expect(mountQueue({ phase: 'download', done: 1, total: 2 }).text()).toContain('下载');
    expect(mountQueue({ phase: 'weird', done: 1, total: 2 }).text()).toContain('weird');
  });

  it('显示分子分母与百分比', () => {
    const w = mountQueue({ phase: 'upload', done: 12, total: 30 });
    expect(w.find('[data-testid="transfer-text"]').text()).toBe('12 / 30（40%）');
    expect(w.findComponent({ name: 'ElProgress' }).props('percentage')).toBe(40);
  });

  it('有当前文件时展示文件名', () => {
    expect(mountQueue({ phase: 'upload', done: 1, total: 2, current: 'characters/hero.psd' }).text()).toContain(
      'characters/hero.psd',
    );
  });

  it('total 为 0 时百分比是 0 而不是 NaN', () => {
    expect(mountQueue({ phase: 'hash', done: 0, total: 0 }).findComponent({ name: 'ElProgress' }).props('percentage')).toBe(0);
  });
});

// ---------------------------------------------------------------- WorkspaceView

describe('WorkspaceView', () => {
  function mountWs() {
    return mount(WorkspaceView, { global: { stubs: commonStubs } });
  }

  function openWc(over: Partial<{ root: string; repo: string; rev: number; watching: boolean }> = {}) {
    store.wc = { root: '/w/art', repo: 'art', rev: 3, sparse_paths: [], watching: true, ...over };
  }

  it('没有工作副本时给引导而不是空白表格', () => {
    const w = mountWs();
    expect(w.text()).toContain('还没有打开工作副本');
    expect(w.find('[data-testid="goto-repos"]').exists()).toBe(true);
    // v0.4.17：工作副本页自己就能打开已有副本，不用再绕到仓库页
    expect(w.find('[data-testid="open-existing-empty"]').exists()).toBe(true);
    expect(w.find('[data-testid="commit-submit"]').exists()).toBe(false);
  });

  it('有工作副本时渲染顶栏与工具条', () => {
    openWc();
    const w = mountWs();
    expect(w.text()).toContain('art');
    for (const id of ['wc-update', 'wc-new-dir', 'wc-delete']) {
      expect(w.find(`[data-testid="${id}"]`).exists(), id).toBe(true);
    }
    // 锁动作**只**在右键菜单里：工具栏不再重复放一份（真实反馈：那一行按钮太挤）
    for (const id of ['wc-lock', 'wc-unlock', 'wc-break']) {
      expect(w.find(`[data-testid="${id}"]`).exists(), id).toBe(false);
    }
    // 「新建文件」已整体去掉：资产文件由创作工具产生，版本工具里"新建空文件"没有意义
    expect(w.find('[data-testid="wc-new-file"]').exists()).toBe(false);
    expect(w.find('[data-testid="wc-current-hint"]').text()).toContain('右键');
  });

  it('自动同步可用时**不给**手动刷新按钮，降级时才露出来', () => {
    openWc({ watching: true });
    expect(mountWs().find('[data-testid="wc-refresh"]').exists()).toBe(false);

    openWc({ watching: false });
    const w = mountWs();
    expect(w.find('[data-testid="wc-refresh"]').exists()).toBe(true);
    expect(w.find('[data-testid="sync-state"]').text()).toContain('文件监听不可用');
  });

  it('「标记新增」按钮已经不存在（未纳管文件直接算新增）', () => {
    openWc();
    expect(mountWs().find('[data-testid="wc-add"]').exists()).toBe(false);
  });

  it('更新按钮旁边给出"服务端已到 rN"的提示', () => {
    openWc({ rev: 3 });
    store.repos = [
      {
        id: 1,
        name: 'art',
        description: '',
        owner: 'alice',
        head_rev: 7,
        created_at: '',
        my_role: 'owner',
        my_permissions: { read: true, write: true, admin: true },
      },
    ];
    const w = mountWs();
    expect(w.find('[data-testid="update-hint"]').text()).toContain('r7');
    store.repos = [];
  });

  it('存在冲突时提交按钮禁用并说明原因（不能等点了才报错）', async () => {
    openWc();
    store.items = [item('a.psd', 'conflicted')];
    store.summary = { total: 1, committable: 0, conflicts: 1, unversioned: 0, ignored: 0 };
    const w = mountWs();
    await w.vm.$nextTick();
    expect(w.find('[data-testid="commit-submit"]').attributes('disabled')).toBeDefined();
    expect(w.find('[data-testid="commit-blocked"]').text()).toContain('冲突');
  });

  it('可提交时按钮可用，并显示提交集大小', () => {
    openWc();
    store.items = [item('a.psd', 'modified'), item('b.psd', 'added')];
    store.summary = { total: 2, committable: 2, conflicts: 0, unversioned: 0, ignored: 0 };
    const w = mountWs();
    expect(w.find('[data-testid="commit-submit"]').attributes('disabled')).toBeUndefined();
    expect(w.find('[data-testid="commit-submit"]').text()).toContain('2');
  });

  it('**提交说明可以留空**（v0.4.17：服务端本就接受空说明）', async () => {
    openWc();
    store.items = [item('a.psd', 'modified')];
    store.summary = { total: 1, committable: 1, conflicts: 0, unversioned: 0, ignored: 0 };
    const w = mountWs();
    await w.find('[data-testid="commit-submit"]').trigger('click');
    expect(store.note).not.toHaveBeenCalled();
    // 刚打开工作副本时树上是干净的（没预勾选）→ paths 交给引擎按"全部本地变更"筛
    expect(store.commit).toHaveBeenCalledWith('', undefined);
  });

  it('填了说明就带着说明提交', async () => {
    openWc();
    store.items = [item('a.psd', 'modified')];
    store.summary = { total: 1, committable: 1, conflicts: 0, unversioned: 0, ignored: 0 };
    const w = mountWs();
    await w.find('[data-testid="commit-message"]').setValue('改贴图');
    await w.find('[data-testid="commit-submit"]').trigger('click');
    expect(store.commit).toHaveBeenCalledWith('改贴图', undefined);
  });

  it('**刚打开工作副本时不预勾选任何项**（勾选集为空，目录自然也不会被带勾）', async () => {
    openWc();
    store.items = [
      item('characters/hero.psd', 'modified'),
      item('characters/bg.psd', 'modified'),
      item('brand-new.psd', 'unversioned'),
    ];
    const w = mountWs();
    await w.vm.$nextTick();
    // 回归（真实反馈："刚打开工作副本时，目录树都不应该选中"）：之前默认勾选会把
    // 全部本地变更一起勾上，而 el-tree 在子项全勾时连**父目录**都显示为勾选态
    expect(w.findComponent(WorkspaceTree).props('initialChecked')).toEqual([]);
    // 但提交集仍然是"全部本地变更"（未勾选 = 提交全部，提示条会说明）
    expect(w.find('[data-testid="commit-submit"]').text()).toContain('2');
  });

  it('**自动同步刷新后，用户勾选的项仍然勾着**（回归：el-tree 重建会丢掉勾选）', async () => {
    openWc();
    store.items = [item('a.psd', 'modified'), item('b.psd', 'modified')];
    const w = mountWs();
    await w.vm.$nextTick();
    const tree = w.findComponent(WorkspaceTree);

    // 用户勾上一个（刚打开时是空的，所以这里勾的每一项都是他自己选的）
    tree.vm.$emit('update:checked', ['a.psd']);
    await w.vm.$nextTick();
    expect(tree.props('initialChecked')).toEqual(['a.psd']);

    // 自动同步推来新状态（新数组 → el-tree 会重建全部节点、勾选状态本来会丢）
    store.items = [item('a.psd', 'modified'), item('b.psd', 'modified'), item('c.psd', 'modified')];
    await w.vm.$nextTick();
    await w.vm.$nextTick();

    const after = tree.props('initialChecked') as string[];
    expect(after).toEqual(['a.psd']); // 勾选原样保持
    expect(after).not.toContain('c.psd'); // 用户动过手之后，新变更不会自己跳进来
  });

  it('提交之后勾选回到干净状态（下一轮由用户重新选）', async () => {
    openWc();
    store.items = [item('a.psd', 'modified')];
    const w = mountWs();
    await w.vm.$nextTick();
    const tree = w.findComponent(WorkspaceTree);
    tree.vm.$emit('update:checked', ['a.psd']);
    await w.vm.$nextTick();

    await w.find('[data-testid="commit-submit"]').trigger('click');
    expect(store.commit).toHaveBeenCalled();
    await w.vm.$nextTick();
    expect(tree.props('initialChecked')).toEqual([]);
  });

  it('勾选过的路径从树上消失后被丢掉（不会挂着幽灵勾选）', async () => {
    openWc();
    store.items = [item('a.psd', 'modified'), item('gone.psd', 'modified')];
    const w = mountWs();
    await w.vm.$nextTick();
    const tree = w.findComponent(WorkspaceTree);
    tree.vm.$emit('update:checked', ['a.psd', 'gone.psd']);
    await w.vm.$nextTick();

    store.items = [item('a.psd', 'modified')]; // gone.psd 被提交/删除
    await w.vm.$nextTick();
    await w.vm.$nextTick();
    expect(tree.props('initialChecked')).toEqual(['a.psd']);
  });

  it('手动清空勾选 = 什么都不提交（不能被当成"提交全部"）', async () => {
    openWc();
    store.items = [item('a.psd', 'modified')];
    store.summary = { total: 1, committable: 1, conflicts: 0, unversioned: 0, ignored: 0 };
    const w = mountWs();
    await w.vm.$nextTick();
    w.findComponent(WorkspaceTree).vm.$emit('update:checked', []);
    await w.vm.$nextTick();
    expect(w.find('[data-testid="commit-submit"]').attributes('disabled')).toBeDefined();
    expect(w.find('[data-testid="commit-blocked"]').text()).toContain('没有可提交的变更');
  });

  it('未纳管文件默认不进提交集，只在旁边提示"勾选后才会提交"', () => {
    openWc();
    store.items = [item('a.psd', 'modified'), item('new.psd', 'unversioned')];
    store.summary = { total: 2, committable: 1, conflicts: 0, unversioned: 1, ignored: 0 };
    const w = mountWs();
    // 提交集只算上那一个已修改文件：未纳管默认不勾
    expect(w.find('[data-testid="commit-submit"]').text()).toContain('1');
    expect(w.find('[data-testid="commit-hint"]').text()).toContain('1 个新增');
  });

  it('勾上未纳管文件后按钮**不再**被"没有可提交的变更"挡住（回归：按全量状态判定会误挡）', async () => {
    openWc();
    store.items = [item('new.psd', 'unversioned')];
    store.summary = { total: 1, committable: 0, conflicts: 0, unversioned: 1, ignored: 0 };
    const w = mountWs();
    // 没勾 → 确实没什么可提交的
    expect(w.find('[data-testid="commit-submit"]').attributes('disabled')).toBeDefined();

    // 勾上（真实场景里由树的 check 事件触发）
    w.findComponent(WorkspaceTree).vm.$emit('update:checked', ['new.psd']);
    await w.vm.$nextTick();
    expect(w.find('[data-testid="commit-submit"]').attributes('disabled')).toBeUndefined();
    expect(w.find('[data-testid="commit-submit"]').text()).toContain('1');
  });

  it('全是未纳管文件时直接说明"没有可提交的变更"（而不是给个能点的空按钮）', () => {
    openWc();
    store.items = [item('new.psd', 'unversioned')];
    store.summary = { total: 1, committable: 0, conflicts: 0, unversioned: 1, ignored: 0 };
    const w = mountWs();
    expect(w.find('[data-testid="commit-blocked"]').text()).toContain('没有可提交的变更');
  });

  it('没勾选任何项时删除/还原按钮禁用（它们按勾选集工作）', () => {
    openWc();
    const w = mountWs();
    for (const id of ['wc-delete', 'wc-revert'] as const) {
      expect(w.find(`[data-testid="${id}"]`).attributes('disabled'), id).toBeDefined();
    }
    expect(w.find('[data-testid="wc-current"]').text()).toContain('点选');
  });

  it('**删除按勾选的项执行，而不是高亮的那一行**（真实反馈回归）', async () => {
    openWc();
    store.items = [item('a.psd', 'modified'), item('b.psd', 'modified'), item('c.psd', 'modified')];
    const w = mountWs();
    await w.vm.$nextTick();
    const tree = w.findComponent(WorkspaceTree);

    // 勾上 a、b，高亮却停在 c —— 两者故意不一致
    tree.vm.$emit('update:checked', ['a.psd', 'b.psd']);
    tree.vm.$emit('update:current', 'c.psd');
    await w.vm.$nextTick();
    expect(w.find('[data-testid="wc-delete"]').text()).toContain('2');

    await w.find('[data-testid="wc-delete"]').trigger('click');
    // 确认框里的文案要说清是"勾选的 N 项"
    const boxes = document.querySelectorAll('.el-message-box');
    const box = boxes[boxes.length - 1];
    expect(box, '应当弹出确认框').toBeTruthy();
    expect(box!.textContent ?? '').toContain('勾选的 2 项');
    // el-button 在测试里是替身（没有 el-button--primary 类），按文案取确认按钮
    const ok = [...box!.querySelectorAll('button')].find((b) => /删除|确定/.test(b.textContent ?? ''));
    expect(ok, '确认按钮应当在弹框里').toBeTruthy();
    ok!.click();
    await flushPromises();

    expect(store.remove).toHaveBeenCalledWith(['a.psd', 'b.psd']);
    expect(store.remove).not.toHaveBeenCalledWith(['c.psd']);
  });

  it('删除后**清空勾选**（与「还原」一致，避免误点再删一遍同一批）', async () => {
    openWc();
    store.items = [item('a.psd', 'modified')];
    const w = mountWs();
    await w.vm.$nextTick();
    const tree = w.findComponent(WorkspaceTree);
    tree.vm.$emit('update:checked', ['a.psd']);
    await w.vm.$nextTick();
    expect(tree.props('initialChecked')).toEqual(['a.psd']);

    await w.find('[data-testid="wc-delete"]').trigger('click');
    const box = document.querySelector('.el-message-box')!;
    const ok = [...box.querySelectorAll('button')].find((b) => /删除|确定/.test(b.textContent ?? ''));
    ok!.click();
    await flushPromises();
    await w.vm.$nextTick();
    expect(tree.props('initialChecked')).toEqual([]);
    expect(w.find('[data-testid="wc-delete"]').attributes('disabled')).toBeDefined();
  });

  it('当前节点显示在工具栏上；目录上的加锁/解锁/强制解锁按**整棵子树**计数', async () => {
    openWc();
    store.items = [
      item('characters/hero.psd', 'modified'),
      item('characters/bg.psd', 'modified'),
      item('characters/sky.psd', 'modified'),
      item('characters/ui.psd', 'modified'),
      item('characters/locked.psd', 'modified'),
    ];
    // 一把我的锁 + 一把他人的锁（都在 characters 子树内）→ 还有 3 个文件可加锁
    store.locks = [lock('characters/hero.psd', 'alice'), lock('characters/locked.psd', 'bob')];
    const w = mountWs();
    // 模拟点选 characters 目录（树的 node-click → update:current）
    const tree = w.findComponent(WorkspaceTree);
    const dirNode = {
      path: 'characters',
      name: 'characters',
      kind: 'dir' as const,
      status: null,
      changed: 3,
      changedStatuses: ['modified' as const],
      locked: null,
      locksMine: 2,
      locksOther: 1,
      size: 0,
      children: store.items.map((it) => ({
        path: it.path,
        name: it.path.split('/').pop()!,
        kind: 'file' as const,
        status: it.status,
        changed: 1,
        changedStatuses: [it.status],
        locked: null,
        locksMine: 0,
        locksOther: 0,
        size: 0,
        children: [],
      })),
    };
    // 右键即选中该节点并弹出菜单
    tree.vm.$emit('context-menu', { path: 'characters', kind: 'dir', x: 100, y: 80 });
    await w.vm.$nextTick();

    expect(w.find('[data-testid="wc-current"]').text()).toContain('characters');
    const menu = w.find('[data-testid="wc-ctx-menu"]');
    expect(menu.exists()).toBe(true);
    expect(menu.text()).toContain('characters');
    // 我的锁 → 解锁可用；他人的锁 → 强制解锁可用；加锁显示"还能加锁的文件数"（5 减 2 = 3）
    expect(menu.find('[data-testid="wc-ctx-unlock"]').attributes('disabled')).toBeUndefined();
    expect(menu.find('[data-testid="wc-ctx-force-unlock"]').attributes('disabled')).toBeUndefined();
    expect(menu.find('[data-testid="wc-ctx-force-unlock"]').text()).toContain('强制解锁');
    expect(menu.find('[data-testid="wc-ctx-lock"]').text()).toContain('3');
    store.locks = [];
  });

  it('「忽略规则」弹窗：可拖动（否则挡住后面的目录树就没法一边看一边改）', async () => {
    openWc();
    const w = mountWs();
    await w.find('[data-testid="wc-ignore"]').trigger('click');
    await flushPromises();
    expect(w.findComponent({ name: 'ElDialog' }).props('draggable')).toBe(true);
  });

  it('「忽略规则」弹窗：回填当前规则 → 保存调 store（**按副本**，不在设置里）', async () => {
    openWc();
    const w = mountWs();
    await w.find('[data-testid="wc-ignore"]').trigger('click');
    await flushPromises();
    expect(store.loadIgnoreRules).toHaveBeenCalled();
    expect((w.find('[data-testid="wc-ignore-text"]').element as HTMLTextAreaElement).value).toBe('*.tmp');

    await w.find('[data-testid="wc-ignore-text"]').setValue('*.tmp\nbuild/');
    await w.find('[data-testid="wc-ignore-save"]').trigger('click');
    await flushPromises();
    expect(store.saveIgnoreRules).toHaveBeenCalledWith('*.tmp\nbuild/');
    store.loadIgnoreRules.mockResolvedValue('*.tmp');
  });

  it('读取规则失败时给出警告（别让保存把原有规则覆盖掉）', async () => {
    openWc();
    store.loadIgnoreRules.mockResolvedValue(null);
    const w = mountWs();
    await w.find('[data-testid="wc-ignore"]').trigger('click');
    await flushPromises();
    expect(w.text()).toContain('读取现有规则失败');
    store.loadIgnoreRules.mockResolvedValue('*.tmp');
  });

  it('「打开已有副本」弹窗：操作列钉在右侧 + 整行可点（窄窗口也能看到"打开"）', async () => {
    openWc();
    store.config = {
      ...store.config,
      recent: [{ dir: '/w/art', repo: 'art', server: 'http://a', lastOpenedAt: Date.now() }],
    };
    const w = mountWs();
    await w.find('[data-testid="wc-open-existing"]').trigger('click');
    await w.vm.$nextTick();

    // 回归：弹窗写死 620px 而四列最小宽度合计 660px，「操作」列被挤进横向滚动区
    const action = w
      .findAllComponents({ name: 'ElTableColumn' })
      .find((c) => c.props('label') === '操作');
    expect(action, '弹窗里应当有操作列').toBeTruthy();
    expect(action!.props('fixed')).toBe('right');

    // 整行可点即打开：不必去找按钮；打开成功后弹窗要**关掉**
    await w.find('[data-testid="open-recent-table"] .table-row').trigger('click');
    expect(store.openWorkingCopy).toHaveBeenCalledWith('/w/art');
    const dialog = w.findComponent({ name: 'ElDialog' });
    expect(dialog.props('modelValue')).toBe(false);

    // 列宽/固定是为了"窄窗口也看得到打开"——宽度的自适应用的是 CSS min()，这里只能钉住意图
    expect(dialog.props('width')).toContain('vw');
    expect(dialog.props('draggable')).toBe(true); // 同样会挡住目录树，得能拖开
    store.config = { ...store.config, recent: [] };
  });

  it('双击目录切换展开/收起（单击只选中，不展开）', async () => {
    openWc();
    store.items = [item('characters/hero.psd', 'modified')];
    const w = mountWs();
    const node = w.find('[data-testid="wc-node-characters"]');

    // 未展开 → 双击应 expand
    await node.trigger('dblclick');
    expect(treeStubCalls('characters')).toEqual(['expand']);

    // 已展开 → 双击应 collapse
    setTreeStubExpanded('characters', true);
    await node.trigger('dblclick');
    expect(treeStubCalls('characters')).toEqual(['expand', 'collapse']);
  });

  it('点标签只选中：不带"点叶子顺带勾选"的隐藏行为（回归）', () => {
    openWc();
    store.items = [item('characters/hero.psd', 'modified')];
    const w = mountWs();
    // el-tree 的 checkOnClickLeaf 默认 true —— 必须显式关掉，
    // 否则"点文件标签选中它"会顺手把它的勾选切掉（提交集莫名变空）
    expect(w.findComponent(WorkspaceTree).findComponent({ name: 'ElTree' }).props('checkOnClickLeaf')).toBe(false);
    expect(w.findComponent(WorkspaceTree).findComponent({ name: 'ElTree' }).props('checkOnClickNode')).toBeFalsy();
  });

  it('双击收起会同步展开集合（否则下一次刷新又把目录顶开 —— 真 bug 回归）', async () => {
    openWc();
    store.items = [item('characters/hero.psd', 'modified')];
    const w = mountWs();
    const tree = w.findComponent(WorkspaceTree);
    const node = w.find('[data-testid="wc-node-characters"]');
    setTreeStubExpanded('characters', true);
    // 替身里没有展开箭头 DOM → 走内部节点实例那条兜底路径（真实环境优先点箭头）
    await node.trigger('dblclick');
    expect(treeStubCalls('characters')).toEqual(['collapse']);

    // 自动同步推来新状态 → 树重建；el-tree 会按 default-expanded-keys 复原展开
    const stub = tree.findComponent({ name: 'ElTree' });
    const restored = stub.props('defaultExpandedKeys') as string[];
    expect(restored).not.toContain('characters'); // 收起的目录不该被复原
  });

  it('双击文件不做事（只有目录能展开）', async () => {
    openWc();
    store.items = [item('characters/hero.psd', 'modified')];
    const w = mountWs();
    await w.find('[data-testid="wc-node-characters/hero.psd"]').trigger('dblclick');
    expect(treeStubCalls('characters/hero.psd')).toEqual([]);
  });

  it('左键点别处会关闭右键菜单（树里的点击带 .stop，必须监听捕获阶段）', async () => {
    openWc();
    store.items = [item('characters/hero.psd', 'modified')];
    // **必须 attachTo**：捕获阶段的监听挂在 document 上，组件没进 document 树就收不到事件
    const w = mount(WorkspaceView, { global: { stubs: commonStubs }, attachTo: document.body });
    await w.vm.$nextTick();
    w.findComponent(WorkspaceTree).vm.$emit('context-menu', {
      path: 'characters',
      kind: 'dir',
      x: 10,
      y: 10,
    });
    await w.vm.$nextTick();
    expect(w.find('[data-testid="wc-ctx-menu"]').exists()).toBe(true);

    // 在**树节点内部**派发左键：el-tree 的节点/复选框都 stopPropagation，
    // 冒泡阶段收不到 —— 这正是"点树上别处菜单不关"的根因。
    // 两种路径都要覆盖：真实点击有 mousedown；键盘激活/程序化 click 只有 click。
    const node = w.find('[data-testid="wc-node-characters"]').element;
    node.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    await w.vm.$nextTick();
    expect(w.find('[data-testid="wc-ctx-menu"]').exists()).toBe(false);

    // 只发 click（没有 mousedown）也要能关
    w.findComponent(WorkspaceTree).vm.$emit('context-menu', {
      path: 'characters',
      kind: 'dir',
      x: 10,
      y: 10,
    });
    await w.vm.$nextTick();
    expect(w.find('[data-testid="wc-ctx-menu"]').exists()).toBe(true);
    node.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    await w.vm.$nextTick();
    expect(w.find('[data-testid="wc-ctx-menu"]').exists()).toBe(false);
    w.unmount();
  });

  it('点菜单自己不会把菜单关掉（菜单项的点击要能执行）', async () => {
    openWc();
    store.items = [item('characters/hero.psd', 'modified')];
    const w = mount(WorkspaceView, { global: { stubs: commonStubs }, attachTo: document.body });
    await w.vm.$nextTick();
    w.findComponent(WorkspaceTree).vm.$emit('context-menu', {
      path: 'characters',
      kind: 'dir',
      x: 10,
      y: 10,
    });
    await w.vm.$nextTick();
    w.find('[data-testid="wc-ctx-lock"]').element.dispatchEvent(
      new MouseEvent('mousedown', { bubbles: true }),
    );
    await w.vm.$nextTick();
    expect(w.find('[data-testid="wc-ctx-menu"]').exists()).toBe(true);
    w.unmount();
  });

  it('右键菜单里加锁**不弹备注框**：点一下就锁（文件与目录批量都是）', async () => {
    openWc();
    store.items = [item('characters/a.psd', 'modified'), item('characters/b.psd', 'modified')];
    const w = mountWs();
    await w.vm.$nextTick();
    const tree = w.findComponent(WorkspaceTree);

    // 文件：右键 → 加锁
    tree.vm.$emit('context-menu', { path: 'characters/a.psd', kind: 'file', x: 10, y: 10 });
    await w.vm.$nextTick();
    await w.find('[data-testid="wc-ctx-lock"]').trigger('click');
    await w.vm.$nextTick();
    expect(store.lock).toHaveBeenCalledWith('characters/a.psd');
    // 关键：不再弹 ElMessageBox
    expect(document.querySelector('.el-message-box')).toBeNull();
    expect(w.find('[data-testid="wc-ctx-menu"]').exists()).toBe(false); // 点完自动关

    // 目录 → 批量加锁，同样不弹框
    tree.vm.$emit('context-menu', { path: 'characters', kind: 'dir', x: 10, y: 10 });
    await w.vm.$nextTick();
    await w.find('[data-testid="wc-ctx-lock"]').trigger('click');
    await w.vm.$nextTick();
    expect(store.lockMany).toHaveBeenCalledWith(['characters/a.psd', 'characters/b.psd']);
    expect(document.querySelector('.el-message-box')).toBeNull();
  });

  it('右键菜单项按"能不能做"启用/禁用', async () => {
    openWc();
    store.items = [item('characters/hero.psd', 'modified'), item('characters/locked.psd', 'modified')];
    store.locks = [lock('characters/locked.psd', 'bob')];
    const w = mountWs();
    expect(w.find('[data-testid="wc-ctx-menu"]').exists()).toBe(false);

    w.findComponent(WorkspaceTree).vm.$emit('context-menu', {
      path: 'characters',
      kind: 'dir',
      x: 120,
      y: 80,
    });
    await w.vm.$nextTick();

    const menu = w.find('[data-testid="wc-ctx-menu"]');
    expect(menu.exists()).toBe(true);
    // 没有我的锁 → 解锁禁用；bob 锁着一把 → 强制解锁可用；两个文件都能加锁 → 加锁可用
    expect(menu.find('[data-testid="wc-ctx-lock"]').attributes('disabled')).toBeUndefined();
    expect(menu.find('[data-testid="wc-ctx-unlock"]').attributes('disabled')).toBeDefined();
    expect(menu.find('[data-testid="wc-ctx-force-unlock"]').attributes('disabled')).toBeUndefined();
  });

  it('树里渲染出状态标签与目录汇总（数据来自 utils/tree.ts）', () => {
    openWc();
    store.items = [item('characters/hero.psd', 'modified'), item('characters/bg.psd', 'modified')];
    store.summary = { total: 2, committable: 2, conflicts: 0, unversioned: 0, ignored: 0 };
    const w = mountWs();
    expect(w.find('[data-testid="wc-node-characters"]').exists()).toBe(true);
    expect(w.find('[data-testid="wc-dir-count-characters"]').text()).toContain('变更 2');
    expect(w.find('[data-testid="wc-status-characters/hero.psd"]').text()).toContain('已修改');
  });
});

// ---------------------------------------------------------------- ReposView

describe('ReposView', () => {
  it('从"最近打开的工作副本"打开成功后，通知外层跳到工作副本页', async () => {
    store.wc = null;
    store.config = {
      ...store.config,
      recent: [{ dir: '/w/art', repo: 'art', server: 'http://a', lastOpenedAt: Date.now() }],
    };
    const w = mount(ReposView, { global: { stubs: commonStubs } });
    await w.find('[data-testid="recent-open"]').trigger('click');
    expect(store.openWorkingCopy).toHaveBeenCalledWith('/w/art');
    expect(w.emitted('goto')).toEqual([['workspace']]);
    store.config = { ...store.config, recent: [] };
  });

  it('打开失败（返回 false）不跳转', async () => {
    store.wc = null;
    store.config = {
      ...store.config,
      recent: [{ dir: '/w/bad', repo: 'art', server: 'http://a', lastOpenedAt: Date.now() }],
    };
    store.openWorkingCopy = vi.fn(async () => false);
    const w = mount(ReposView, { global: { stubs: commonStubs } });
    await w.find('[data-testid="recent-open"]').trigger('click');
    expect(w.emitted('goto')).toBeUndefined();
    store.openWorkingCopy = vi.fn(async () => true);
    store.config = { ...store.config, recent: [] };
  });
});

// ---------------------------------------------------------------- LoginView

describe('LoginView', () => {
  it('字段不全时只提示，不发登录请求', async () => {
    const w = mount(LoginView, { global: { stubs: commonStubs } });
    await w.find('[data-testid="login-username"]').setValue('');
    await w.find('[data-testid="login-submit"]').trigger('click');
    expect(store.login).not.toHaveBeenCalled();
    expect(store.note).toHaveBeenCalledWith('请填写服务器、用户名与密码', 'err');
  });

  it('字段齐全时带上服务器/账号/密码调登录', async () => {
    const w = mount(LoginView, { global: { stubs: commonStubs } });
    await w.find('[data-testid="login-server"]').setValue('http://127.0.0.1:9000');
    await w.find('[data-testid="login-username"]').setValue('bob');
    await w.find('[data-testid="login-password"]').setValue('pw');
    await w.find('[data-testid="login-submit"]').trigger('click');
    expect(store.login).toHaveBeenCalledWith('http://127.0.0.1:9000', 'bob', 'pw');
  });

  it('展示应用与运行时版本（便于用户报障时一眼看到版本）', () => {
    const w = mount(LoginView, { global: { stubs: commonStubs } });
    expect(w.find('[data-testid="login-appinfo"]').text()).toContain('Electron 44.3.0');
  });

  it('登录失败时把错误显示出来', async () => {
    store.flash = { kind: 'err', text: '用户名或密码错误' };
    const w = mount(LoginView, { global: { stubs: commonStubs } });
    expect(w.find('[data-testid="login-error"]').text()).toContain('用户名或密码错误');
  });
});

// ---------------------------------------------------------------- 冲突解决

import type { ConflictInfo, ConflictSides } from '../../src/shared/dto.js';
import ConflictResolver from '../../src/renderer/components/ConflictResolver.vue';
import ConflictView from '../../src/renderer/views/ConflictView.vue';

function conflictInfo(over: Partial<ConflictInfo> = {}): ConflictInfo {
  return {
    path: 'a.psd',
    kind: 'file',
    reason: 'both-modified',
    has_mine: true,
    has_theirs: true,
    mergeable: true,
    theirs_rev: 4,
    sides: { base: 'text', mine: 'text', theirs: 'text' },
    ...over,
  };
}

function conflictSides(over: Partial<ConflictSides> = {}): ConflictSides {
  return {
    path: 'a.psd',
    mergeable: true,
    base: { kind: 'text', size: 4, text: 'BASE' },
    mine: { kind: 'text', size: 4, text: 'MINE' },
    theirs: { kind: 'text', size: 6, text: 'THEIRS' },
    ...over,
  };
}

describe('ConflictResolver', () => {
  function mountResolver(info = conflictInfo(), sides = conflictSides()) {
    return mount(ConflictResolver, {
      props: { info, sides, busy: false, solvedCount: 0, total: 2 },
      global: { stubs: commonStubs },
    });
  }

  it('三方内容并列展示（基线 / 我的 / 服务端）', () => {
    const w = mountResolver();
    expect(w.find('[data-testid="conflict-base"]').text()).toBe('BASE');
    expect(w.find('[data-testid="conflict-mine"]').text()).toBe('MINE');
    expect(w.find('[data-testid="conflict-theirs"]').text()).toBe('THEIRS');
  });

  it('可合并时给出多行输入框，且默认填入"我的版本"（从我的改动开始改最省事）', () => {
    const w = mountResolver();
    const input = w.find('[data-testid="conflict-merge-input"]');
    expect(input.exists()).toBe(true);
    // data-testid 落在真正的输入元素上（真实 Element Plus 同理）
    expect(input.element.tagName).toBe('TEXTAREA');
    expect((input.element as HTMLTextAreaElement).value).toBe('MINE');
  });

  it('不可合并（二进制）时没有合并输入框，并明确说只能二选一', () => {
    const w = mountResolver(
      conflictInfo({ mergeable: false, sides: { base: 'binary', mine: 'binary', theirs: 'binary' } }),
      conflictSides({
        mergeable: false,
        base: { kind: 'binary', size: 10, text: null },
        mine: { kind: 'binary', size: 10, text: null },
        theirs: { kind: 'binary', size: 12, text: null },
      }),
    );
    expect(w.find('[data-testid="conflict-merge-input"]').exists()).toBe(false);
    expect(w.find('[data-testid="conflict-save-merged"]').exists()).toBe(false);
    expect(w.find('[data-testid="conflict-hint"]').text()).toContain('二选一');
    expect(w.find('[data-testid="conflict-mine"]').text()).toContain('二进制');
  });

  it('服务端已删除时按钮文案改成"接受服务端删除"，且没有合并入口', () => {
    const w = mountResolver(
      conflictInfo({ reason: 'deleted-remotely', mergeable: false, has_theirs: false }),
      conflictSides({ mergeable: false, theirs: { kind: 'missing', size: 0, text: null } }),
    );
    expect(w.find('[data-testid="conflict-take-theirs"]').text()).toContain('接受服务端删除');
    expect(w.find('[data-testid="conflict-hint"]').text()).toContain('服务端已删除');
    expect(w.find('[data-testid="conflict-theirs"]').text()).toContain('不存在');
  });

  it('点击"保留我的版本" / "使用服务端版本" 各发出对应 choice', async () => {
    const w = mountResolver();
    await w.find('[data-testid="conflict-take-mine"]').trigger('click');
    await w.find('[data-testid="conflict-take-theirs"]').trigger('click');
    expect(w.emitted('resolve')).toEqual([['mine'], ['theirs']]);
  });

  it('保存合并结果时带上编辑后的内容', async () => {
    const w = mountResolver();
    await w.find('[data-testid="conflict-merge-input"]').setValue('MERGED-CONTENT');
    await w.find('[data-testid="conflict-save-merged"]').trigger('click');
    expect(w.emitted('resolve')).toEqual([['merged', 'MERGED-CONTENT']]);
  });

  it('合并内容被清空时保存按钮禁用（不允许用空内容覆盖文件）', async () => {
    const w = mountResolver();
    await w.find('[data-testid="conflict-merge-input"]').setValue('');
    expect(w.find('[data-testid="conflict-save-merged"]').attributes('disabled')).toBeDefined();
  });

  it('切换到另一个冲突项时合并框重置为该文件的本地内容', async () => {
    const w = mountResolver();
    await w.find('[data-testid="conflict-merge-input"]').setValue('临时输入');
    await w.setProps({
      info: conflictInfo({ path: 'b.psd' }),
      sides: conflictSides({ path: 'b.psd', mine: { kind: 'text', size: 3, text: 'B2' } }),
    });
    expect(
      (w.find('[data-testid="conflict-merge-input"]').element as HTMLTextAreaElement).value,
    ).toBe('B2');
  });

  it('显示进度与文件过大时的说明', () => {
    const w = mountResolver(
      conflictInfo({ mergeable: false }),
      conflictSides({ mergeable: false, theirs: { kind: 'too-large', size: 9_000_000, text: null } }),
    );
    expect(w.find('[data-testid="conflict-count"]').text()).toBe('0 / 2 已解决');
    expect(w.find('[data-testid="conflict-theirs"]').text()).toContain('文件过大');
  });
});

describe('ConflictView', () => {
  function mountView() {
    return mount(ConflictView, { global: { stubs: commonStubs } });
  }

  it('没有工作副本时给引导', () => {
    store.wc = null;
    store.conflicts = [];
    expect(mountView().text()).toContain('还没有打开工作副本');
  });

  it('没有冲突时显示"没有未解决的冲突"', async () => {
    store.wc = { root: '/w/art', repo: 'art', rev: 3, sparse_paths: [] };
    store.conflicts = [];
    const w = mountView();
    await w.vm.$nextTick();
    expect(w.find('[data-testid="conflicts-empty"]').exists()).toBe(true);
  });

  it('有冲突时渲染表格与三方对比', async () => {
    store.wc = { root: '/w/art', repo: 'art', rev: 3, sparse_paths: [] };
    store.conflicts = [conflictInfo()];
    store.loadConflictSides = vi.fn(async () => conflictSides());
    const w = mountView();
    await flushPromises();
    expect(w.find('[data-testid="conflict-table"]').exists()).toBe(true);
    expect(w.find('[data-testid="conflict-resolver"]').exists()).toBe(true);
  });

  it('解决成功后刷新冲突清单与状态', async () => {
    store.wc = { root: '/w/art', repo: 'art', rev: 3, sparse_paths: [] };
    store.conflicts = [conflictInfo()];
    store.loadConflictSides = vi.fn(async () => conflictSides());
    store.resolveConflict = vi.fn(async () => true);
    store.refreshConflicts = vi.fn(async () => {});
    store.refreshStatus = vi.fn(async () => {});
    const w = mountView();
    await flushPromises();

    await w.find('[data-testid="conflict-take-mine"]').trigger('click');
    await flushPromises();
    expect(store.resolveConflict).toHaveBeenCalledWith('a.psd', 'mine', undefined);
    expect(store.refreshConflicts).toHaveBeenCalled();
    expect(store.refreshStatus).toHaveBeenCalled();
  });

  it('解决失败时不做任何"已完成"的假象（不刷进度）', async () => {
    store.wc = { root: '/w/art', repo: 'art', rev: 3, sparse_paths: [] };
    store.conflicts = [conflictInfo()];
    store.loadConflictSides = vi.fn(async () => conflictSides());
    store.resolveConflict = vi.fn(async () => false);
    store.refreshConflicts = vi.fn(async () => {});
    store.refreshStatus = vi.fn(async () => {});
    const w = mountView();
    await flushPromises();

    await w.find('[data-testid="conflict-take-theirs"]').trigger('click');
    await flushPromises();
    expect(store.refreshStatus).not.toHaveBeenCalled();
    expect(w.find('[data-testid="conflict-count"]').text()).toBe('0 / 1 已解决');
  });
});
