/**
 * 主进程本地配置（§6.5 设置项 + "记住服务器列表"）。
 *
 * 落盘位置 `~/.b-artifact/config.json`（`B_ARTIFACT_HOME` 可重定向）。
 * 两条硬要求：
 *   1. **原子写**：先写 `<file>.tmp` 再 rename，断电/崩溃不会留下半截 JSON；
 *   2. **损坏可恢复**：解析失败时把坏文件改名留证，返回默认值而不是让应用起不来。
 */

import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';

import { artifactHome } from '../core/home.js';
import { defaultCacheDir } from '../core/pristine.js';

export interface RecentEntry {
  /** 工作副本根目录（绝对路径）。 */
  dir: string;
  repo: string;
  server: string;
  lastOpenedAt: number;
}

export interface AppConfig {
  /** 传输并发（§6.5 设置项）。 */
  concurrency: number;
  /** 全局 blob 缓存路径。 */
  cacheDir: string;
  /** 追加的忽略规则（用户在设置里编辑，§6.5）。 */
  /** 最近连接过的服务器（去重、最近在前）。 */
  servers: string[];
  /** 最近打开的工作副本。 */
  recent: RecentEntry[];
  /** 最近一次选择目录的父目录，作为下次默认值。 */
  defaultCheckoutParent: string;
}

export const CONCURRENCY_MIN = 1;
export const CONCURRENCY_MAX = 32;
const SERVERS_MAX = 10;
const RECENT_MAX = 20;

export function defaultConfig(): AppConfig {
  return {
    concurrency: 4,
    cacheDir: defaultCacheDir(),
    servers: [],
    recent: [],
    defaultCheckoutParent: '',
  };
}

/** 配置文件路径。 */
export function configPath(home: string = artifactHome()): string {
  return join(home, 'config.json');
}

function clampConcurrency(n: number): number {
  if (!Number.isFinite(n)) return defaultConfig().concurrency;
  return Math.min(CONCURRENCY_MAX, Math.max(CONCURRENCY_MIN, Math.round(n)));
}

/** 把任意来源的对象规整成合法配置（脏字段逐个兜底，不做"全有或全无"）。 */
export function normalizeConfig(raw: unknown): AppConfig {
  const d = defaultConfig();
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return d;
  const r = raw as Record<string, unknown>;

  const servers = Array.isArray(r['servers'])
    ? (r['servers'] as unknown[]).filter((s): s is string => typeof s === 'string').slice(0, SERVERS_MAX)
    : [];

  const recent = Array.isArray(r['recent'])
    ? (r['recent'] as unknown[])
        .filter((e): e is Record<string, unknown> => !!e && typeof e === 'object')
        .map((e) => ({
          dir: typeof e['dir'] === 'string' ? e['dir'] : '',
          repo: typeof e['repo'] === 'string' ? e['repo'] : '',
          server: typeof e['server'] === 'string' ? e['server'] : '',
          lastOpenedAt: typeof e['lastOpenedAt'] === 'number' ? e['lastOpenedAt'] : 0,
        }))
        .filter((e) => e.dir !== '')
        .slice(0, RECENT_MAX)
    : [];

  return {
    concurrency: typeof r['concurrency'] === 'number' ? clampConcurrency(r['concurrency']) : d.concurrency,
    cacheDir: typeof r['cacheDir'] === 'string' && r['cacheDir'] ? r['cacheDir'] : d.cacheDir,
    servers,
    recent,
    defaultCheckoutParent:
      typeof r['defaultCheckoutParent'] === 'string' ? r['defaultCheckoutParent'] : '',
  };
}

export class ConfigStore {
  private cfg: AppConfig | null = null;

  constructor(readonly file: string = configPath()) {}

  load(): AppConfig {
    if (this.cfg) return this.cfg;
    if (!existsSync(this.file)) {
      this.cfg = defaultConfig();
      return this.cfg;
    }
    try {
      this.cfg = normalizeConfig(JSON.parse(readFileSync(this.file, 'utf8')));
    } catch {
      // 坏文件改名留证（下次写入会重建），应用照常启动
      try {
        renameSync(this.file, `${this.file}.broken`);
      } catch {
        /* 留证失败不影响启动 */
      }
      this.cfg = defaultConfig();
    }
    return this.cfg;
  }

  get(): AppConfig {
    return this.load();
  }

  save(next: AppConfig): AppConfig {
    const cfg = normalizeConfig(next);
    mkdirSync(dirname(this.file), { recursive: true });
    const tmp = `${this.file}.tmp`;
    writeFileSync(tmp, `${JSON.stringify(cfg, null, 2)}\n`, { mode: 0o600 });
    renameSync(tmp, this.file);
    this.cfg = cfg;
    return cfg;
  }

  update(patch: Partial<AppConfig>): AppConfig {
    return this.save({ ...this.load(), ...patch });
  }

  /** 记住一个服务器地址（最近在前、去重）。 */
  rememberServer(server: string): AppConfig {
    const cfg = this.load();
    const servers = [server, ...cfg.servers.filter((s) => s !== server)].slice(0, SERVERS_MAX);
    return this.save({ ...cfg, servers });
  }

  forgetServer(server: string): AppConfig {
    const cfg = this.load();
    return this.save({ ...cfg, servers: cfg.servers.filter((s) => s !== server) });
  }

  rememberRecent(entry: Omit<RecentEntry, 'lastOpenedAt'> & { lastOpenedAt?: number }): AppConfig {
    const cfg = this.load();
    const next: RecentEntry = { ...entry, lastOpenedAt: entry.lastOpenedAt ?? Date.now() };
    const recent = [
      next,
      ...cfg.recent.filter((r) => r.dir !== next.dir),
    ].slice(0, RECENT_MAX);
    return this.save({ ...cfg, recent });
  }

  dropRecent(dir: string): AppConfig {
    const cfg = this.load();
    return this.save({ ...cfg, recent: cfg.recent.filter((r) => r.dir !== dir) });
  }

  /** 仅供测试：清掉内存缓存，强制下次 load 重新读盘。 */
  reset(): void {
    this.cfg = null;
  }

  /** 删掉配置文件（贴心地连 `*.tmp` 一起清）。 */
  destroy(): void {
    for (const p of [this.file, `${this.file}.tmp`, `${this.file}.broken`]) {
      try {
        rmSync(p, { force: true });
      } catch {
        /* 不存在就算了 */
      }
    }
    this.cfg = null;
  }
}
