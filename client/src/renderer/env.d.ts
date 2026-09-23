/// <reference types="vite/client" />

declare module '*.vue' {
  import type { DefineComponent } from 'vue';
  const component: DefineComponent<Record<string, unknown>, Record<string, unknown>, unknown>;
  export default component;
}

import type {
  AppConfig,
  AppInfo,
  AuthState,
  CommitOutcome,
  CacheClearResult,
  CacheStats,
  ConflictInfo,
  ConflictSides,
  DownloadOutcome,
  LogEntry,
  LockInfo,
  LoginOutcome,
  ProgressEvent,
  RepoSummary,
  StatusItem,
  TreeAtResult,
  UpdateOutcome,
  WorkingCopyState,
} from '../shared/dto';
import type { IpcResult } from '../shared/ipc';

/** 通道 → 返回值的类型映射；`api.ts` 靠它给调用点自动推导类型。 */
export interface BridgeCallMap {
  'app:info': AppInfo;
  'app:config': AppConfig;
  'app:setConfig': AppConfig;
  'app:pickDir': string | null;
  'auth:login': LoginOutcome;
  'auth:logout': Record<string, never>;
  'auth:state': AuthState | null;
  'auth:forgetServer': AppConfig;
  'repos:list': RepoSummary[];
  'wc:recent': AppConfig['recent'];
  'wc:checkout': WorkingCopyState;
  'wc:open': WorkingCopyState;
  'wc:close': Record<string, never>;
  'wc:status': StatusItem[];
  'wc:add': { added: number };
  'wc:remove': Record<string, never>;
  'wc:revert': Record<string, never>;
  'wc:mkdir': Record<string, never>;
  'wc:ignoreRules': Record<string, never>;
  'wc:setIgnoreRules': { content: string };
  'wc:commit': CommitOutcome;
  'wc:update': UpdateOutcome;
  'wc:conflicts': ConflictInfo[];
  'wc:conflictSides': ConflictSides;
  'wc:resolveConflict': Record<string, never>;
  'wc:locks': LockInfo[];
  'wc:lock': LockInfo;
  'wc:unlock': Record<string, never>;

  'repo:log': { items: LogEntry[]; total: number };
  'repo:treeAt': TreeAtResult;
  'repo:downloadRevision': DownloadOutcome;

  'config:cacheStats': CacheStats;
  'config:clearCache': CacheClearResult;
  'shell:revealPath': { revealed: boolean };
}

export interface BridgeEventMap {
  'wc:progress': ProgressEvent;
  'wc:state': WorkingCopyState | null;
  /** 文件监听触发的自动同步（§6.5）。 */
  'wc:changed': StatusItem[];
}

/** preload 暴露的窄接口（形状与 `src/preload.ts` 的 `buildBridge` 一致）。 */
export interface BartifactBridge {
  invoke(channel: string, payload?: unknown): Promise<IpcResult<unknown>>;
  on(channel: string, listener: (payload: unknown) => void): () => void;
  channels: readonly string[];
}

declare global {
  interface Window {
    bartifact?: BartifactBridge;
  }
}

export {};
