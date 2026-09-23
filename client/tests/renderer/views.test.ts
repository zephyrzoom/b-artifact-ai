/**
 * 历史 / 设置两个视图的组件测试。
 *
 * 单独一个文件（而不是并进 components.test.ts）：这两个视图依赖 `@/api`，
 * 而 `vi.mock` 是**文件级**的，混在一起会把 api 替身泄漏给别的用例。
 */

import { flushPromises, mount } from '@vue/test-utils';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { CacheStats, LogEntry } from '../../src/shared/dto.js';
import { commonStubs } from './stubs.js';

const api = {
  log: vi.fn(),
  treeAt: vi.fn(),
  downloadRevision: vi.fn(),
  pickDir: vi.fn(),
  cacheStats: vi.fn(),
  clearCache: vi.fn(),
  setConfig: vi.fn(),
  revealPath: vi.fn(),
};

vi.mock('../../src/renderer/api.js', () => ({ api }));

const store = {
  wc: null as null | { root: string; repo: string; rev: number; sparse_paths: string[] },
  config: null as null | Record<string, unknown>,
  conflicts: [],
  repos: [] as Array<{
    name: string;
    my_permissions: { read: boolean; write: boolean; admin: boolean };
    my_role: string;
  }>,
  busy: false,
  flash: null,
  note: vi.fn(),
  refreshRepos: vi.fn(async () => {}),
  checkout: vi.fn(async () => true),
  // 忠实模拟真实 store 的 run：成功且给了 okText 时会 flash 一条成功提示
  run: vi.fn(async (fn: () => Promise<unknown>, okText?: string) => {
    const r = await fn();
    if (okText) store.note(okText, 'ok');
    return r;
  }),
  openWorkingCopy: vi.fn(async () => true),
};

vi.mock('../../src/renderer/stores/app.js', () => ({ useAppStore: () => store }));

/** FileTree 替身：声明 modelValue，便于断言"换仓库时勾选被清空"。 */
const FileTreeStub = {
  name: 'FileTree',
  props: { modelValue: { type: Array, default: () => [] }, repo: { type: String, default: '' } },
  emits: ['update:modelValue'],
  template: '<div class="file-tree-stub" />',
};

const HistoryView = (await import('../../src/renderer/views/HistoryView.vue')).default;
const ReposView = (await import('../../src/renderer/views/ReposView.vue')).default;
const SettingsView = (await import('../../src/renderer/views/SettingsView.vue')).default;

function logEntry(over: Partial<LogEntry> = {}): LogEntry {
  return {
    rev: 2,
    author: 'alice',
    message: '改贴图',
    created_at: '2026-09-15T03:00:00Z',
    file_count: 3,
    byte_delta: 2048,
    manifest_hash: 'm',
    ...over,
  };
}

function mountView(component: unknown) {
  return mount(component as never, {
    global: { stubs: { ...commonStubs, FileTree: FileTreeStub } },
  });
}

