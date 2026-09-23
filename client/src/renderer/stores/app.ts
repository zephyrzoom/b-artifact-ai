/**
 * 应用状态（Pinia）。
 *
 * 唯一的"业务编排"位置：视图只读状态、调 action。所有 IPC 调用都经 `api.ts`，
 * 错误统一收敛到 `message`，避免每个视图各写一套 try/catch。
 */

import { defineStore } from 'pinia';
import { applyLocalLocks } from '@/utils/locks';
import { computed, ref } from 'vue';

import type {
  AppConfig,
  AppInfo,
  AuthState,
  ConflictInfo,
  ConflictResolution,
  ConflictSides,
  LockInfo,
  ProgressEvent,
  RepoSummary,
  StatusItem,
  WorkingCopyState,
} from '@shared/dto';

import { api, IpcError, on } from '@/api';
import { committableItems, summarize } from '@/utils/status';

export interface Flash {
  kind: 'ok' | 'err';
  text: string;
}

/** 批量操作的结果（部分成功要如实报出来，不能只报"成功 / 失败"两态）。 */
export interface BatchResult {
  ok: number;
  failed: { path: string; message: string }[];
}

export const useAppStore = defineStore('app', () => {
  const ready = ref(false);
  const info = ref<AppInfo | null>(null);
  const config = ref<AppConfig | null>(null);
  const auth = ref<AuthState | null>(null);
  const repos = ref<RepoSummary[]>([]);
  const wc = ref<WorkingCopyState | null>(null);
  const items = ref<StatusItem[]>([]);
  const locks = ref<LockInfo[]>([]);
  const conflicts = ref<ConflictInfo[]>([]);
  const progress = ref<ProgressEvent | null>(null);
  const busy = ref(false);
  const flash = ref<Flash | null>(null);

  const summary = computed(() => summarize(items.value));
  const candidates = computed(() => committableItems(items.value));
  const loggedIn = computed(() => auth.value !== null);

  function note(text: string, kind: Flash['kind'] = 'ok'): void {
    flash.value = { kind, text };
  }

  /** 统一的调用包装：busy 标记 + 错误转 flash（IPC 层已经保证错误带 code）。 */
  async function run<T>(fn: () => Promise<T>, okText?: string): Promise<T | null> {
    busy.value = true;
    try {
      const r = await fn();
      if (okText) note(okText);
      return r;
    } catch (e) {
      const msg = e instanceof IpcError ? `${e.message}（${e.code}）` : String(e);
      note(msg, 'err');
      return null;
    } finally {
      busy.value = false;
    }
  }

  // ---------- 启动 ----------

  async function init(): Promise<void> {
    await run(async () => {
      info.value = await api.appInfo();
      config.value = await api.config();
      auth.value = await api.authState();
    });
    // 主进程推送：传输进度与工作副本状态。
    // 这里容错：如果 preload 没注入，上面那次 appInfo 调用已经把错误 flash 出来了，
    // 再让 init 抛出去只会变成一条无人处理的 rejection，反而看不出发生了什么。
    try {
      on('wc:progress', (p) => {
        progress.value = p;
      });
      on('wc:state', (s) => {
        wc.value = s;
      });
      // 文件监听触发的自动同步（§6.5）：主进程重算完直接把结果推过来，
      // 渲染层不再需要"刷新状态"按钮。
      on('wc:changed', (list) => {
        items.value = list;
        // 顺手把锁列表也刷一遍：本地文件变了往往伴随一次提交/加锁
        void refreshLocksQuiet();
      });
    } catch {
      /* 已在 flash 里报过 NO_BRIDGE */
    }
    ready.value = true;
    if (loggedIn.value) await refreshRepos();
  }

  /**
   * 重新拉一次配置（静默）。
   *
   * **检出 / 打开工作副本之后必须调**：主进程会把这条记录写进 `config.recent`，
   * 而渲染层的 `store.config` 是登录时的快照 —— 不重拉的话，「打开已有副本」弹窗
   * 会显示**过期的最近列表**（刚检出完打开它，列表甚至是空的；真事故过）。
   */
  async function refreshConfig(): Promise<void> {
    try {
      config.value = await api.config();
    } catch {
      /* 静默：配置读失败不该打断当前操作，下一次操作还会再试 */
    }
  }

  // ---------- 认证 ----------

  async function login(server: string, username: string, password: string): Promise<boolean> {
    const r = await run(() => api.login(server, username, password), '登录成功');
    if (!r) return false;
    auth.value = { server: r.server, username: r.username };
    config.value = await api.config();
    await refreshRepos();
    return true;
  }

  async function logout(): Promise<void> {
    await run(() => api.logout(), '已退出登录');
    stopLockPolling();
    auth.value = null;
    repos.value = [];
    wc.value = null;
    items.value = [];
    locks.value = [];
  }

  // ---------- 仓库与工作副本 ----------

  async function refreshRepos(): Promise<void> {
    const r = await run(() => api.listRepos());
    if (r) repos.value = r;
  }

  async function checkout(repo: string, dir: string, sparse: string[]): Promise<boolean> {
    if (!repo) {
      note('请先选择仓库', 'err');
      return false;
    }
    if (!dir) {
      note('请先选择检出目录', 'err');
      return false;
    }
    const r = await run(
      () => api.checkout(repo, dir, sparse.length > 0 ? sparse : undefined),
      `已检出到 ${dir}`,
    );
    if (!r) return false;
    wc.value = r;
    await refreshConfig(); // recent 变了（否则「打开已有副本」看到的是过期列表）
    await refreshStatus();
    await refreshConflicts();
    startLockPolling();
    return true;
  }

  async function openWorkingCopy(dir: string): Promise<boolean> {
    const r = await run(() => api.open(dir), `已打开 ${dir}`);
    if (!r) return false;
    wc.value = r;
    await refreshConfig();
    await refreshStatus();
    await refreshConflicts();
    startLockPolling();
    return true;
  }

  async function closeWorkingCopy(): Promise<void> {
    stopLockPolling();
    await run(() => api.close());
    wc.value = null;
    items.value = [];
    locks.value = [];
    conflicts.value = [];
  }

  async function refreshStatus(forceHash = false): Promise<void> {
    if (!wc.value) return;
    const r = await run(() => api.status(forceHash));
    if (r) items.value = r;
  }

  // ---------- 冲突（§6.5） ----------

  async function refreshConflicts(): Promise<void> {
    if (!wc.value) {
      conflicts.value = [];
      return;
    }
    const r = await run(() => api.conflicts());
    if (r) conflicts.value = r;
  }

  /** 读某个冲突的三方内容（不走 state，直接返回给视图）。 */
  async function loadConflictSides(path: string): Promise<ConflictSides | null> {
    return run(() => api.conflictSides(path));
  }

  async function resolveConflict(
    path: string,
    choice: ConflictResolution,
    content?: string,
  ): Promise<boolean> {
    const r = await run(
      () => api.resolveConflict(path, choice, content),
      choice === 'theirs' ? `已采用服务端版本：${path}` : `已保留本地版本：${path}`,
    );
    if (!r) return false;
    await refreshConflicts();
    return true;
  }

  // ---------- 变更操作 ----------

  async function add(paths: string[]): Promise<void> {
    if (paths.length === 0) return;
    await run(() => api.add(paths), `已标记 ${paths.length} 项待新增`);
    await refreshStatus();
  }

  async function remove(paths: string[]): Promise<void> {
    if (paths.length === 0) return;
    await run(() => api.remove(paths), `已移除 ${paths.length} 项（可从基线还原）`);
    await refreshStatus();
  }

  /**
   * 读本副本的忽略规则文件内容（读失败返回 null，界面据此提示而不是抹掉用户的内容）。
   *
   * **忽略规则按副本走**（§6.2）：每个仓库要忽略的东西不一样，所以它属于工作副本，
   * 不属于全局设置（真实反馈："每个仓库需要忽略的文件是不同的，设置里是公共的配置"）。
   */
  async function loadIgnoreRules(): Promise<string | null> {
    if (!wc.value) return null;
    try {
      const r = await api.ignoreRules();
      return typeof r.content === 'string' ? r.content : ''; // 缺字段等价于"没有规则"
    } catch (e) {
      note(describe(e), 'err');
      return null;
    }
  }

  /** 保存本副本的忽略规则；写完刷新状态（规则变了，`ignored` 集合跟着变）。 */
  async function saveIgnoreRules(content: string): Promise<boolean> {
    const r = await run(() => api.setIgnoreRules(content), '已保存忽略规则');
    if (!r) return false;
    await refreshStatus();
    return true;
  }

  /** 树上新建目录（§6.5）。目录本身不进提交，放下文件后随文件一起入库。 */
  async function mkdir(path: string): Promise<boolean> {
    if (!path) return false;
    const r = await run(() => api.mkdir(path), `已创建目录 ${path}`);
    if (!r) return false;
    await refreshStatus();
    return true;
  }

  async function revert(paths: string[]): Promise<void> {
    if (paths.length === 0) return;
    await run(() => api.revert(paths), `已还原 ${paths.length} 项`);
    await refreshStatus();
  }

  /**
   * 提交。`message` 允许为空（v0.4.17：服务端本就接受空说明，是客户端多加的必填）。
   *
   * 提交前不需要用户手动加锁：引擎在服务端返回 412 NEEDS_LOCK 时会**自动补锁**再重试
   * （§6.5），因此这里只管提交，补锁产生的锁在成功后统一刷新。
   */
  async function commit(message: string, paths?: string[]): Promise<boolean> {
    // 提交的提示语在拿到 rev 之后才拼得出来，所以这里不传 okText
    const r = await run(() => api.commit(message, paths));
    if (!r) return false;
    note(`提交成功，r${r.rev}（${r.committed.length} 个文件）`);
    if (r.rev !== undefined) wc.value = wc.value ? { ...wc.value, rev: r.rev } : wc.value;
    await refreshStatus();
    await refreshLocks();
    return true;
  }

  async function update(): Promise<void> {
    const r = await run(() => api.update());
    if (!r) return;
    wc.value = wc.value ? { ...wc.value, rev: r.rev } : wc.value;
    progress.value = null;
    const bits = [`更新到 r${r.rev}`];
    if (r.updated.length) bits.push(`${r.updated.length} 个更新`);
    if (r.deleted.length) bits.push(`${r.deleted.length} 个删除`);
    if (r.conflicts.length) bits.push(`${r.conflicts.length} 个冲突`);
    if (r.skipped.length) bits.push(`${r.skipped.length} 个跳过`);
    note(bits.join(' · '), r.conflicts.length ? 'err' : 'ok');
    await refreshStatus();
    await refreshConflicts();
  }

  // ---------- 锁 ----------

  async function refreshLocks(path?: string): Promise<void> {
    if (!wc.value) return;
    const r = await run(() => api.locks(path));
    if (r) locks.value = r;
  }

  /**
   * 本地乐观更新锁列表。
   *
   * **为什么需要它**：界面上的锁标记完全依赖 `GET /locks` 的结果。而那次请求偶发失败时
   * （网络抖动 / 主进程忙），标记会一直停在过期状态 —— 用户看到"锁已经解了、树上还显示
   * 已锁"，而且怎么点都不变（真事故过：同一套 E2E 在打包产物上偶发红，源码树上全绿）。
   *
   * 所以：**我方操作成功之后立刻在本地生效**（我知道自己刚锁了什么、刚解了什么），
   * 下一次成功的 `GET /locks` 再对账。这样界面的正确性不再取决于某一次请求的成败。
   */
  /**
   * 静默刷新锁列表：不动 `busy`、不弹提示。
   *
   * 为什么要主动刷：锁是**服务端状态**，别人加锁不会产生本地文件变化，因此文件监听
   * （`wc:changed`）根本不会触发 —— 光靠它，"同事刚锁了某个文件"在界面上要等到用户
   * 自己点一次操作才看得见（用户报过"锁状态对不上、解不开"）。
   */
  async function refreshLocksQuiet(): Promise<void> {
    if (!wc.value) return;
    try {
      locks.value = await api.locks();
    } catch {
      /* 静默：下一轮再试 */
    }
  }

  /** 工作副本打开期间的锁轮询（15s：一次索引范围查询，代价可以忽略）。 */
  const LOCK_POLL_MS = 15_000;
  let lockTimer: ReturnType<typeof setInterval> | null = null;

  function stopLockPolling(): void {
    if (lockTimer) clearInterval(lockTimer);
    lockTimer = null;
  }

  function startLockPolling(): void {
    stopLockPolling();
    lockTimer = setInterval(() => void refreshLocksQuiet(), LOCK_POLL_MS);
  }

  /** 加锁（v0.4.17：只剩文件锁，§5.2）。入口在工作副本目录树上。 */
  async function lock(path: string, comment = ''): Promise<boolean> {
    if (!path) {
      note('请先在目录树里点选一个文件或目录', 'err');
      return false;
    }
    const r = await run(
      () => api.lock(path, { comment: comment || undefined }),
      `已锁定 ${path}`,
    );
    if (!r) return false;
    locks.value = applyLocalLocks(locks.value, [path], [], auth.value?.username ?? '');
    await refreshLocks();
    return true;
  }

  /**
   * 批量加锁（§6.5：树上"锁这个目录"展开为对子树内每个文件加锁）。
   *
   * **继续到底、不中途放弃**：撞上别人持锁的文件时，把其余文件锁上更有用 ——
   * 用户能立刻在树上看到哪些成功了、哪些被别人占着，而不是整批白跑一趟。
   */
  async function lockMany(paths: string[]): Promise<BatchResult> {
    if (paths.length === 0) return { ok: 0, failed: [] };
    busy.value = true;
    const failed: BatchResult['failed'] = [];
    const locked: string[] = [];
    let ok = 0;
    try {
      for (const p of paths) {
        try {
          await api.lock(p);
          ok += 1;
          locked.push(p);
        } catch (e) {
          failed.push({ path: p, message: describe(e) });
        }
      }
    } finally {
      busy.value = false;
    }
    locks.value = applyLocalLocks(locks.value, locked, [], auth.value?.username ?? '');
    await refreshLocks();
    return { ok, failed };
  }

  /** 单个解锁 / 强制解锁（`reason` 给定时即强制解锁，需 admin 且进审计，§5.5）。 */
  async function unlock(path: string, reason?: string): Promise<void> {
    await unlockMany([path], reason);
  }

  /**
   * 批量解锁 / 强制解锁（树上按子树生效）。
   *
   * 同样"继续到底"：一批里有一把锁没权利破，不该把其余能解的也放弃掉。
   */
  async function unlockMany(paths: string[], reason?: string): Promise<BatchResult> {
    if (paths.length === 0) return { ok: 0, failed: [] };
    const breaking = typeof reason === 'string';
    busy.value = true;
    const failed: BatchResult['failed'] = [];
    const released: string[] = [];
    let ok = 0;
    try {
      for (const p of paths) {
        try {
          await api.unlock(p, breaking ? { breakLock: true, reason } : {});
          ok += 1;
          released.push(p);
        } catch (e) {
          failed.push({ path: p, message: describe(e) });
        }
      }
    } finally {
      busy.value = false;
    }
    locks.value = applyLocalLocks(locks.value, [], released, auth.value?.username ?? '');
    await refreshLocks();
    return { ok, failed };
  }

  /** 错误 → 一句话（IPC 错误带 code，直接展示给用户也能看懂）。 */
  function describe(e: unknown): string {
    return e instanceof IpcError ? `${e.message}（${e.code}）` : String(e);
  }

  return {
    // state
    ready,
    info,
    config,
    auth,
    repos,
    wc,
    items,
    locks,
    conflicts,
    progress,
    busy,
    flash,
    // getters
    summary,
    candidates,
    loggedIn,
    // actions
    note,
    run,
    init,
    login,
    logout,
    refreshRepos,
    checkout,
    openWorkingCopy,
    closeWorkingCopy,
    refreshStatus,
    refreshConflicts,
    loadConflictSides,
    resolveConflict,
    add,
    remove,
    mkdir,
    loadIgnoreRules,
    saveIgnoreRules,
    revert,
    commit,
    update,
    refreshConfig,
    refreshLocks,
    refreshLocksQuiet,
    lock,
    lockMany,
    unlock,
    unlockMany,
  };
});
