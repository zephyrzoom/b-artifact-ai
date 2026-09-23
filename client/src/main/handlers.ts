/**
 * IPC 通道处理器：渲染层的每一个动作都落到这里，再由这里调用**同一个** `core/` 引擎
 * （CLI 用的也是它，§6.4"core 可被 CLI 复用"）。
 *
 * 所有外部能力（网络、文件对话框、系统 API）都通过 `HandlerDeps` 注入，
 * 因此这一层可以在 Vitest 里用替身跑完整流程，不需要真的起 Electron。
 */

import { join } from 'node:path';

import type { ApiClient, LockInfo, RepoSummary, UserInfo } from '../core/api.js';
import { cacheStats, clearCache } from '../core/cache.js';
import { ApiError, WcError } from '../core/errors.js';
import { IGNORE_FILE, IgnoreRules } from '../core/ignore.js';
import { downloadRevisionFile } from '../core/revisions.js';
import type { StatusItem } from '../core/scan.js';
import type { ConflictResolution, WorkingCopy } from '../core/wc.js';
import type { Channel, EventChannel } from '../shared/channels.js';
import { MainError } from './errors.js';
import type { SessionStore, SessionState } from './session.js';
import type { AppConfig, ConfigStore, RecentEntry } from './store.js';
import { createWatcher, SELF_WRITE_MS, type WatchFactory, type Watcher } from './watch.js';

export interface AppInfo {
  version: string;
  electron: string;
  node: string;
  chrome: string;
  platform: string;
  arch: string;
}

export interface HandlerDeps {
  config: ConfigStore;
  session: SessionStore;
  appInfo: AppInfo;
  /** 新建 REST 客户端（token 由主进程注入，渲染层看不到）。 */
  makeClient(opts: { baseUrl: string; token?: string }): ApiClient;
  /** 打开/检出工作副本；返回实例由主进程持有。 */
  openWorkingCopy(root: string): WorkingCopy;
  checkoutWorkingCopy(opts: { repo: string; dir: string; sparse?: string[] }): Promise<WorkingCopy>;
  /** 原生目录选择（取消返回 null）。 */
  pickDir?(title: string): Promise<string | null>;
  /** 在文件管理器中定位路径。 */
  revealPath?(path: string): void;
  /**
   * 文件监听（§6.5 自动同步）。省略 = 不自动同步（测试与降级路径）。
   * 真实实现是 `fs.watch(root, { recursive: true })`。
   */
  watch?: WatchFactory;
  /** 主进程 → 渲染层的事件推送。 */
  emit?(channel: EventChannel, payload: unknown): void;
  /** 日志（测试里可注入收集器）。 */
  log?(message: string): void;
}

export interface WorkingCopyState {
  root: string;
  repo: string;
  rev: number;
  sparse_paths: string[];
  /** 文件监听是否可用；`false` = 界面必须露出手动刷新入口。 */
  watching: boolean;
}

export type Handler = (payload: Record<string, unknown>) => Promise<unknown>;

function asString(v: unknown): string {
  return typeof v === 'string' ? v : '';
}

/** 主进程内的工作副本状态（同一时刻只打开一个，符合单窗口形态）。 */
export class WcSession {
  private wc: WorkingCopy | null = null;
  private watcher: Watcher | null = null;
  /** 监听降级原因（`fs.watch` 起不来时非空），随状态一起给渲染层。 */
  private watchError = '';

  constructor(private readonly deps: HandlerDeps) {}

  get current(): WorkingCopy | null {
    return this.wc;
  }

  /**
   * 接管工作副本并返回状态快照。
   *
   * 关键细节：**同一个实例时不能 close**。提交/更新之后为了把前进的 rev 同步给顶栏，
   * 会拿当前实例再 adopt 一次——如果无条件 close 当前实例，就等于把自己关了，
   * 后续任何操作都会撞 "database is not open"。
   *
   * v0.4.17：每次接管都重启文件监听（换目录了就得盯着新目录）。
   */
  adopt(wc: WorkingCopy): WorkingCopyState {
    if (this.wc && this.wc !== wc) this.wc.close();
    this.wc = wc;
    this.restartWatch();
    const state = this.snapshot();
    this.deps.emit?.('wc:state', state);
    return state;
  }

  /** 当前快照（含监听状态）。 */
  snapshot(): WorkingCopyState {
    const wc = this.wc!;
    return {
      root: wc.root,
      repo: wc.repo,
      rev: wc.revision,
      sparse_paths: wc.sparsePaths,
      watching: this.watcher !== null && !this.watcher.degraded,
    };
  }

