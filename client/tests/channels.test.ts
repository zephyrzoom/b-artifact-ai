import { describe, expect, it } from 'vitest';

import {
  CHANNELS,
  CHANNEL_SPECS,
  EVENT_CHANNELS,
  isChannel,
  isEventChannel,
  validateChannelPayload,
  validatePayload,
  type Schema,
} from '../src/shared/channels.js';

const schema: Schema = {
  name: { type: 'string', required: true, maxLen: 10 },
  kind: { type: 'string', values: ['file', 'dir'] },
  level: { type: 'number' },
  flag: { type: 'boolean' },
  paths: { type: 'string[]', maxLen: 5 },
  limit: { type: 'number', default: 20 },
};

describe('validatePayload', () => {
  it('必填缺失 → 报错并指出字段名', () => {
    const r = validatePayload(schema, {});
    expect(r).toEqual({ ok: false, message: '缺少参数：name' });
  });

  it('类型不符逐个报错', () => {
    expect(validatePayload(schema, { name: 1 })).toEqual({ ok: false, message: 'name 必须是字符串' });
    expect(validatePayload(schema, { name: 'a', level: 'x' })).toEqual({
      ok: false,
      message: 'level 必须是数字',
    });
    expect(validatePayload(schema, { name: 'a', flag: 1 })).toEqual({
      ok: false,
      message: 'flag 必须是布尔值',
    });
    expect(validatePayload(schema, { name: 'a', paths: 'x' })).toEqual({
      ok: false,
      message: 'paths 必须是字符串数组',
    });
    expect(validatePayload(schema, { name: 'a', paths: [1] })).toEqual({
      ok: false,
      message: 'paths 的元素必须是字符串',
    });
  });

  it('NaN / Infinity 不算数字（否则会静默传进引擎）', () => {
    expect(validatePayload(schema, { name: 'a', level: Number.NaN }).ok).toBe(false);
    expect(validatePayload(schema, { name: 'a', level: Number.POSITIVE_INFINITY }).ok).toBe(false);
  });

  it('字符串与数组元素都受长度上限约束', () => {
    expect(validatePayload(schema, { name: 'x'.repeat(11) })).toEqual({
      ok: false,
      message: 'name 超长（上限 10）',
    });
    expect(validatePayload(schema, { name: 'a', paths: ['123456'] })).toEqual({
      ok: false,
      message: 'paths 的元素超长（上限 5）',
    });
  });

  it('枚举白名单之外的取值被拒', () => {
    expect(validatePayload(schema, { name: 'a', kind: 'symlink' })).toEqual({
      ok: false,
      message: 'kind 取值非法：symlink',
    });
    expect(validatePayload(schema, { name: 'a', kind: 'dir' }).ok).toBe(true);
  });

  it('未知参数一律拒绝（防手滑写成 camelCase 后静默失效）', () => {
    const r = validatePayload(schema, { name: 'a', unknown: 1 });
    expect(r).toEqual({ ok: false, message: '未知参数：unknown' });
  });

  it('缺省值自动填充', () => {
    const r = validatePayload(schema, { name: 'a' });
    expect(r.ok).toBe(true);
    if (r.ok) expect(r.value).toEqual({ name: 'a', limit: 20 });
  });

  it('payload 为 undefined / null 等同于空对象', () => {
    expect(validatePayload({}, undefined)).toEqual({ ok: true, value: {} });
    expect(validatePayload({}, null)).toEqual({ ok: true, value: {} });
  });

  it('数组 / 字符串等非对象载荷被拒', () => {
    expect(validatePayload(schema, [])).toEqual({ ok: false, message: '参数必须是对象' });
    expect(validatePayload(schema, 'name=a')).toEqual({ ok: false, message: '参数必须是对象' });
  });

  it('只返回 schema 里声明过的字段，不透传原始对象', () => {
    const r = validatePayload(schema, { name: 'a', level: 1 });
    expect(r.ok).toBe(true);
    if (r.ok) expect(Object.keys(r.value).sort()).toEqual(['level', 'limit', 'name']);
  });
});

describe('通道表', () => {
  it('白名单非空且包含核心通道', () => {
    expect(CHANNELS.length).toBeGreaterThan(10);
    for (const c of ['app:info', 'auth:login', 'repos:list', 'wc:checkout', 'wc:commit', 'wc:lock']) {
      expect(CHANNELS).toContain(c);
    }
  });

  it('isChannel 只认自己的键（原型链上的属性不算）', () => {
    expect(isChannel('wc:status')).toBe(true);
    expect(isChannel('wc:nope')).toBe(false);
    expect(isChannel('toString')).toBe(false);
    expect(isChannel('')).toBe(false);
  });

  it('事件通道与 invoke 通道不重叠（否则渲染层能自己给自己推消息）', () => {
    for (const e of EVENT_CHANNELS) {
      expect(isChannel(e)).toBe(false);
      expect(isEventChannel(e)).toBe(true);
    }
  });

  it('未知通道的 payload 校验直接失败（不抛异常）', () => {
    expect(validateChannelPayload('wc:nope', {})).toEqual({
      ok: false,
      message: '未知通道：wc:nope',
    });
  });

  it('每个通道的 schema 都是合法形状（type 必须受支持）', () => {
    const allowed = new Set(['string', 'number', 'boolean', 'string[]']);
    for (const [channel, spec] of Object.entries(CHANNEL_SPECS)) {
      for (const [key, field] of Object.entries(spec as Schema)) {
        expect(allowed.has(field.type), `${channel}.${key} 的 type 非法`).toBe(true);
      }
    }
  });

  it('登录通道把三项都标成必填（少一项就不该发请求）', () => {
    expect(validateChannelPayload('auth:login', { server: 'http://x' })).toEqual({
      ok: false,
      message: '缺少参数：username',
    });
  });

  it('lock 的 kind 只允许 file / dir', () => {
    expect(validateChannelPayload('wc:lock', { path: 'a.psd', kind: 'subtree' }).ok).toBe(false);
    expect(validateChannelPayload('wc:lock', { path: 'a.psd' }).ok).toBe(true);
  });

  it('超长路径被挡在 IPC 边界（先于引擎的路径校验）', () => {
    const long = 'a'.repeat(3000);
    expect(validateChannelPayload('wc:add', { paths: [long] }).ok).toBe(false);
  });
});

