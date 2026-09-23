/**
 * IPC 通道契约（主进程 / preload / 渲染层共用的唯一事实源）。
 *
 * 设计要点：
 *   - **白名单就是这张表**：`preload` 只暴露这里列出的通道，主进程只为这里注册处理器；
 *     `ipc.ts` 在注册时会校验"处理器没有对应通道定义"→ 直接抛，避免手滑加了通道却漏了校验。
 *   - 每个通道自带 payload schema，进主进程第一件事就是按 schema 校验，
 *     渲染层被攻破也伪造不出越界参数（§6.4 安全基线）。
 *   - 依赖零第三方校验库：schema 只支持本项目实际用到的几种形状，行为完全可单测。
 */

// ---------- 极简 schema ----------

export type FieldType = 'string' | 'number' | 'boolean' | 'string[]';

export interface Field {
  type: FieldType;
  required?: boolean;
  /** 字符串/数组元素的长度上限（防超大 payload）。 */
  maxLen?: number;
  /** 字符串枚举白名单。 */
  values?: readonly string[];
  /** 缺省值（缺省即非必填）。 */
  default?: unknown;
}

export type Schema = Record<string, Field>;

export interface Validated {
  ok: true;
  value: Record<string, unknown>;
}
export interface Invalid {
  ok: false;
  message: string;
}

/** 按 schema 校验 payload；返回规范化后的对象（缺省值已填充）。 */
export function validatePayload(schema: Schema, input: unknown): Validated | Invalid {
  const src = input === undefined || input === null ? {} : input;
  if (typeof src !== 'object' || Array.isArray(src)) {
    return { ok: false, message: '参数必须是对象' };
  }
  const rec = src as Record<string, unknown>;
  const out: Record<string, unknown> = {};

  for (const key of Object.keys(rec)) {
    if (!(key in schema)) return { ok: false, message: `未知参数：${key}` };
  }

  for (const [key, field] of Object.entries(schema)) {
    const raw = rec[key];
    if (raw === undefined) {
      if (field.default !== undefined) {
        out[key] = field.default;
        continue;
      }
      if (field.required) return { ok: false, message: `缺少参数：${key}` };
      continue;
    }

    switch (field.type) {
      case 'string': {
        if (typeof raw !== 'string') return { ok: false, message: `${key} 必须是字符串` };
        if (field.maxLen !== undefined && raw.length > field.maxLen) {
          return { ok: false, message: `${key} 超长（上限 ${field.maxLen}）` };
        }
        if (field.values && !field.values.includes(raw)) {
          return { ok: false, message: `${key} 取值非法：${raw}` };
        }
        out[key] = raw;
        break;
      }
      case 'number': {
        if (typeof raw !== 'number' || !Number.isFinite(raw)) {
          return { ok: false, message: `${key} 必须是数字` };
        }
        out[key] = raw;
        break;
      }
      case 'boolean': {
        if (typeof raw !== 'boolean') return { ok: false, message: `${key} 必须是布尔值` };
        out[key] = raw;
        break;
      }
      case 'string[]': {
        if (!Array.isArray(raw)) return { ok: false, message: `${key} 必须是字符串数组` };
        for (const item of raw) {
          if (typeof item !== 'string') return { ok: false, message: `${key} 的元素必须是字符串` };
          if (field.maxLen !== undefined && item.length > field.maxLen) {
            return { ok: false, message: `${key} 的元素超长（上限 ${field.maxLen}）` };
          }
        }
        out[key] = raw;
        break;
      }
    }
  }

  return { ok: true, value: out };
}

// ---------- 通道表 ----------

/** 路径类参数的长度上限（与 §6.6 的全路径 1024 字节上限对齐，留出富余）。 */
const PATH_MAX = 2048;

