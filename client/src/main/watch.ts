/**
 * 工作副本文件监听（§6.5 M4.8：状态自动同步）。
 *
 * 目标：用户在 Finder / 资源管理器里改了文件，界面**自己**就更新，不需要按"刷新状态"。
 *
 * 三层判断，缺一层就会出问题：
 *
 * 1. **忽略自己的元数据**：`.b-artifact/` 下全是 wc.db、pristine、wc.lock——每次 status
 *    都会读它，若把它的变化也算成"用户改动"就会自激成死循环。
 * 2. **忽略规则命中的路径**：`.b-artifactignore` + 用户在设置里追加的规则。这些路径本来
 *    就不参与状态判定，给它们触发一次全量扫描纯属浪费（大仓库上一次扫描很贵）。
 * 3. **抑制自己写入的文件**：update / revert / 检出落盘时我们会大量写工作文件，那本来就
 *    会刷新状态；再让 watcher 触发一轮只是空转，还会把 `busy` 标记搅乱。
 *
 * 另一个硬要求是**降级**：`fs.watch` 在超大目录树或某些平台组合上会直接抛错。
 * 这种情况必须让上层知道（`degraded`），退回"操作后刷新 + 手动刷新按钮"，
 * **不能让状态停在过期视图上**。
 *
 * 本模块刻意不 import `node:fs`：真实监听从 `WatchFactory` 注入，
 * 于是防抖、抑制、忽略规则的判定都能用假时钟 + 假 watcher 单测。
 */

import { WC_DIR } from '../core/db.js';

/** 真实 `fs.watch` 的替身面：返回一个能停掉的句柄。 */
export interface WatchHandle {
  close(): void;
}

/**
 * 监听工厂。第二个参数是**相对工作副本根**的变更路径（posix；拿不到文件名时给 `''`，
 * 表示"根下有变化但不知道是哪个"——这种情况不能丢，宁可多扫一次）。
 */
export type WatchFactory = (root: string, onEvent: (rel: string) => void) => WatchHandle;

export const DEBOUNCE_MS = 300;
/** 自写入抑制窗口：操作期间 + 操作后各一段时间（落盘往往是异步的，尾部事件会晚到）。 */
export const SELF_WRITE_MS = 1500;

export interface WatcherOptions {
  root: string;
  watch: WatchFactory;
  /** 防抖结束后调用：重算状态并推送。 */
  onSettled: () => void | Promise<void>;
  /** 额外忽略规则（`.b-artifactignore` / 用户追加规则），入参是仓库内相对路径。 */
  isIgnored?: (rel: string, isDir: boolean) => boolean;
  debounceMs?: number;
  /** 可注入的时钟与定时器：让"防抖"能被确定性地单测，不必 sleep 300ms。 */
  now?: () => number;
  setTimer?: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clearTimer?: (t: ReturnType<typeof setTimeout>) => void;
  /** 监听不可用（`watch` 抛错）时回调一次，供上层记录/推送降级状态。 */
  onDegrade?: (reason: string) => void;
}

export interface Watcher {
  /** 停止监听（幂等）。 */
  stop(): void;
  /** 合并窗口内的事件（主要给测试用；真实场景由防抖自动处理）。 */
  flush(): Promise<void>;
  /** 告诉监听器"接下来的变化是我自己写的"，窗口结束后自动恢复。 */
  suppress(ms?: number): void;
  /** 是否处于抑制窗口内。 */
  isSuppressed(): boolean;
  /** 监听不可用（已降级为手动刷新）。 */
  readonly degraded: boolean;
}

/**
 * 该路径是否根本不值得触发一次状态重算。
 *
 * `rel === ''` 表示"根有变化但不知道具体是谁"——**必须触发**（宁多扫一次，不错过一次）。
 */
export function isNoise(rel: string, isIgnored?: (rel: string, isDir: boolean) => boolean): boolean {
  if (rel === '') return false;
  const segs = rel.split('/');
  // 元数据目录（任何深度出现同名目录都要挡：部分检出/嵌套工作副本都可能出现）
  if (segs.includes(WC_DIR)) return true;
  // 本地临时文件：`.mine`（冲突伴生）与编辑器交换文件，都不进版本控制
  const name = segs[segs.length - 1]!;
  if (name.endsWith('.mine') || name.endsWith('~') || name.startsWith('.#')) return true;
  return isIgnored?.(rel, false) ?? false;
}

/** 创建防抖 + 抑制 + 忽略规则三合一的工作副本监听器。 */
export function createWatcher(opts: WatcherOptions): Watcher {
  const debounceMs = opts.debounceMs ?? DEBOUNCE_MS;
  const now = opts.now ?? (() => Date.now());
  const setTimer = opts.setTimer ?? ((fn: () => void, ms: number) => setTimeout(fn, ms));
  const clearTimer = opts.clearTimer ?? ((t: ReturnType<typeof setTimeout>) => clearTimeout(t));
  let timer: ReturnType<typeof setTimeout> | null = null;
  let handle: WatchHandle | null = null;
  let stopped = false;
  let degraded = false;
  let suppressUntil = 0;
  /** 上一轮触发的 promise，供 `flush()` 等待（测试里要确定性）。 */
  let pending: Promise<void> = Promise.resolve();

  const fire = (): void => {
    timer = null;
    if (stopped) return;
    if (now() < suppressUntil) {
      // ⚠️ **不能静默丢弃**。抑制窗口里进来的事件未必是"我们自己写的"：用户完全可能在
      // 应用写盘后的 1.5s 内又改了别的文件（保存忽略规则 → 立刻改一个源文件），丢掉就等于
      // 那次变化**永远**不反映到界面上（要等下一次别的事件）。
      // 改成**推迟到窗口结束再算**：最坏多扫一次（自己写的那批本来就该被抑制），但不会漏。
      schedule(suppressUntil - now());
      return;
    }
    // 重算失败不该让监听器"从此失聪"：下一次文件变化还会再试。
    // 注意**同步抛错也要吞**——`Promise.resolve(fn())` 会先同步执行 fn，
    // 只用 `.catch()` 接不住同步异常（这条是单测逼出来的）。
    try {
      pending = Promise.resolve(opts.onSettled()).catch(() => {});
    } catch {
      pending = Promise.resolve();
    }
  };

  const schedule = (delay = debounceMs): void => {
    if (timer) clearTimer(timer);
    timer = setTimer(fire, delay);
  };

  try {
    handle = opts.watch(opts.root, (rel) => {
      if (stopped) return;
      if (isNoise(rel, opts.isIgnored)) return;
      schedule();
    });
  } catch (e) {
    degraded = true;
    handle = null;
    opts.onDegrade?.((e as Error).message ?? String(e));
  }

  return {
    get degraded() {
      return degraded;
    },
    stop() {
      stopped = true;
      if (timer) {
        clearTimer(timer);
        timer = null;
      }
      try {
        handle?.close();
      } catch {
        /* 关不掉不影响主流程 */
      }
      handle = null;
    },
    async flush() {
      if (timer) {
        clearTimer(timer);
        fire();
      }
      await pending;
    },
    suppress(ms = SELF_WRITE_MS) {
      suppressUntil = now() + ms;
    },
    isSuppressed() {
      return now() < suppressUntil;
    },
  };
}