  close(): void {
    this.stopWatch();
    this.wc?.close();
    this.wc = null;
    this.deps.emit?.('wc:state', null);
  }

  /**
   * 让监听器忽略接下来这段时间的变化：**我们自己**正在写工作文件。
   *
   * 只在真正会写盘的操作上调用（update / revert / 删除 / 检出 / 冲突解决）。
   * 包住整个操作 + 尾部窗口：落盘往往是异步的，最后一个事件可能晚到几十毫秒。
   */
  async withSelfWrite<T>(fn: () => Promise<T>): Promise<T> {
    this.watcher?.suppress(SELF_WRITE_MS);
    try {
      return await fn();
    } finally {
      this.watcher?.suppress(SELF_WRITE_MS);
    }
  }

  private stopWatch(): void {
    this.watcher?.stop();
    this.watcher = null;
  }

  /**
   * 起监听：变化 → 防抖 → 重算状态 → 推给渲染层。
   *
   * 忽略规则每次重算后重建（用户改了 `.b-artifactignore`，下一次事件就该按新规则过滤）。
   */
  private restartWatch(): void {
    this.stopWatch();
    this.watchError = '';
    const root = this.wc?.root;
    if (!root || !this.deps.watch) return;

    let ignore = this.buildIgnore(root);
    this.watcher = createWatcher({
      root,
      watch: this.deps.watch,
      isIgnored: (rel, isDir) => ignore.matches(rel, isDir),
      onSettled: async () => {
        const wc = this.wc;
        if (!wc) return;
        const items = await wc.status({});
        // 状态算完之后再刷新规则：本次事件已经处理完了，改规则是为了下一批
        ignore = this.buildIgnore(root);
        this.deps.emit?.('wc:changed', items);
      },
      onDegrade: (reason) => {
        this.watchError = reason;
        this.deps.log?.(`文件监听不可用，已降级为手动刷新：${reason}`);
        this.watcher = null;
        if (this.wc) this.deps.emit?.('wc:state', this.snapshot());
      },
    });
  }

  /**
   * 忽略规则 = 本副本的 `.b-artifactignore`（读文件失败等价于空规则）。
   *
   * **只有这一层**：全局设置里那份追加规则已删除（v0.4.19）—— 忽略规则天生是"按仓库"的，
   * 放在公共配置里必然变成"甲要忽略的东西打扰乙"。
   */
  private buildIgnore(root: string): IgnoreRules {
    const rules = new IgnoreRules();
    rules.addFile('', join(root, IGNORE_FILE));
    return rules;
  }

  /** 监听降级原因（空串 = 正常）。 */
  get degraded(): string {
    return this.watchError;
  }
}

/** 当前工作副本为空时的统一报错（渲染层据此提示"请先打开工作副本"）。 */
function requireWc(s: WcSession): WorkingCopy {
  if (!s.current) throw new MainError('NO_WORKING_COPY', '尚未打开工作副本');
  return s.current;
}

/**
 * 仓库级读操作的目标仓库：**一律由调用方显式给出**（IPC 参数里的 `repo`）。
 *
 * v0.4.20 之前这里是 `requireWc(s).repo` —— "你在哪个工作副本里，就看哪个仓库的历史"。
 * 那条心智模型对**历史视图**仍然成立（它本来就是副本视角），但对**仓库页**是错的：
 * 仓库页的用户已经显式选了仓库，凭什么要先打开一个工作副本才能看目录树？
 * （真实反馈："工作副本打开后，仓库部分检出才能出来目录树，不打开显示暂无数据"。）
 * 现在历史视图自己把副本的仓库传上来，仓库页传选中的仓库 —— 服务端不再猜。
 */
function repoParam(p: Record<string, unknown>): string {
  const name = asString(p['repo']);
  // schema 已要求必填，这里再挡一次"空串"：空仓库名打到服务端会变成 `//tree` 这种脏请求，
  // 报错要报在边界上（也让人一眼看出是调用方漏传）。
  if (name === '') throw new MainError('BAD_REQUEST', '缺少参数：repo');
  return name;
}