export const CHANNEL_SPECS = {
  // 应用信息与配置
  'app:info': {},
  'app:config': {},
  'app:setConfig': {
    concurrency: { type: 'number' },
    cacheDir: { type: 'string', maxLen: PATH_MAX },
    /** 检出目录的默认父目录（设置页配置；检出时用它 + 仓库名）。 */
    defaultCheckoutParent: { type: 'string', maxLen: PATH_MAX },
  },
  'app:pickDir': { title: { type: 'string', maxLen: 200 } },

  // 登录（token 只留在主进程，绝不进渲染层）
  'auth:login': {
    server: { type: 'string', required: true, maxLen: 512 },
    username: { type: 'string', required: true, maxLen: 200 },
    password: { type: 'string', required: true, maxLen: 1024 },
  },
  'auth:logout': {},
  /** 当前登录态（不含 token）。 */
  'auth:state': {},
  /** 历史服务器列表（"记住服务器列表"，§6.5）。 */
  'auth:forgetServer': { server: { type: 'string', required: true, maxLen: 512 } },

  // 仓库
  'repos:list': {},

  // 工作副本
  'wc:recent': {},
  'wc:checkout': {
    repo: { type: 'string', required: true, maxLen: 200 },
    dir: { type: 'string', required: true, maxLen: PATH_MAX },
    sparse: { type: 'string[]', maxLen: PATH_MAX },
  },
  'wc:open': { dir: { type: 'string', required: true, maxLen: PATH_MAX } },
  'wc:close': {},
  'wc:status': { forceHash: { type: 'boolean' } },
  'wc:add': { paths: { type: 'string[]', required: true, maxLen: PATH_MAX } },
  'wc:remove': { paths: { type: 'string[]', required: true, maxLen: PATH_MAX } },
  'wc:revert': { paths: { type: 'string[]', required: true, maxLen: PATH_MAX } },
  /** 树上新建目录（§6.5 直接增删）。 */
  'wc:mkdir': { path: { type: 'string', required: true, maxLen: PATH_MAX } },
  /** 本副本的忽略规则（§6.2）：读 `.b-artifactignore` / 写它。 */
  'wc:ignoreRules': {},
  'wc:setIgnoreRules': { content: { type: 'string', required: true, maxLen: 65536 } },
  /** v0.4.17：**提交说明可空**（服务端 `message` 本就是可选），所以不再是 required。 */
  'wc:commit': { message: { type: 'string', maxLen: 10000 }, paths: { type: 'string[]', maxLen: PATH_MAX } },
  'wc:update': {},

  // 冲突（§6.5 冲突视图）
  'wc:conflicts': {},
  'wc:conflictSides': { path: { type: 'string', required: true, maxLen: PATH_MAX } },
  'wc:resolveConflict': {
    path: { type: 'string', required: true, maxLen: PATH_MAX },
    choice: { type: 'string', required: true, values: ['mine', 'theirs', 'merged'] },
    /** 合并结果（`choice === 'merged'` 时必填）。上限 2 MiB，与文本对比上限同量级。 */
    content: { type: 'string', maxLen: 2 * 1024 * 1024 },
  },

  // 锁
  'wc:locks': { path: { type: 'string', maxLen: PATH_MAX } },
  /** v0.4.17：只剩文件锁，`kind` 已删除（§5.2）。加锁入口在工作副本目录树上。 */
  'wc:lock': {
    path: { type: 'string', required: true, maxLen: PATH_MAX },
    comment: { type: 'string', maxLen: 500 },
  },
  'wc:unlock': {
    path: { type: 'string', required: true, maxLen: PATH_MAX },
    breakLock: { type: 'boolean' },
    reason: { type: 'string', maxLen: 500 },
  },

  // 历史修订（§6.5 历史视图）
  'repo:log': {
    /** **显式给仓库**：仓库页在没有打开工作副本时也要能看历史/目录树（§6.5 部分检出）。 */
    repo: { type: 'string', required: true, maxLen: PATH_MAX },
    limit: { type: 'number' },
    offset: { type: 'number' },
    prefix: { type: 'string', maxLen: PATH_MAX },
  },
  'repo:treeAt': {
    repo: { type: 'string', required: true, maxLen: PATH_MAX },
    /** 0 / 缺省 = HEAD。 */
    rev: { type: 'number' },
    path: { type: 'string', maxLen: PATH_MAX },
    depth: { type: 'number' },
  },
  'repo:downloadRevision': {
    repo: { type: 'string', required: true, maxLen: PATH_MAX },
    path: { type: 'string', required: true, maxLen: PATH_MAX },
    rev: { type: 'number' },
    targetDir: { type: 'string', required: true, maxLen: PATH_MAX },
  },

  // 配置与缓存（§6.5 设置视图）
  'config:cacheStats': {},
  'config:clearCache': { keepTmp: { type: 'boolean' } },

  // 系统
  'shell:revealPath': { path: { type: 'string', required: true, maxLen: PATH_MAX } },
} as const satisfies Record<string, Schema>;

export type Channel = keyof typeof CHANNEL_SPECS;

/** 白名单通道名（preload 与主进程共用）。 */
export const CHANNELS = Object.keys(CHANNEL_SPECS) as Channel[];

/** 主进程 → 渲染层的事件通道（单向推送，渲染层只能监听）。 */
export const EVENT_CHANNELS = [
  'wc:progress',
  'wc:state',
  /** 文件监听触发的状态重算结果（§6.5 自动同步）。载荷是 `StatusItem[]`。 */
  'wc:changed',
] as const;
export type EventChannel = (typeof EVENT_CHANNELS)[number];

export function isChannel(v: string): v is Channel {
  return Object.prototype.hasOwnProperty.call(CHANNEL_SPECS, v);
}

export function isEventChannel(v: string): v is EventChannel {
  return (EVENT_CHANNELS as readonly string[]).includes(v);
}

/** 按通道名校验 payload。 */
export function validateChannelPayload(channel: string, payload: unknown): Validated | Invalid {
  if (!isChannel(channel)) return { ok: false, message: `未知通道：${channel}` };
  return validatePayload(CHANNEL_SPECS[channel] as Schema, payload);
}
