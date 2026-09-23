/**
 * 工作副本文件监听（§6.5 M4.8 自动同步）。
 *
 * 三条必须钉住的规则：
 *   1. **防抖**：连续事件只重算一次（一个"另存为"可能产生十几个事件）；
 *   2. **忽略**：`.b-artifact/`、`.mine`、忽略规则命中的路径都不该触发重算；
 *   3. **抑制与降级**：自己写的盘不触发；`fs.watch` 起不来必须让上层知道。
 */

import { describe, expect, it, vi } from 'vitest';

import { createWatcher, isNoise, type WatchHandle } from '../src/main/watch.js';

/** 假 watcher：把回调存下来，测试自己"制造事件"。 */
function fakeWatch() {
  const listeners: Array<(rel: string) => void> = [];
  const closed: number[] = [];
  const factory = (_root: string, onEvent: (rel: string) => void): WatchHandle => {
    listeners.push(onEvent);
    const id = listeners.length - 1;
    return { close: () => closed.push(id) };
  };
  const emit = (rel: string): void => {
    for (const l of listeners) l(rel);
  };
  return { factory, emit, closed, get count() { return listeners.length; } };
}

/** 可控时钟 + 可手动推进的定时器（不依赖真实 300ms，测试才稳定）。 */
function fakeClock() {
  let now = 1_000_000;
  const timers: Array<{ at: number; fn: () => void }> = [];
  return {
    now: () => now,
    setTimer: (fn: () => void, ms: number) => {
      const t = { at: now + ms, fn };
      timers.push(t);
      return t as unknown as ReturnType<typeof setTimeout>;
    },
    clearTimer: (t: ReturnType<typeof setTimeout>) => {
      const i = timers.indexOf(t as unknown as { at: number; fn: () => void });
      if (i >= 0) timers.splice(i, 1);
    },
    advance: (ms: number) => {
      now += ms;
      for (const t of [...timers]) {
        if (t.at <= now) {
          timers.splice(timers.indexOf(t), 1);
          t.fn();
        }
      }
    },
  };
}

describe('isNoise', () => {
  it('元数据目录、冲突伴生文件、编辑器临时文件都算噪音', () => {
    expect(isNoise('.b-artifact/wc.db')).toBe(true);
    expect(isNoise('a/.b-artifact/x')).toBe(true);
    expect(isNoise('characters/hero.psd.mine')).toBe(true);
    expect(isNoise('characters/hero.psd~')).toBe(true);
    expect(isNoise('.~lock.hero.psd#')).toBe(false); // 只挡 .# 前缀
    expect(isNoise('.#hero.psd')).toBe(true);
  });

  it('普通路径要触发；拿不到文件名（空串）也必须触发——宁多扫一次，不错过一次', () => {
    expect(isNoise('characters/hero.psd')).toBe(false);
    expect(isNoise('')).toBe(false);
  });

  it('忽略规则命中的路径交给调用方判定', () => {
    const ignored = (rel: string): boolean => rel.endsWith('.tmp');
    expect(isNoise('a/b.tmp', ignored)).toBe(true);
    expect(isNoise('a/b.psd', ignored)).toBe(false);
  });
});