beforeEach(() => {
  for (const fn of Object.values(api)) (fn as ReturnType<typeof vi.fn>).mockReset();
  store.note.mockClear();
  store.run.mockClear();
  store.openWorkingCopy.mockClear();
  store.refreshRepos.mockClear();
  store.checkout.mockClear();
  // 表格替身升级后，`el-table-column` 的插槽真的会被渲染 —— 行数据必须是**真实形状**
  // （缺 `my_permissions` 会在模板里直接抛错，而不是安静地什么都不显示）
  store.repos = [
    { name: 'art', my_permissions: { read: true, write: true, admin: false }, my_role: 'write' },
    { name: 'art2', my_permissions: { read: true, write: false, admin: false }, my_role: 'read' },
  ];
  store.wc = { root: '/w/art', repo: 'art', rev: 3, sparse_paths: [] };
  store.config = {
    concurrency: 8,
    cacheDir: '/home/u/.b-artifact/cache',
    servers: [],
    recent: [
      { dir: '/w/art', repo: 'art', server: 'http://s', lastOpenedAt: Date.now() },
    ],
    defaultCheckoutParent: '/w/checkouts',
  };

  api.log.mockResolvedValue({ items: [logEntry()], total: 1 });
  api.treeAt.mockResolvedValue({
    repo: 'art',
    rev: 2,
    prefix: '',
    depth: 1,
    items: [
      { path: 'props', kind: 'dir', blob_hash: null, size: 0, mode: 0, mtime: 0, changed_rev: 2 },
      { path: 'props/table.png', kind: 'file', blob_hash: 'sha$t', size: 11, mode: 0, mtime: 0, changed_rev: 2 },
    ],
    total: 2,
  });
  api.downloadRevision.mockResolvedValue({ saved: '/tmp/x/table.png', size: 11, rev: 2, path: 'props/table.png' });
  api.pickDir.mockResolvedValue('/tmp/x');
  api.cacheStats.mockResolvedValue({
    dir: '/home/u/.b-artifact/cache',
    blobs: 12,
    bytes: 4096,
    tmpFiles: 2,
    tmpBytes: 100,
  } satisfies CacheStats);
  api.clearCache.mockResolvedValue({ removed: 2, bytes: 100, clearedTmp: true });
  api.setConfig.mockImplementation(async (patch: Record<string, unknown>) => ({
    ...store.config,
    ...patch,
  }));
});

// ---------------------------------------------------------------- 历史视图

