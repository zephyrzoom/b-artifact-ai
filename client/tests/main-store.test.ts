import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  CONCURRENCY_MAX,
  CONCURRENCY_MIN,
  ConfigStore,
  defaultConfig,
  normalizeConfig,
} from '../src/main/store.js';

let dir: string;
let file: string;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), 'ba-cfg-'));
  file = join(dir, 'config.json');
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

describe('defaultConfig', () => {
  it('默认并发 4、缓存目录指向用户级 ~/.b-artifact/cache', () => {
    const c = defaultConfig();
    expect(c.concurrency).toBe(4);
    expect(c.cacheDir.endsWith(join('.b-artifact', 'cache'))).toBe(true);
    expect(c.servers).toEqual([]);
    expect(c.recent).toEqual([]);
  });
});

describe('normalizeConfig（脏字段逐个兜底）', () => {
  it('非对象 → 全部默认', () => {
    expect(normalizeConfig(null)).toEqual(defaultConfig());
    expect(normalizeConfig([])).toEqual(defaultConfig());
    expect(normalizeConfig('x')).toEqual(defaultConfig());
  });

  it('并发数被夹到合法区间并取整', () => {
    expect(normalizeConfig({ concurrency: 999 }).concurrency).toBe(CONCURRENCY_MAX);
    expect(normalizeConfig({ concurrency: 0 }).concurrency).toBe(CONCURRENCY_MIN);
    expect(normalizeConfig({ concurrency: 2.6 }).concurrency).toBe(3);
    expect(normalizeConfig({ concurrency: Number.NaN }).concurrency).toBe(4);
  });

  it('servers 过滤非字符串并限长', () => {
    const c = normalizeConfig({ servers: ['a', 1, null, 'b'] });
    expect(c.servers).toEqual(['a', 'b']);
    expect(normalizeConfig({ servers: Array.from({ length: 30 }, (_, i) => `s${i}`) }).servers).toHaveLength(10);
  });

  it('recent 丢掉没有 dir 的条目并补齐字段类型', () => {
    const c = normalizeConfig({
      recent: [{ dir: '/a', repo: 'r' }, { repo: 'x' }, 'junk', { dir: '/b', lastOpenedAt: 'y' }],
    });
    expect(c.recent).toHaveLength(2);
    expect(c.recent[0]).toEqual({ dir: '/a', repo: 'r', server: '', lastOpenedAt: 0 });
    expect(c.recent[1]!.lastOpenedAt).toBe(0);
  });

  it('检出默认路径：缺省为空串，坏值不污染其它字段', () => {
    expect(defaultConfig().defaultCheckoutParent).toBe('');
    const c = normalizeConfig({ defaultCheckoutParent: 42 });
    expect(c.defaultCheckoutParent).toBe('');
    const ok = normalizeConfig({ defaultCheckoutParent: '/w' });
    expect(ok.defaultCheckoutParent).toBe('/w');
  });

  it('一个字段坏掉不影响其它字段（不做全有或全无）', () => {
    const c = normalizeConfig({ concurrency: 'lots', cacheDir: '/tmp/cache', servers: 'not-an-array' });
    expect(c.concurrency).toBe(4);
    expect(c.cacheDir).toBe('/tmp/cache');
    expect(c.servers).toEqual([]);
  });
});