describe('createWatcher', () => {
  it('防抖：连续事件只重算一次', async () => {
    const clock = fakeClock();
    const w = fakeWatch();
    const onSettled = vi.fn();
    const watcher = createWatcher({
      root: '/w',
      watch: w.factory,
      onSettled,
      now: clock.now,
      setTimer: clock.setTimer,
      clearTimer: clock.clearTimer,
    });

    w.emit('a.psd');
    w.emit('b.psd');
    w.emit('c.psd');
    expect(onSettled).not.toHaveBeenCalled();

    clock.advance(300);
    expect(onSettled).toHaveBeenCalledTimes(1);
    watcher.stop();
  });

  it('噪音事件不排期（否则一次 wc.db 写入就会触发全量扫描）', () => {
    const clock = fakeClock();
    const w = fakeWatch();
    const onSettled = vi.fn();
    const watcher = createWatcher({
      root: '/w',
      watch: w.factory,
      onSettled,
      now: clock.now,
      setTimer: clock.setTimer,
      clearTimer: clock.clearTimer,
    });

    w.emit('.b-artifact/wc.db');
    w.emit('a.psd.mine');
    clock.advance(1000);
    expect(onSettled).not.toHaveBeenCalled();
    watcher.stop();
  });

  it('自写入抑制：窗口内的变化不**立刻**触发，窗口一过补算一次（推迟，不是丢弃）', () => {
    const clock = fakeClock();
    const w = fakeWatch();
    const onSettled = vi.fn();
    const watcher = createWatcher({
      root: '/w',
      watch: w.factory,
      onSettled,
      now: clock.now,
      setTimer: clock.setTimer,
      clearTimer: clock.clearTimer,
    });

    watcher.suppress(1500);
    expect(watcher.isSuppressed()).toBe(true);
    w.emit('a.psd');
    clock.advance(300);
    expect(onSettled).not.toHaveBeenCalled(); // 防抖到点时仍在窗口内 → 不立刻算

    // 关键回归：这一次事件**没有被丢掉**，窗口结束时会补算一次。
    // 丢掉的话，"保存忽略规则后马上改别的文件"这类操作就永远不反映到界面上（真事故过）。
    clock.advance(2000);
    expect(watcher.isSuppressed()).toBe(false);
    expect(onSettled).toHaveBeenCalledTimes(1);

    w.emit('a.psd');
    clock.advance(300);
    expect(onSettled).toHaveBeenCalledTimes(2);
    watcher.stop();
  });

  it('抑制窗口内的"自己写的那批"事件最多只补算一次（不会每来一个事件扫一遍）', () => {
    const clock = fakeClock();
    const w = fakeWatch();
    const onSettled = vi.fn();
    const watcher = createWatcher({
      root: '/w',
      watch: w.factory,
      onSettled,
      now: clock.now,
      setTimer: clock.setTimer,
      clearTimer: clock.clearTimer,
    });

    watcher.suppress(1500);
    w.emit('a.psd');
    w.emit('b.psd');
    w.emit('c.psd');
    clock.advance(1800);
    expect(onSettled).toHaveBeenCalledTimes(1);
    watcher.stop();
  });

  it('stop 之后不再触发，并且关掉底层句柄', () => {
    const clock = fakeClock();
    const w = fakeWatch();
    const onSettled = vi.fn();
    const watcher = createWatcher({
      root: '/w',
      watch: w.factory,
      onSettled,
      now: clock.now,
      setTimer: clock.setTimer,
      clearTimer: clock.clearTimer,
    });
    watcher.stop();
    w.emit('a.psd');
    clock.advance(500);
    expect(onSettled).not.toHaveBeenCalled();
    expect(w.closed).toEqual([0]);
  });

  it('listen 抛错 → 标记降级并回调（上层据此露出手动刷新）', () => {
    const onDegrade = vi.fn();
    const watcher = createWatcher({
      root: '/w',
      watch: () => {
        throw new Error('ERR_FEATURE_UNAVAILABLE_ON_PLATFORM');
      },
      onSettled: () => {},
      onDegrade,
    });
    expect(watcher.degraded).toBe(true);
    expect(onDegrade).toHaveBeenCalledWith('ERR_FEATURE_UNAVAILABLE_ON_PLATFORM');
    // 降级后 stop 不能炸（没有句柄）
    expect(() => watcher.stop()).not.toThrow();
  });

  it('重算抛错不会让监听器死掉：下一次事件照常触发', async () => {
    const clock = fakeClock();
    const w = fakeWatch();
    let calls = 0;
    const watcher = createWatcher({
      root: '/w',
      watch: w.factory,
      onSettled: () => {
        calls += 1;
        if (calls === 1) throw new Error('扫描失败');
      },
      now: clock.now,
      setTimer: clock.setTimer,
      clearTimer: clock.clearTimer,
    });

    w.emit('a.psd');
    clock.advance(300);
    w.emit('b.psd');
    clock.advance(300);
    expect(calls).toBe(2);
    watcher.stop();
  });
});