describe('HistoryView', () => {
  it('没有工作副本时给引导（历史按工作副本所属仓库展示）', async () => {
    store.wc = null;
    const w = mountView(HistoryView);
    await flushPromises();
    expect(w.text()).toContain('还没有打开工作副本');
    expect(api.log).not.toHaveBeenCalled();
  });

  it('修订列表的表头写全称「修订」——不能只摆一个 r', async () => {
    const w = mountView(HistoryView);
    await flushPromises();
    const labels = w
      .findAllComponents({ name: 'ElTableColumn' })
      .map((c) => c.props('label') as string);
    expect(labels).toContain('修订');
    expect(labels).not.toContain('r');
  });

  it('加载修订列表，并自动选中最新一条', async () => {
    api.log.mockResolvedValue({ items: [logEntry({ rev: 5 }), logEntry({ rev: 4 })], total: 2 });
    const w = mountView(HistoryView);
    await flushPromises();

    expect(api.log).toHaveBeenCalledWith('art', { limit: 50, offset: 0 });
    const rows = (w.vm as unknown as { revisions: LogEntry[] }).revisions;
    expect(rows.map((r) => r.rev)).toEqual([5, 4]);
    // 选中最新一条后自动拉它的文件明细
    expect(api.treeAt).toHaveBeenCalledWith('art', 5, '', 1);
  });

  it('列表头给出作者数与累计增量', async () => {
    api.log.mockResolvedValue({
      items: [logEntry({ rev: 2, author: 'alice', byte_delta: 100 }), logEntry({ rev: 1, author: 'bob', byte_delta: 200 })],
      total: 2,
    });
    const w = mountView(HistoryView);
    await flushPromises();
    expect(w.text()).toContain('2 条修订 · 2 位作者 · 累计 +300 B');
  });

  it('满页时出现"加载更早的修订"，不满页时提示已到底', async () => {
    api.log.mockResolvedValue({ items: [logEntry()], total: 1 });
    const w = mountView(HistoryView);
    await flushPromises();
    expect(w.find('[data-testid="history-load-more"]').exists()).toBe(false);
    expect(w.find('[data-testid="history-end"]').exists()).toBe(true);
  });

  it('点"加载更早"会带 offset 再拉一页并追加', async () => {
    const many = Array.from({ length: 50 }, (_, i) => logEntry({ rev: 50 - i }));
    api.log.mockResolvedValueOnce({ items: many, total: 120 });
    const w = mountView(HistoryView);
    await flushPromises();
    expect(w.find('[data-testid="history-load-more"]').exists()).toBe(true);

    api.log.mockResolvedValueOnce({ items: [logEntry({ rev: 1 })], total: 120 });
    await w.find('[data-testid="history-load-more"]').trigger('click');
    await flushPromises();

    expect(api.log).toHaveBeenLastCalledWith('art', { limit: 50, offset: 50 });
    expect((w.vm as unknown as { revisions: LogEntry[] }).revisions).toHaveLength(51);
  });

  it('选中非 HEAD 修订时提示服务端慢路径（list_dir_at_rev 已知未达标）', async () => {
    store.wc = { root: '/w/art', repo: 'art', rev: 3, sparse_paths: [] };
    api.log.mockResolvedValue({ items: [logEntry({ rev: 2 })], total: 1 });
    const w = mountView(HistoryView);
    await flushPromises();
    expect(w.find('[data-testid="history-slow-hint"]').exists()).toBe(true);
  });

  it('选中 HEAD 修订时不提示慢路径', async () => {
    store.wc = { root: '/w/art', repo: 'art', rev: 2, sparse_paths: [] };
    api.log.mockResolvedValue({ items: [logEntry({ rev: 2 })], total: 1 });
    const w = mountView(HistoryView);
    await flushPromises();
    expect(w.find('[data-testid="history-slow-hint"]').exists()).toBe(false);
  });

  // 明细表用 el-table 的 scoped slot 渲染"下载此版本"按钮，替身不渲染行 →
  // 这里直接驱动组件方法（DOM 级点击由真实 Electron 冒烟覆盖）
  const fileRow = {
    path: 'props/table.png',
    kind: 'file' as const,
    blob_hash: 'sha$t',
    size: 11,
    mode: 0,
    mtime: 0,
    changed_rev: 2,
  };

  it('没有设置下载目录时点下载只提示，不发请求', async () => {
    const w = mountView(HistoryView);
    await flushPromises();
    const vm = w.vm as unknown as { download: (r: typeof fileRow) => Promise<void> };
    await vm.download(fileRow);
    expect(api.downloadRevision).not.toHaveBeenCalled();
    expect(store.note).toHaveBeenCalledWith('请先设置下载目录', 'err');
  });

  it('设置目录后下载旧版本，并把保存路径提示出来', async () => {
    const w = mountView(HistoryView);
    await flushPromises();
    await w.find('[data-testid="download-target"]').trigger('click');
    await flushPromises();
    expect(api.pickDir).toHaveBeenCalled();
    expect(api.downloadRevision).not.toHaveBeenCalled();

    const vm = w.vm as unknown as { download: (r: typeof fileRow) => Promise<void> };
    await vm.download(fileRow);
    await flushPromises();
    expect(api.downloadRevision).toHaveBeenCalledWith('art', 'props/table.png', 2, '/tmp/x');
    expect(store.note).toHaveBeenCalledWith(expect.stringContaining('/tmp/x/table.png'));
  });

  it('目录行点击进入子目录（带前缀再查一层）', async () => {
    const w = mountView(HistoryView);
    await flushPromises();
    const vm = w.vm as unknown as { goto: (p: string) => void };
    vm.goto('props');
    await flushPromises();
    expect(api.treeAt).toHaveBeenLastCalledWith('art', 2, 'props', 1);
  });
});

// ---------------------------------------------------------------- 设置视图