export function buildHandlers(deps: HandlerDeps): Record<Channel, Handler> {
  const { config, session } = deps;
  const wcs = new WcSession(deps);

  /** 用当前登录态建客户端；未登录抛 UNAUTHENTICATED。 */
  function authedClient(): { client: ApiClient; state: SessionState } {
    const state = session.get();
    if (!state || !state.token) {
      throw new MainError('UNAUTHENTICATED', '尚未登录');
    }
    return { client: deps.makeClient({ baseUrl: state.server, token: state.token }), state };
  }

  function publicUser(u: UserInfo): { username: string; is_admin: boolean; display_name: string } {
    return {
      username: u.username,
      is_admin: !!u.is_admin,
      display_name: u.display_name ?? '',
    };
  }

  return {
    // ---------- 应用 ----------
    'app:info': async () => deps.appInfo,
    'app:config': async () => config.get(),
    'app:setConfig': async (p) => {
      const patch: Partial<AppConfig> = {};
      if (typeof p['concurrency'] === 'number') patch.concurrency = p['concurrency'];
      if (typeof p['cacheDir'] === 'string') patch.cacheDir = p['cacheDir'];
      if (typeof p['defaultCheckoutParent'] === 'string') {
        patch.defaultCheckoutParent = p['defaultCheckoutParent'];
      }
      return config.update(patch);
    },
    /**
     * 选目录（只返回用户选了什么）。
     *
     * **不再顺手写进配置**：以前每次选目录都会把父目录记成"检出默认值"，连"下载旧版本到哪"
     * 这种无关操作都会改掉检出的默认路径 —— 默认路径现在是设置页里的显式配置（v0.4.21）。
     */
    'app:pickDir': async (p) => {
      if (!deps.pickDir) return null;
      return deps.pickDir(asString(p['title']) || '选择目录');
    },

    // ---------- 认证 ----------
    'auth:login': async (p) => {
      const server = asString(p['server']);
      const client = deps.makeClient({ baseUrl: server });
      // ApiError 原样上抛，由 ipc 层映射成错误信封（渲染层按 code 分支）
      const r = await client.login(asString(p['username']), asString(p['password']));
      session.save({ server, username: r.user.username, token: r.token });
      config.rememberServer(server);
      deps.log?.(`登录成功：${r.user.username}@${server}`);
      return { server, ...publicUser(r.user) };
    },
    'auth:logout': async () => {
      const state = session.get();
      if (state?.token) {
        try {
          await deps.makeClient({ baseUrl: state.server, token: state.token }).logout();
        } catch (e) {
          // 令牌可能已在服务端失效：登出必须"总能成功"，否则用户会被卡住
          if (!(e instanceof ApiError)) throw e;
          deps.log?.(`logout 失败（忽略）：${e.code}`);
        }
      }
      session.clear();
      wcs.close();
      return {};
    },
    'auth:state': async () => {
      const s = session.get();
      return s ? { server: s.server, username: s.username } : null;
    },
    'auth:forgetServer': async (p) => config.forgetServer(asString(p['server'])),

    // ---------- 仓库 ----------
    'repos:list': async (): Promise<RepoSummary[]> => {
      const { client } = authedClient();
      return client.listRepos();
    },

    // ---------- 工作副本 ----------
    'wc:recent': async (): Promise<RecentEntry[]> => config.get().recent,
    'wc:checkout': async (p) => {
      const { client, state } = authedClient();
      const wc = await deps.checkoutWorkingCopy({
        repo: asString(p['repo']),
        dir: asString(p['dir']),
        sparse: p['sparse'] as string[] | undefined,
      });
      const snapshot = wcs.adopt(wc);
      config.rememberRecent({ dir: wc.root, repo: wc.repo, server: state.server });
      void client;
      return snapshot;
    },
    'wc:open': async (p) => {
      const root = asString(p['dir']);
      try {
        return wcs.adopt(deps.openWorkingCopy(root));
      } catch (e) {
        if (e instanceof WcError && e.code === 'NOT_A_WORKING_COPY') {
          config.dropRecent(root);
        }
        throw e;
      }
    },
    'wc:close': async () => {
      wcs.close();
      return {};
    },
    'wc:status': async (p): Promise<StatusItem[]> => {
      const wc = requireWc(wcs);
      return wc.status({ forceHash: p['forceHash'] === true });
    },
    'wc:add': async (p) => {
      const wc = requireWc(wcs);
      return { added: wc.add(p['paths'] as string[]) };
    },
    'wc:remove': async (p) => {
      // 删文件是我们自己写的盘：抑制监听，免得刚删完又触发一轮状态重算
      await wcs.withSelfWrite(() => requireWc(wcs).remove(p['paths'] as string[]));
      return {};
    },
    'wc:revert': async (p) => {
      await wcs.withSelfWrite(() => requireWc(wcs).revert(p['paths'] as string[]));
      return {};
    },
    'wc:mkdir': async (p) => {
      await requireWc(wcs).mkdir(asString(p['path']));
      return {};
    },
    'wc:ignoreRules': async () => ({ content: requireWc(wcs).readIgnoreFile() }),
    'wc:setIgnoreRules': async (p) => {
      // 自己写的盘：抑制监听，免得刚保存又触发一轮状态重算
      await wcs.withSelfWrite(() => requireWc(wcs).writeIgnoreFile(asString(p['content'])));
      return {};
    },
    'wc:commit': async (p) => {
      const wc = requireWc(wcs);
      const outcome = await wc.commit({
        message: asString(p['message']),
        paths: p['paths'] as string[] | undefined,
        onProgress: (e) => deps.emit?.('wc:progress', { ...e, phase: 'commit' }),
      });
      wcs.adopt(wc);
      return outcome;
    },
    'wc:update': async () => {
      const wc = requireWc(wcs);
      // update 会成批覆盖工作文件（400ms 以上的抑制窗口覆盖整个操作 + 尾部事件）
      const outcome = await wcs.withSelfWrite(() =>
        wc.update({
          onProgress: (e) => deps.emit?.('wc:progress', { ...e, phase: 'update' }),
        }),
      );
      wcs.adopt(wc);
      return outcome;
    },

    // ---------- 冲突（§6.5） ----------
    'wc:conflicts': async () => requireWc(wcs).conflicts(),
    'wc:conflictSides': async (p) => requireWc(wcs).conflictSides(asString(p['path'])),
    'wc:resolveConflict': async (p) => {
      const wc = requireWc(wcs);
      await wcs.withSelfWrite(() =>
        wc.resolveConflict(asString(p['path']), {
          choice: p['choice'] as ConflictResolution,
          content: typeof p['content'] === 'string' ? p['content'] : undefined,
        }),
      );
      return {};
    },

    // ---------- 历史修订（§6.5） ----------
    'repo:log': async (p) => {
      const { client, state } = authedClient();
      const prefix = typeof p['prefix'] === 'string' ? p['prefix'] : '';
      void state;
      return client.log(repoParam(p), {
        limit: typeof p['limit'] === 'number' ? p['limit'] : undefined,
        offset: typeof p['offset'] === 'number' ? p['offset'] : undefined,
        prefix: prefix || undefined,
      });
    },
    'repo:treeAt': async (p) => {
      const { client } = authedClient();
      const path = typeof p['path'] === 'string' ? p['path'] : '';
      return client.tree(repoParam(p), {
        rev: typeof p['rev'] === 'number' ? p['rev'] : 0,
        prefix: path,
        depth: typeof p['depth'] === 'number' ? p['depth'] : 1,
      });
    },
    'repo:downloadRevision': async (p) => {
      const { client } = authedClient();
      return downloadRevisionFile(client, repoParam(p), {
        path: asString(p['path']),
        rev: typeof p['rev'] === 'number' ? p['rev'] : 0,
        targetDir: asString(p['targetDir']),
      });
    },

    // ---------- 配置与缓存（§6.5 设置视图） ----------
    'config:cacheStats': async () => cacheStats(config.get().cacheDir),
    'config:clearCache': async (p) => clearCache(config.get().cacheDir, { keepTmp: p['keepTmp'] === true }),

    // ---------- 锁 ----------
    'wc:locks': async (p): Promise<LockInfo[]> => {
      const wc = requireWc(wcs);
      const path = p['path'];
      return wc.listLocks(typeof path === 'string' && path ? path : undefined);
    },
    'wc:lock': async (p) => {
      const wc = requireWc(wcs);
      // v0.4.17：只剩文件锁（§5.2）
      return wc.lock(asString(p['path']), {
        comment: typeof p['comment'] === 'string' ? p['comment'] : undefined,
      });
    },
    'wc:unlock': async (p) => {
      await requireWc(wcs).unlock(asString(p['path']), {
        breakLock: p['breakLock'] === true,
        reason: typeof p['reason'] === 'string' ? p['reason'] : undefined,
      });
      return {};
    },

    // ---------- 系统 ----------
    'shell:revealPath': async (p) => {
      if (!deps.revealPath) return { revealed: false };
      deps.revealPath(asString(p['path']));
      return { revealed: true };
    },
  };
}

export { WcSession as WorkingCopySession };