describe('ConfigStore 持久化', () => {
  it('首次 load 返回默认值且不写盘', () => {
    const s = new ConfigStore(file);
    expect(s.load()).toEqual(defaultConfig());
    expect(existsSync(file)).toBe(false);
  });

  it('save 原子写：落盘内容可再次读回，且不留 .tmp', () => {
    const s = new ConfigStore(file);
    s.save({ ...defaultConfig(), concurrency: 8, servers: ['http://a'] });

    expect(existsSync(`${file}.tmp`)).toBe(false);
    const onDisk = JSON.parse(readFileSync(file, 'utf8')) as { concurrency: number };
    expect(onDisk.concurrency).toBe(8);

    const s2 = new ConfigStore(file);
    expect(s2.load().concurrency).toBe(8);
    expect(s2.load().servers).toEqual(['http://a']);
  });

  it('文件损坏 → 改名留证 + 返回默认值（应用不该因为配置坏了起不来）', () => {
    writeFileSync(file, '{ this is not json');
    const s = new ConfigStore(file);
    expect(s.load()).toEqual(defaultConfig());
    expect(existsSync(`${file}.broken`)).toBe(true);
    expect(existsSync(file)).toBe(false);
  });

  it('update 只改传入字段', () => {
    const s = new ConfigStore(file);
    s.save({ ...defaultConfig(), servers: ['http://a'], cacheDir: '/tmp/x' });
    const next = s.update({ concurrency: 6 });
    expect(next.concurrency).toBe(6);
    expect(next.servers).toEqual(['http://a']);
    expect(next.cacheDir).toBe('/tmp/x');
  });

  it('reset 后重新读盘（内存缓存不会盖住外部修改）', () => {
    const s = new ConfigStore(file);
    s.save({ ...defaultConfig(), concurrency: 2 });
    writeFileSync(file, JSON.stringify({ ...defaultConfig(), concurrency: 16 }));
    expect(s.get().concurrency).toBe(2);
    s.reset();
    expect(s.get().concurrency).toBe(16);
  });
});

describe('服务器与最近工作副本', () => {
  it('rememberServer 去重且最近在前', () => {
    const s = new ConfigStore(file);
    s.rememberServer('http://a');
    s.rememberServer('http://b');
    const c = s.rememberServer('http://a');
    expect(c.servers).toEqual(['http://a', 'http://b']);
  });

  it('服务器列表上限 10，超出丢最旧的', () => {
    const s = new ConfigStore(file);
    for (let i = 0; i < 12; i++) s.rememberServer(`http://s${i}`);
    const c = s.get();
    expect(c.servers).toHaveLength(10);
    expect(c.servers[0]).toBe('http://s11');
    expect(c.servers).not.toContain('http://s0');
  });

  it('forgetServer 移除指定项', () => {
    const s = new ConfigStore(file);
    s.rememberServer('http://a');
    s.rememberServer('http://b');
    expect(s.forgetServer('http://a').servers).toEqual(['http://b']);
  });

  it('rememberRecent 按目录去重并置顶，时间自动补', () => {
    const s = new ConfigStore(file);
    s.rememberRecent({ dir: '/w/a', repo: 'r', server: 'http://s' });
    s.rememberRecent({ dir: '/w/b', repo: 'r', server: 'http://s' });
    const c = s.rememberRecent({ dir: '/w/a', repo: 'r', server: 'http://s' });
    expect(c.recent.map((r) => r.dir)).toEqual(['/w/a', '/w/b']);
    expect(c.recent[0]!.lastOpenedAt).toBeGreaterThan(0);
  });

  it('最近列表上限 20', () => {
    const s = new ConfigStore(file);
    for (let i = 0; i < 25; i++) s.rememberRecent({ dir: `/w/${i}`, repo: 'r', server: 's' });
    expect(s.get().recent).toHaveLength(20);
    expect(s.get().recent[0]!.dir).toBe('/w/24');
  });

  it('dropRecent 移除失效目录（工作副本被删/移走时）', () => {
    const s = new ConfigStore(file);
    s.rememberRecent({ dir: '/w/a', repo: 'r', server: 's' });
    s.rememberRecent({ dir: '/w/b', repo: 'r', server: 's' });
    expect(s.dropRecent('/w/a').recent.map((r) => r.dir)).toEqual(['/w/b']);
  });

  it('destroy 清掉主文件与临时文件', () => {
    const s = new ConfigStore(file);
    s.save(defaultConfig());
    writeFileSync(`${file}.tmp`, 'x');
    s.destroy();
    expect(existsSync(file)).toBe(false);
    expect(existsSync(`${file}.tmp`)).toBe(false);
  });
});