describe('SettingsView', () => {
  it('**不再展示「最近打开的工作副本」**（它只在"打开已有副本"弹窗与仓库页出现）', async () => {
    const w = mountView(SettingsView);
    await flushPromises();
    expect(w.find('[data-testid="settings-recent"]').exists()).toBe(false);
    expect(w.text()).not.toContain('最近打开的工作副本');
    // 设置页该有的东西还在（删得干净、没删过头）
    expect(w.find('[data-testid="settings-general"]').exists()).toBe(true);
    expect(w.find('[data-testid="settings-cache"]').exists()).toBe(true);
  });

  it('挂载时用当前配置回填表单，并查一次缓存占用', async () => {
    const w = mountView(SettingsView);
    await flushPromises();
    expect((w.find('[data-testid="settings-concurrency"]').element as HTMLInputElement).value).toBe('8');
    expect(w.find('[data-testid="settings-ignore"]').exists()).toBe(false);
    expect(api.cacheStats).toHaveBeenCalled();
  });

  it('显示缓存占用（blob 数 / 空间 / 残留临时文件）', async () => {
    const w = mountView(SettingsView);
    await flushPromises();
    expect(w.find('[data-testid="cache-blobs"]').text()).toBe('12');
    expect(w.find('[data-testid="cache-bytes"]').text()).toBe('4.0 KB');
    expect(w.find('[data-testid="cache-tmp"]').text()).toBe('2');
  });

  it('检出默认路径：回填 → 浏览按钮写入输入框 → 保存时一起提交', async () => {
    const w = mountView(SettingsView);
    await flushPromises();
    expect((w.find('[data-testid="settings-checkout-dir"]').element as HTMLInputElement).value).toBe(
      '/w/checkouts',
    );

    api.pickDir.mockResolvedValueOnce('/picked/checkouts');
    await w.find('[data-testid="settings-checkout-browse"]').trigger('click');
    await flushPromises();
    expect((w.find('[data-testid="settings-checkout-dir"]').element as HTMLInputElement).value).toBe(
      '/picked/checkouts',
    );

    await w.find('[data-testid="settings-save"]').trigger('click');
    await flushPromises();
    expect(api.setConfig).toHaveBeenCalledWith(
      expect.objectContaining({ defaultCheckoutParent: '/picked/checkouts' }),
    );
  });

  it('保存时只提交并发数，并用返回值回填（忽略规则不在设置里）', async () => {
    const w = mountView(SettingsView);
    await flushPromises();
    await w.find('[data-testid="settings-concurrency"]').setValue('16');
    await w.find('[data-testid="settings-save"]').trigger('click');
    await flushPromises();

    expect(api.setConfig).toHaveBeenCalledWith(
      expect.objectContaining({ concurrency: 16, defaultCheckoutParent: '/w/checkouts' }),
    );
    expect(store.note).toHaveBeenCalledWith('设置已保存', 'ok');
  });

  it('没有残留临时文件时"清理残留"按钮禁用', async () => {
    api.cacheStats.mockResolvedValue({ dir: '/c', blobs: 1, bytes: 1, tmpFiles: 0, tmpBytes: 0 });
    const w = mountView(SettingsView);
    await flushPromises();
    expect(w.find('[data-testid="cache-clear-tmp"]').attributes('disabled')).toBeDefined();
  });

  it('清空整个缓存后刷新占用并记录结果', async () => {
    api.cacheStats
      .mockResolvedValueOnce({ dir: '/c', blobs: 12, bytes: 4096, tmpFiles: 2, tmpBytes: 100 })
      .mockResolvedValueOnce({ dir: '/c', blobs: 0, bytes: 0, tmpFiles: 0, tmpBytes: 0 });
    const w = mountView(SettingsView);
    await flushPromises();

    const vm = w.vm as unknown as { clear: (s: 'tmp' | 'all') => Promise<void> };
    await vm.clear('tmp');
    await flushPromises();

    expect(api.clearCache).toHaveBeenCalledWith(true);
    expect(w.find('[data-testid="cache-last-clear"]').text()).toContain('删除 2 个文件');
  });

  it('"在文件管理器中打开"用当前缓存目录', async () => {
    const w = mountView(SettingsView);
    await flushPromises();
    await w.find('[data-testid="cache-reveal"]').trigger('click');
    expect(api.revealPath).toHaveBeenCalledWith('/home/u/.b-artifact/cache');
  });
});


// ---------------------------------------------------------------- 仓库页（部分检出）