describe('冲突通道的 schema', () => {
  it('choice 只接受 mine / theirs / merged（避免界面拼错后静默走到默认分支）', () => {
    for (const c of ['mine', 'theirs', 'merged']) {
      expect(validateChannelPayload('wc:resolveConflict', { path: 'a.psd', choice: c }).ok).toBe(true);
    }
    expect(validateChannelPayload('wc:resolveConflict', { path: 'a.psd', choice: 'both' })).toEqual({
      ok: false,
      message: 'choice 取值非法：both',
    });
    expect(validateChannelPayload('wc:resolveConflict', { path: 'a.psd' })).toEqual({
      ok: false,
      message: '缺少参数：choice',
    });
  });

  it('content 可省略（只有 merged 才需要），但超长会被挡在 IPC 边界', () => {
    expect(validateChannelPayload('wc:resolveConflict', { path: 'a.psd', choice: 'theirs' }).ok).toBe(true);
    const huge = 'x'.repeat(2 * 1024 * 1024 + 1);
    expect(validateChannelPayload('wc:resolveConflict', { path: 'a.psd', choice: 'merged', content: huge }).ok).toBe(
      false,
    );
  });

  it('wc:conflictSides 必须带路径', () => {
    expect(validateChannelPayload('wc:conflictSides', {})).toEqual({
      ok: false,
      message: '缺少参数：path',
    });
    expect(validateChannelPayload('wc:conflictSides', { path: 'a/b.psd' }).ok).toBe(true);
  });

  it('wc:conflicts 不需要参数', () => {
    expect(validateChannelPayload('wc:conflicts', {}).ok).toBe(true);
  });
});

describe('历史与缓存通道的 schema', () => {
  it('仓库级读通道**必须带 repo**（仓库页在没有工作副本时也要能看目录树/历史）', () => {
    // v0.4.20：这三个通道以前靠"当前工作副本的仓库"，于是没打开副本时仓库页的目录树是空的
    // 只给 repo 时：前两个（其余参数可选）应当通过；下载还要 path/targetDir（另有用例覆盖）
    for (const ch of ['repo:log', 'repo:treeAt'] as const) {
      expect(validateChannelPayload(ch, {}).ok, ch).toBe(false);
      expect(validateChannelPayload(ch, { repo: 'art' }).ok, ch).toBe(true);
    }
    expect(
      validateChannelPayload('repo:downloadRevision', { repo: 'art', path: 'a.psd', targetDir: '/tmp' }).ok,
    ).toBe(true);
  });

  it('repo:log 的其余参数可选（默认拿第一页）', () => {
    expect(validateChannelPayload('repo:log', { repo: 'art' }).ok).toBe(true);
    expect(
      validateChannelPayload('repo:log', { repo: 'art', limit: 50, offset: 100, prefix: 'props' }).ok,
    ).toBe(true);
  });

  it('repo:log 的 limit/offset 必须是数字（字符串会被静默当默认值用）', () => {
    expect(validateChannelPayload('repo:log', { repo: 'art', limit: '50' })).toEqual({
      ok: false,
      message: 'limit 必须是数字',
    });
  });

  it('repo:treeAt 的 rev 可省略（0 = HEAD）', () => {
    expect(validateChannelPayload('repo:treeAt', { repo: 'art' }).ok).toBe(true);
    expect(
      validateChannelPayload('repo:treeAt', { repo: 'art', rev: 3, path: 'props', depth: 1 }).ok,
    ).toBe(true);
  });

  it('repo:downloadRevision 必须给出仓库、路径与目标目录', () => {
    expect(validateChannelPayload('repo:downloadRevision', { path: 'a.psd' })).toEqual({
      ok: false,
      message: '缺少参数：repo',
    });
    expect(validateChannelPayload('repo:downloadRevision', { repo: 'art', path: 'a.psd' })).toEqual({
      ok: false,
      message: '缺少参数：targetDir',
    });
    expect(validateChannelPayload('repo:downloadRevision', { repo: 'art', targetDir: '/tmp' })).toEqual({
      ok: false,
      message: '缺少参数：path',
    });
    expect(
      validateChannelPayload('repo:downloadRevision', { repo: 'art', rev: 2, path: 'a.psd', targetDir: '/tmp' }).ok,
    ).toBe(true);
  });

  it('repo:downloadRevision 的超长路径被挡在 IPC 边界', () => {
    expect(
      validateChannelPayload('repo:downloadRevision', { path: 'a'.repeat(3000), targetDir: '/tmp' }).ok,
    ).toBe(false);
  });

  it('config:cacheStats 无参；config:clearCache 的 keepTmp 可选且必须是布尔', () => {
    expect(validateChannelPayload('config:cacheStats', {}).ok).toBe(true);
    expect(validateChannelPayload('config:clearCache', {}).ok).toBe(true);
    expect(validateChannelPayload('config:clearCache', { keepTmp: true }).ok).toBe(true);
    expect(validateChannelPayload('config:clearCache', { keepTmp: 'yes' })).toEqual({
      ok: false,
      message: 'keepTmp 必须是布尔值',
    });
  });
});