describe('ReposView（勾选式部分检出）', () => {
  function mountRepos() {
    return mountView(ReposView);
  }

  it('检出默认目录 = **设置里的默认路径 + 仓库名**（用户手改过就不再覆盖）', async () => {
    store.wc = null;
    store.config = { ...store.config, defaultCheckoutParent: '/w/checkouts' };
    const w = mountView(ReposView);
    await flushPromises();

    const rows = w.findAll('.table-row');
    await rows[0]!.trigger('click'); // 选中 art
    await flushPromises();
    expect((w.find('[data-testid="checkout-dir"]').element as HTMLInputElement).value).toBe(
      '/w/checkouts/art',
    );

    // 手改过之后再切仓库：不能把用户输入的路径冲掉
    await w.find('[data-testid="checkout-dir"]').setValue('/custom/place');
    await rows[1]!.trigger('click'); // 换成 art2
    await flushPromises();
    expect((w.find('[data-testid="checkout-dir"]').element as HTMLInputElement).value).toBe(
      '/custom/place',
    );

    // 提示行的预览也指向同一个路径
    expect(w.find('[data-testid="checkout-dir-hint"]').text()).toContain('/custom/place');
    store.config = { ...store.config, defaultCheckoutParent: '' };
  });

  it('未选中仓库时不渲染目录树，给一句引导', async () => {
    const w = mountRepos();
    await flushPromises();
    expect(w.findComponent(FileTreeStub).exists()).toBe(false);
    expect(w.text()).toContain('先选中仓库');
  });

  it('选中仓库后挂上目录树，并把仓库名传下去', async () => {
    const w = mountRepos();
    await flushPromises();
    const vm = w.vm as unknown as { onPick: (r: { name: string } | null) => void };
    vm.onPick({ name: 'art' });
    await flushPromises();

    const tree = w.findComponent(FileTreeStub);
    expect(tree.exists()).toBe(true);
    expect(tree.props('repo')).toBe('art');
  });

  it('检出时把勾选的前缀一起传给 store', async () => {
    const w = mountRepos();
    await flushPromises();
    const vm = w.vm as unknown as {
      onPick: (r: { name: string } | null) => void;
      dir: string;
      sparsePrefixes: string[];
      submit: () => Promise<void>;
    };
    vm.onPick({ name: 'art' });
    await flushPromises();

    // 模拟用户在树上勾选了 characters（组件内部会 emit update:modelValue）
    w.findComponent(FileTreeStub).vm.$emit('update:modelValue', ['characters']);
    await flushPromises();
    vm.dir = '/tmp/wc';
    await vm.submit();

    expect(store.checkout).toHaveBeenCalledWith('art', '/tmp/wc', ['characters']);
  });

  it('换仓库会清空已勾选的前缀（上一个仓库的路径不能带过去）', async () => {
    const w = mountRepos();
    await flushPromises();
    const vm = w.vm as unknown as { onPick: (r: { name: string } | null) => void; sparsePrefixes: string[] };

    vm.onPick({ name: 'art' });
    await flushPromises();
    w.findComponent(FileTreeStub).vm.$emit('update:modelValue', ['characters']);
    await flushPromises();
    expect(vm.sparsePrefixes).toEqual(['characters']);

    vm.onPick({ name: 'art2' });
    await flushPromises();
    expect(vm.sparsePrefixes).toEqual([]);
  });

  it('显示检出目录的回显（便于确认到底往哪儿检）', async () => {
    const w = mountRepos();
    await flushPromises();
    const vm = w.vm as unknown as { dir: string };
    vm.dir = '/tmp/workspace/art';
    await flushPromises();
    expect(w.find('[data-testid="checkout-dir-hint"]').text()).toContain('/tmp/workspace/art');
  });

  it('「浏览…」把选中的目录填进输入框', async () => {
    const w = mountRepos();
    await flushPromises();
    api.pickDir.mockResolvedValue('/picked/dir');
    await w.find('[data-testid="checkout-browse"]').trigger('click');
    await flushPromises();
    expect((w.vm as unknown as { dir: string }).dir).toBe('/picked/dir');
  });
});
