/**
 * b-artifact 客户端 Electron 冒烟（真实应用 + 真实服务端 + 真实 IPC）
 *
 * 为什么用 CDP 驱动而不是在 app 里塞测试钩子：往主进程加"测试模式"会让被测对象
 * 与线上跑的不是同一份代码。这里改为给 Electron 开 `--remote-debugging-port`，
 * 从外部驱动**渲染层**——于是每一次点击都真的走
 * `渲染层 → preload(contextBridge) → IPC 白名单校验 → 主进程处理器 → core 引擎 → 服务端`，
 * 链路完整性由真实进程保证。
 *
 * 覆盖（对应 §6.5 的工作副本闭环）：
 *   启动与 preload 注入 → 错误密码被拒 → 登录 → 仓库列表 → 检出 →
 *   本地新增 → 状态刷新 → 标记新增 → 提交 → 外部提交后更新 → 加锁 / 解锁 →
 *   工作副本落盘校验
 *
 * 用法：
 *   node scripts/electron_smoke.mjs --base http://127.0.0.1:18328 \
 *     --electron <Electron 二进制> --app <client 目录> --work <工作目录> \
 *     --user admin --password 'xxx' [--shots <目录>]
 */

import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';

// ---------------------------------------------------------------- 参数

function arg(name, fallback = null) {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : fallback;
}

const BASE = arg('base', 'http://127.0.0.1:18328').replace(/\/$/, '');
const ADMIN = arg('user', 'admin');
const PASSWORD = arg('password', 'smoke-PW-123456');
const ELECTRON = arg('electron', '');
const APP_DIR = arg('app', '');
const WORK = arg('work', mkdtempSync(join(tmpdir(), 'ba-e2e-')));
const SHOTS = arg('shots', '');
const CDP_PORT = Number(arg('cdp-port', '9444'));

const REPO = `electron-${Date.now().toString().slice(-6)}`;
const WC_DIR = join(WORK, 'wc');
const HOME_DIR = join(WORK, 'home');

// ---------------------------------------------------------------- 断言

let pass = 0;
const failures = [];
function ok(label) {
  pass += 1;
  console.log(`  ✓ ${label}`);
}
function fail(label, extra = '') {
  failures.push(label);
  console.log(`  ✗ ${label}${extra ? ` — ${extra}` : ''}`);
}
function expect(label, cond, extra = '') {
  cond ? ok(label) : fail(label, extra);
}

// ---------------------------------------------------------------- 造数（REST）

let token = '';
async function api(method, path, body, raw = false) {
  const headers = { Authorization: `Bearer ${token}` };
  if (body !== undefined && !raw) headers['Content-Type'] = 'application/json';
  const res = await fetch(`${BASE}${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : raw ? body : JSON.stringify(body),
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`${method} ${path} → ${res.status} ${text.slice(0, 200)}`);
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

async function loginViaApi() {
  const r = await fetch(`${BASE}/api/v1/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username: ADMIN, password: PASSWORD }),
  });
  if (!r.ok) throw new Error(`API 登录失败：${r.status}`);
  token = (await r.json()).token;
}

async function seed() {
  await api('POST', '/api/v1/repos', { name: REPO, description: 'Electron 冒烟' });
  const content = 'REMOTE-README-V1';
  const bytes = new TextEncoder().encode(content);
  const hash = createHash('sha256').update(bytes).digest('hex');
  const commitId = crypto.randomUUID();
  // v0.4.17：先锁后提交（§5.2），造数也必须遵守
  await lockPaths(['docs/readme.txt']);
  const prep = await api('POST', `/api/v1/repos/${REPO}/commit/prepare`, {
    commit_id: commitId,
    base_rev: 0,
    message: '远端初始提交',
    changes: [{ path: 'docs/readme.txt', op: 'add', kind: 'file', blob_hash: hash, size: bytes.length, mode: 0, mtime: 0 }],
  });
  if ((prep.need_blobs ?? []).includes(hash)) await api('PUT', `/api/v1/repos/${REPO}/blobs/${hash}`, bytes, true);
  const res = await api('POST', `/api/v1/repos/${REPO}/commit`, {
    commit_id: commitId,
    commit_token: prep.commit_token,
    message: '远端初始提交',
  });
  // 提完就放锁：让后面"解锁后服务端无残留锁"的断言只反映界面行为
  await unlockPaths(['docs/readme.txt']);
  return res.rev ?? 1;
}

/** 加锁（§5.2：每个变更路径都要本人持有）——只对本仓库、路径按段编码。 */
async function lockPaths(paths) {
  for (const p of paths) {
    await api('POST', `/api/v1/repos/${REPO}/locks`, { path: p, comment: 'smoke 造数' });
  }
}

/** 释放锁（同上）。删除路径也要先锁，这条流程顺带把它覆盖了。 */
async function unlockPaths(paths) {
  for (const p of paths) {
    const seg = p.split('/').map(encodeURIComponent).join('/');
    await api('DELETE', `/api/v1/repos/${REPO}/locks/${seg}`).catch(() => {});
  }
}

const HOLDER = 'e2eholder';

/**
 * 造一个"同事"并让他占住某个文件（REST 直连）。
 *
 * 用来验证界面上"别人持锁"的两个表现：树上显示「已锁·别人」、以及**强制解锁**入口
 * （用户报过"加了锁之后解不开"，强制解锁就是那个出口）。
 */
async function heldByOther(path, comment = '同事在改') {
  await api('POST', '/api/v1/admin/users', { username: HOLDER, password: PASSWORD }).catch(() => {});
  const users = await api('GET', '/api/v1/admin/users');
  const uid = (users.items ?? []).find((u) => u.username === HOLDER)?.id;
  if (!uid) throw new Error(`建不出用户 ${HOLDER}`);
  await api('POST', `/api/v1/admin/repos/${REPO}/acl`, {
    path_prefix: '',
    subject_type: 'user',
    subject_id: uid,
    level: 'write',
    inherit: true,
  });
  const res = await fetch(`${BASE}/api/v1/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username: HOLDER, password: PASSWORD }),
  });
  if (!res.ok) throw new Error(`holder 登录失败：${res.status}`);
  const holderToken = (await res.json()).token;
  const lockRes = await fetch(`${BASE}/api/v1/repos/${REPO}/locks`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${holderToken}` },
    body: JSON.stringify({ path, comment }),
  });
  if (!lockRes.ok) throw new Error(`holder 加锁失败：${lockRes.status} ${await lockRes.text()}`);
}

/** 模拟"别人提交了变更"（新增 / 修改 / 删除都走这里）。 */
async function remoteApply(changes, baseRev, message = '他人提交') {
  const prepared = [];
  for (const c of changes) {
    if (c.op === 'delete') {
      prepared.push({ path: c.path, op: 'delete', kind: 'file' });
      continue;
    }
    const bytes = new TextEncoder().encode(c.content);
    const hash = createHash('sha256').update(bytes).digest('hex');
    await api('PUT', `/api/v1/repos/${REPO}/blobs/${hash}`, bytes, true);
    prepared.push({ path: c.path, op: c.op, kind: 'file', blob_hash: hash, size: bytes.length, mode: 0, mtime: 0 });
  }
  const commitId = crypto.randomUUID();
  // 模拟"另一个人"的完整流程：先锁 → 提交 → 放锁（§5.2）
  const paths = prepared.map((c) => c.path);
  await lockPaths(paths);
  const prep = await api('POST', `/api/v1/repos/${REPO}/commit/prepare`, {
    commit_id: commitId,
    base_rev: baseRev,
    message,
    changes: prepared,
  });
  const res = await api('POST', `/api/v1/repos/${REPO}/commit`, {
    commit_id: commitId,
    commit_token: prep.commit_token,
    message,
  });
  await unlockPaths(paths);
  return res.rev ?? baseRev + 1;
}

async function headRev() {
  return (await api('GET', `/api/v1/repos/${REPO}/info`)).head_rev;
}

/** 兼容旧调用点：新增一个文件。 */
async function remoteCommit(path, content, baseRev) {
  return remoteApply([{ path, op: 'add', content }], baseRev);
}

// ---------------------------------------------------------------- CDP

class Cdp {
  constructor(ws) {
    this.ws = ws;
    this.id = 0;
    this.pending = new Map();
    this.exceptions = [];
    this.consoleErrors = [];
    ws.addEventListener('message', (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
        return;
      }
      if (msg.method === 'Runtime.exceptionThrown') {
        const d = msg.params.exceptionDetails;
        this.exceptions.push(d.exception?.description || d.text || JSON.stringify(d).slice(0, 300));
      }
      if (msg.method === 'Runtime.consoleAPICalled' && msg.params.type === 'error') {
        this.consoleErrors.push((msg.params.args || []).map((a) => a.value ?? a.description ?? '').join(' '));
      }
    });
  }

  send(method, params = {}) {
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  async run(fn, ...args) {
    const expr = `(${fn.toString()})(${args.map((a) => JSON.stringify(a)).join(',')})`;
    const r = await this.send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) {
      const d = r.exceptionDetails;
      throw new Error(d.exception?.description || d.text || 'evaluate 失败');
    }
    return r.result.value;
  }

  async shot(dir, name) {
    if (!dir) return;
    const { data } = await this.send('Page.captureScreenshot', { format: 'png' });
    writeFileSync(join(dir, `${name}.png`), Buffer.from(data, 'base64'));
  }
}

/**
 * 注入渲染层的交互助手。
 *
 * 挂在 `globalThis.__h` 上而不是用顶层 const：不同一次 evaluate 之间的词法绑定
 * 不保证共享，显式挂全局最稳。
 */
const PRELUDE = `
globalThis.__h = (() => {
  const $ = (s, r = document) => r.querySelector(s);
  const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));
  const tid = (id, r = document) => r.querySelector('[data-testid="' + id + '"]');
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const vtext = (el) => (el && el.textContent ? el.textContent : '').replace(/\\s+/g, ' ').trim();
  const body = () => vtext(document.body);
  async function waitFor(fn, ms = 20000) {
    const t0 = Date.now();
    for (;;) {
      let v = null;
      try { v = await fn(); } catch { v = null; }
      if (v) return v;
      if (Date.now() - t0 > ms) return null;
      await sleep(120);
    }
  }
  /**
   * 找 [data-testid] 底下的**真正输入元素**。
   *
   * Element Plus 的不同组件把属性放在不同层级：el-input 自己有 inheritAttrs:false
   * 会把属性透到内部 input/textarea，而 el-input-number 的根元素是外层 div。
   * 统一用这个函数取，就不会出现"拿到了 div 去 setInput"的 Illegal invocation。
   */
  function inputOf(id, r = document) {
    const el = tid(id, r);
    if (!el) return null;
    if (el.matches('input, textarea')) return el;
    return el.querySelector('input, textarea');
  }
  function setInput(el, value) {
    // 传 testid 字符串也行：内部统一解析成真正的输入元素
    if (typeof el === 'string') el = inputOf(el);
    if (!el) throw new Error('setInput 找不到输入元素');
    const proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, value);
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  }
  function click(el) { if (!el) throw new Error('元素不存在，无法点击'); el.click(); return true; }
  function clickTid(id, r = document) {
    const el = tid(id, r);
    if (!el) throw new Error('找不到 [data-testid="' + id + '"]');
    el.click();
    return true;
  }
  /** 切到左侧某个页签（工作副本 / 仓库 / 锁）。 */
  async function goto(tabText) {
    const item = $$('.el-menu-item').find((e) => vtext(e).includes(tabText));
    if (!item) throw new Error('找不到页签：' + tabText);
    item.click();
    await sleep(400);
    return true;
  }
  /**
   * 确保某个目录"处于展开状态"。
   *
   * 不能盲目双击：那是个切换，已展开时反而会收起。所以读展开箭头上的 expanded
   * 类来判断当前状态，只在未展开时点它。（每个步骤都应自带这个前置条件，
   * 否则会依赖上一步的副作用 —— 踩过：提交步骤不再"选中"节点后，
   * 后续新建文件落到别处、目录变干净而被折叠，一串断言跟着红。）
   */
  function ensureExpanded(path) {
    const label = tid('wc-node-' + path);
    const content = label && label.closest('.el-tree-node__content');
    const caret = content && content.querySelector('.el-tree-node__expand-icon');
    if (!caret) return false;
    if (!caret.classList.contains('expanded')) caret.click();
    return true;
  }
  /**
   * 右键某个节点 → 弹出快捷菜单。
   *
   * 加锁 / 解锁 / 强制解锁**只在快捷菜单里**（工具栏那一行不再放一份），
   * 所以所有锁相关的驱动都必须先走这里。
   */
  function openMenu(path) {
    const label = tid('wc-node-' + path);
    if (!label) return false;
    label.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, clientX: 300, clientY: 260 }));
    return true;
  }
  function menuItem(id) { return tid('wc-ctx-' + id); }
  function rows() { return $$('.el-table__row'); }
  function rowTexts() { return rows().map(vtext); }
  function tabByText(t) { return $$('.el-menu-item').find((e) => vtext(e).includes(t)) || null; }
  return { $, $$, tid, inputOf, sleep, vtext, body, waitFor, setInput, click, clickTid, goto, ensureExpanded, openMenu, menuItem, rows, rowTexts, tabByText };
})();
true;
`;

// ---------------------------------------------------------------- 步骤

const steps = [];
const step = (name, fn) => steps.push({ name, fn });

step('启动：窗口加载渲染层，preload 注入成功', async (c) => {
  const r = await c.run(async () => {
    const h = globalThis.__h;
    const visible = await h.waitFor(() => h.tid('login-submit'), 25000);
    return {
      loginVisible: !!visible,
      hasBridge: typeof window.bartifact === 'object' && window.bartifact !== null,
      channelCount: window.bartifact?.channels?.length ?? 0,
      appInfo: h.tid('login-appinfo') ? h.vtext(h.tid('login-appinfo')) : '',
      bodyText: h.body().slice(0, 200),
    };
  });
  expect('渲染层加载（登录界面可见）', r.loginVisible === true, JSON.stringify(r).slice(0, 300));
  expect('preload 注入 window.bartifact', r.hasBridge === true, JSON.stringify(r).slice(0, 200));
  expect('IPC 白名单已暴露（≥20 个通道）', r.channelCount >= 20, `channels=${r.channelCount}`);
  expect('app:info 往返成功（Electron 版本可见）', /Electron \d/.test(r.appInfo), r.appInfo);
});

step('登录：错误密码被拒，正确密码进入主界面', async (c) => {
  const bad = await c.run(async (base) => {
    const h = globalThis.__h;
    h.setInput(h.tid('login-server'), base);
    h.setInput(h.tid('login-username'), 'nobody');
    h.setInput(h.tid('login-password'), 'wrong-password');
    h.click(h.tid('login-submit'));
    const err = await h.waitFor(() => (h.tid('login-error') ? h.vtext(h.tid('login-error')) : null), 15000);
    return { err: err || '', stillLogin: !!h.tid('login-submit') };
  }, BASE);
  expect('错误密码给出错误提示', bad.err.length > 0, JSON.stringify(bad).slice(0, 200));
  expect('错误密码后仍停在登录界面', bad.stillLogin === true);

  const good = await c.run(async (user, pw) => {
    const h = globalThis.__h;
    h.setInput(h.tid('login-username'), user);
    h.setInput(h.tid('login-password'), pw);
    h.click(h.tid('login-submit'));
    const landed = await h.waitFor(() => h.tid('menu'), 25000);
    return {
      landed: !!landed,
      user: h.tid('user') ? h.vtext(h.tid('user')) : '',
      menus: h.$$('.el-menu-item').map(h.vtext),
    };
  }, ADMIN, PASSWORD);
  expect('登录成功进入主界面', good.landed === true, JSON.stringify(good).slice(0, 200));
  expect('侧栏显示当前用户', good.user === ADMIN, good.user);
  expect(
    '主菜单包含工作副本 / 仓库 / 历史 / 冲突 / 设置',
    ['工作副本', '仓库', '历史', '冲突', '设置'].every((t) => good.menus.some((m) => m.includes(t))),
    JSON.stringify(good.menus),
  );
  // v0.4.17：锁不再单独占一个页签 —— 加锁/解锁/强制解锁都长在工作副本的目录树上
  expect('锁不再是独立菜单项', !good.menus.some((m) => m.trim() === '锁'), JSON.stringify(good.menus));
});

step('仓库页：列表加载并可选中', async (c) => {
  const r = await c.run(async (repo) => {
    const h = globalThis.__h;
    h.click(h.tabByText('仓库'));
    const rows = await h.waitFor(() => {
      const t = h.rowTexts();
      return t.length > 0 ? t : null;
    }, 20000);
    if (!rows) return { err: '仓库列表为空' };
    const target = h.rows().find((row) => h.vtext(row).includes(repo));
    if (!target) return { err: '找不到目标仓库', rows };
    target.click();
    const picked = await h.waitFor(() => {
      const el = h.tid('checkout-repo');
      return el && h.vtext(el).includes(repo) ? true : null;
    }, 10000);

    // 选中仓库后目录树该挂起来（此刻仓库还是空的，看不出新旧差异——真正的回归断言在
    // 后面「关掉工作副本后目录树照样加载」那一步）
    await h.waitFor(() => h.tid('tree-nodes') || h.tid('tree-error') || null, 20000);
    return {
      picked: !!picked,
      rows,
      treeMounted: !!h.tid('tree-nodes'),
      treeError: h.tid('tree-error') ? h.vtext(h.tid('tree-error')) : '',
    };
  }, REPO);
  expect('仓库列表出现目标仓库', !r.err && Array.isArray(r.rows), JSON.stringify(r).slice(0, 300));
  expect('选中仓库后表单回显仓库名', r.picked === true, JSON.stringify(r).slice(0, 200));
  expect('目录树挂起来了且没有报错', r.treeMounted === true && !r.treeError, JSON.stringify(r).slice(0, 300));
});

step('设置里配好「检出目录的默认路径」', async (c) => {
  const r = await c.run(async (base, repo) => {
    const h = globalThis.__h;
    h.clickTid('menu-settings');
    const ready = await h.waitFor(() => h.inputOf('settings-checkout-dir'), 20000);
    if (!ready) return { err: '设置页未就绪' };

    h.setInput('settings-checkout-dir', base);
    h.clickTid('settings-save');
    const saved = await h.waitFor(() => {
      const f = h.tid('flash');
      const t = f ? h.vtext(f) : '';
      return t.includes('设置已保存') ? t : null;
    }, 20000);
    const value = h.inputOf('settings-checkout-dir')?.value ?? '';

    // **回仓库页并按用户流程走一遍**：选仓库 → 检出目录已经预填成"默认路径 + 仓库名"。
    // （必须自己回到仓库页并重新选仓库：视图重新挂载后选中态会丢，后面的检出步骤依赖它）
    await h.goto('仓库');
    const rows = await h.waitFor(() => (h.rowTexts().length > 0 ? h.rowTexts() : null), 20000);
    const row = h.rows().find((r) => h.vtext(r).includes(repo));
    if (row) row.click();
    const prefilled = await h.waitFor(() => h.inputOf('checkout-dir')?.value || null, 15000);

    return {
      saved: saved || '',
      value,
      rows: rows || [],
      prefilled: prefilled || '',
    };
  }, join(WORK, 'checkouts'), REPO);
  expect('设置里能保存检出默认路径', (r.saved || '').includes('设置已保存'), JSON.stringify(r).slice(0, 200));
  expect('输入框回显所保存的路径', r.value === join(WORK, 'checkouts'), r.value);
  expect(
    '配置确实落盘',
    readFileSync(join(HOME_DIR, '.b-artifact', 'config.json'), 'utf8').includes('checkouts'),
  );
  expect(
    '**回到仓库页选中仓库后，检出目录已预填 = 默认路径 + 仓库名**',
    r.prefilled === join(WORK, 'checkouts', REPO),
    `${r.prefilled}`,
  );
});

step('检出：真实落盘到目标目录', async (c) => {
  const r = await c.run(async (dir, repo) => {
    const h = globalThis.__h;
    // **回归**：还没手动输入时，检出目录应当已经按"设置里的默认路径 + 仓库名"预填好了
    const prefilled = h.inputOf('checkout-dir')?.value ?? '';
    h.setInput(h.tid('checkout-dir'), dir);
    h.click(h.tid('checkout-submit'));
    const head = await h.waitFor(() => {
      const el = h.tid('head-repo');
      return el && h.vtext(el).includes(repo) ? true : null;
    }, 40000);
    // 检出成功后应当**自动切到工作副本页**（用户下一步一定是在那儿干活）
    const activeTab = await h.waitFor(() => {
      const el = h.$('.el-menu-item.is-active');
      return el ? h.vtext(el) : null;
    }, 15000);

    // 刚打开的工作副本：树上应当是**干净的** —— 没有任何勾选，也没有高亮的"当前节点"
    // （真实反馈："刚打开工作副本时，目录树都不应该选中"）
    const tree = await h.waitFor(() => h.tid('wc-tree-nodes') || null, 15000);
    const boxes = tree ? Array.from(tree.querySelectorAll('input[type="checkbox"]')) : [];
    const checkedCount = boxes.filter((b) => b.checked).length;
    const currentNodes = tree ? tree.querySelectorAll('.el-tree-node.is-current').length : -1;

    return {
      prefilled,
      head: !!head,
      rev: h.tid('head-rev') ? h.vtext(h.tid('head-rev')) : '',
      root: h.tid('head-root') ? h.vtext(h.tid('head-root')) : '',
      flash: h.tid('flash') ? h.vtext(h.tid('flash')) : '',
      activeTab: activeTab || '',
      checkedCount,
      currentNodes,
      currentText: h.tid('wc-current') ? h.vtext(h.tid('wc-current')) : '',
    };
  }, WC_DIR, REPO);
  expect('检出后顶栏显示仓库', r.head === true, JSON.stringify(r).slice(0, 300));
  expect(
    '**默认目录 = 设置里的默认路径 + 仓库名**',
    r.prefilled === join(WORK, 'checkouts', REPO),
    `${r.prefilled}`,
  );
  expect('顶栏显示修订号 r1', r.rev === 'r1', r.rev);
  expect('顶栏显示工作副本根目录', r.root.includes(WC_DIR) || r.root.includes('wc'), r.root);
  expect('**检出后自动跳到「工作副本」页**', (r.activeTab || '').includes('工作副本'), r.activeTab);
  expect('刚打开时树上**没有任何勾选**', r.checkedCount === 0, JSON.stringify(r).slice(0, 300));
  expect('刚打开时**没有高亮的当前节点**', r.currentNodes === 0, JSON.stringify(r).slice(0, 300));
  expect('工具栏提示"请点选"', (r.currentText || '').includes('点选'), r.currentText);
});

step('检出产物校验：wc.db、pristine、工作文件都在', async () => {
  const wcDb = join(WC_DIR, '.b-artifact', 'wc.db');
  let dbOk = false;
  try {
    dbOk = readFileSync(wcDb).subarray(0, 15).toString() === 'SQLite format 3';
  } catch {
    dbOk = false;
  }
  expect('工作副本里有 wc.db（SQLite）', dbOk);

  let content = '';
  try {
    content = readFileSync(join(WC_DIR, 'docs', 'readme.txt'), 'utf8');
  } catch {
    content = '';
  }
  expect('服务端文件已检出到磁盘且内容一致', content === 'REMOTE-README-V1', JSON.stringify(content));
});

step('打开已有副本弹窗：「打开」看得见、不需要拉横向滚动条', async (c) => {
  const r = await c.run(async () => {
    const h = globalThis.__h;
    await h.goto('工作副本');
    h.clickTid('wc-open-existing');
    const dialog = await h.waitFor(() => document.querySelector('.el-dialog') || null, 15000);
    if (!dialog) return { err: '弹窗没出现' };

    const btn = await h.waitFor(() => h.tid('open-recent-open') || null, 10000);
    if (!btn) return { err: '没有「打开」按钮（最近列表为空？）' };

    // 可见性判据：按钮真的渲染出来了，而且**落在弹窗可视区内**（不用横向滚动）
    const d = dialog.getBoundingClientRect();
    const b = btn.getBoundingClientRect();
    const visible = !!btn.offsetParent && b.width > 0 && b.right <= d.right + 1 && b.left >= d.left - 1;
    // 表格是否横向溢出（有溢出说明又有东西被挤出去了）
    const wrap = dialog.querySelector('.el-table__body-wrapper') || dialog.querySelector('.el-table');
    const overflow = wrap ? wrap.scrollWidth > wrap.clientWidth + 1 : false;

    btn.click();
    // 注意：el-dialog 关闭时**元素仍在 DOM** 里（只是隐藏）——判"关了没"必须看可见性
    const closed = await h.waitFor(() => {
      const d = document.querySelector('.el-dialog');
      if (!d) return true;
      const overlay = d.closest('.el-overlay');
      const hidden =
        !d.offsetParent ||
        (overlay && getComputedStyle(overlay).display === 'none') ||
        (overlay && getComputedStyle(overlay).visibility === 'hidden');
      return hidden ? true : null;
    }, 20000);
    return {
      visible,
      overflow,
      closed: !!closed,
      btnRight: Math.round(b.right),
      dialogRight: Math.round(d.right),
    };
  });
  expect('弹窗里「打开」按钮可见（不用横向滚动）', r.visible === true, JSON.stringify(r).slice(0, 300));
  expect('最近列表没有横向溢出', r.overflow === false, JSON.stringify(r).slice(0, 300));
  expect('点「打开」后弹窗关闭（副本已打开）', r.closed === true, JSON.stringify(r).slice(0, 300));
});

step('自动同步：在系统文件夹里新建文件 → 界面自己出现在树上（不点刷新）', async (c) => {
  // 真实用户操作：直接在系统文件夹里建文件。客户端必须**自己**发现，
  // 这正是 v0.4.17 的诉求（§6.5 自动同步）。
  mkdirSync(join(WC_DIR, 'characters'), { recursive: true });
  writeFileSync(join(WC_DIR, 'characters', 'hero.psd'), 'HERO-V1');

  const r = await c.run(async () => {
    const h = globalThis.__h;
    // 检出后停在"仓库"页签，先切到工作副本页
    await h.goto('工作副本');
    const ready = await h.waitFor(() => h.tid('wc-tree'), 15000);
    if (!ready) return { err: '工作副本页未就绪' };
    // 关键：**不点任何刷新按钮**，等文件监听触发重算 + 推送
    const node = await h.waitFor(() => (h.tid('wc-node-characters/hero.psd') ? true : null), 30000);
    // 未纳管的新文件默认**不勾选**（这条断言原先挂在"树上新建文件"步骤里；新建文件功能
    // 已去掉，改为在"用户自己在系统文件夹里造文件"这条更真实的路径上断言）
    const box = h
      .tid('wc-node-characters/hero.psd')
      ?.closest('.el-tree-node__content')
      ?.querySelector('input[type="checkbox"]');
    return {
      node: !!node,
      status: h.tid('wc-status-characters/hero.psd') ? h.vtext(h.tid('wc-status-characters/hero.psd')) : '',
      dirCount: h.tid('wc-dir-count-characters') ? h.vtext(h.tid('wc-dir-count-characters')) : '',
      sync: h.tid('sync-state') ? h.vtext(h.tid('sync-state')) : '',
      hasRefreshBtn: !!h.tid('wc-refresh'),
      checked: !!box?.checked,
      hint: h.tid('commit-hint') ? h.vtext(h.tid('commit-hint')) : '',
      blocked: h.tid('commit-blocked') ? h.vtext(h.tid('commit-blocked')) : '',
      commitLabel: h.tid('commit-submit') ? h.vtext(h.tid('commit-submit')) : '',
    };
  });
  expect('文件出现在目录树上（自动同步，没点刷新）', r.node === true, JSON.stringify(r).slice(0, 300));
  expect('状态标为「新增」（不再需要先"标记新增"）', r.status.includes('新增'), r.status);
  expect('目录节点汇总"变更 1"', r.dirCount.includes('变更 1'), r.dirCount);
  expect('顶栏显示自动同步中', r.sync.includes('自动同步'), r.sync);
  expect('自动同步可用时**不显示**手动刷新按钮', r.hasRefreshBtn === false);
  expect('**默认不勾选**：未纳管的新文件不进提交集', r.checked === false, JSON.stringify(r).slice(0, 300));
  // 只有未纳管文件时，提交区给的是"没有可提交的变更"（提示条不渲染），两种文案都接受
  const explained =
    /（0）/.test(r.commitLabel || '') ||
    (r.hint || '').includes('新增') ||
    (r.blocked || '').includes('没有可提交');
  expect('提交区说明"未纳管文件不进提交集"', explained, `${r.commitLabel} | ${r.hint} | ${r.blocked}`);
});
step('忽略规则：**按工作副本**编辑 → 命中的文件被忽略', async (c) => {
  // 造一个"噪音文件"，它会先以「新增」出现，写了规则之后应当消失
  writeFileSync(join(WC_DIR, 'scratch.tmp'), 'NOISE');

  const r = await c.run(async () => {
    const h = globalThis.__h;
    await h.goto('工作副本');
    const before = await h.waitFor(() => h.tid('wc-node-scratch.tmp') || null, 30000);
    const statusBefore = before && h.tid('wc-status-scratch.tmp') ? h.vtext(h.tid('wc-status-scratch.tmp')) : '';

    // 工作副本页的「忽略规则」入口（不在设置里）
    h.clickTid('wc-ignore');
    const ta = await h.waitFor(() => h.inputOf('wc-ignore-text'), 15000);
    if (!ta) return { err: '忽略规则弹窗没出现', before: !!before, statusBefore };
    const prefilled = ta.value;

    // 弹窗要能**拖开**（否则它挡住后面的目录树，没法一边看树一边改规则）
    let dragged = { dx: 0, dy: 0 };
    {
      const dlg = ta.closest('.el-dialog');
      const header = dlg?.querySelector('.el-dialog__header');
      if (dlg && header) {
        const r0 = dlg.getBoundingClientRect();
        const h0 = header.getBoundingClientRect();
        header.dispatchEvent(
          new MouseEvent('mousedown', { bubbles: true, clientX: h0.left + 30, clientY: h0.top + 12 }),
        );
        document.dispatchEvent(
          new MouseEvent('mousemove', {
            bubbles: true,
            clientX: h0.left + 30 + 140,
            clientY: h0.top + 12 + 90,
          }),
        );
        await h.sleep(80);
        document.dispatchEvent(new MouseEvent('mouseup', { bubbles: true }));
        await h.sleep(80);
        const r1 = dlg.getBoundingClientRect();
        dragged = { dx: Math.round(r1.left - r0.left), dy: Math.round(r1.top - r0.top) };
      }
    }

    h.setInput('wc-ignore-text', '*.tmp');
    h.clickTid('wc-ignore-save');
    const saved = await h.waitFor(() => {
      const f = h.tid('flash');
      const t = f ? h.vtext(f) : '';
      return t.includes('已保存忽略规则') ? t : null;
    }, 20000);
    // 被忽略的文件**仍然在树上**（树是全量的），只是状态标变成「忽略」——
    // 断言"节点消失"是错的，断言"状态变了"才对
    const ignoredTag = await h.waitFor(() => {
      const el = h.tid('wc-status-scratch.tmp');
      const t = el ? h.vtext(el) : '';
      return t.includes('忽略') ? t : null;
    }, 30000);
    const commitLabel = h.tid('commit-submit') ? h.vtext(h.tid('commit-submit')) : '';
    return {
      before: !!before,
      statusBefore,
      prefilled,
      saved: saved || '',
      ignoredTag: ignoredTag || '',
      dragged,
      stillOnTree: !!h.tid('wc-node-scratch.tmp'),
      commitLabel,
      hasIgnoreBtn: !!h.tid('wc-ignore'),
    };
  });

  expect('规则还没写时：噪音文件以「新增」出现在树上', r.before === true && (r.statusBefore || '').includes('新增'), JSON.stringify(r).slice(0, 300));
  expect('工作副本页有「忽略规则」入口', r.hasIgnoreBtn === true, JSON.stringify(r).slice(0, 200));
  expect('保存后给出提示', (r.saved || '').includes('已保存忽略规则'), r.saved);
  expect(
    '**弹窗可以拖开**（不挡住后面的目录树）',
    Math.abs(r.dragged?.dx ?? 0) > 50 && Math.abs(r.dragged?.dy ?? 0) > 30,
    JSON.stringify(r.dragged),
  );
  expect('**命中的文件状态变成「忽略」**', (r.ignoredTag || '').includes('忽略'), JSON.stringify(r).slice(0, 300));
  expect('它仍然在树上（树是全量的，只是不再算变更）', r.stillOnTree === true, JSON.stringify(r).slice(0, 200));
  expect('被忽略的文件不进提交集', /提交（0）/.test(r.commitLabel || ''), r.commitLabel);
  const written = readFileSync(join(WC_DIR, '.b-artifactignore'), 'utf8');
  expect('规则落在**工作副本根目录**的 .b-artifactignore', written.includes('*.tmp'), written);
});

step('勾选在自动同步刷新后保持不变（回归：刷新把勾选冲掉）', async (c) => {
  const r = await c.run(async () => {
    const h = globalThis.__h;
    const boxOf = (path) => {
      const label = h.tid('wc-node-' + path);
      const content = label && label.closest('.el-tree-node__content');
      return content ? content.querySelector('input[type="checkbox"]') : null;
    };
    await h.goto('工作副本');
    const label = await h.waitFor(() => h.tid('wc-node-characters/hero.psd') || null, 20000);
    if (!label) return { err: '树上没有目标节点' };

    // 勾上"新增"的那个文件（点复选框，不是点标签 —— 点标签只是选中）
    const box = boxOf('characters/hero.psd');
    if (!box) return { err: '找不到该节点的复选框' };
    box.click();
    await h.sleep(400);
    const labelAfterCheck = h.vtext(h.tid('commit-submit'));
    const checkedAfterCheck = !!boxOf('characters/hero.psd')?.checked;

    // 触发一次自动同步：在系统文件夹里改一个**已纳管**文件 → 树数据整体重建
    return {
      labelAfterCheck,
      checkedAfterCheck,
      beforeRefresh: true,
    };
  }, );

  expect('勾选后提交集变成 1 项', /提交（1）/.test(r.labelAfterCheck || ''), `${r.labelAfterCheck} ${JSON.stringify(r).slice(0, 200)}`);
  expect('复选框处于勾选状态', r.checkedAfterCheck === true, JSON.stringify(r).slice(0, 200));

  // 在系统文件夹里改一个已纳管文件（Node 侧写盘，模拟用户在 Finder 里编辑）
  writeFileSync(join(WC_DIR, 'docs', 'readme.txt'), 'REMOTE-README-V1-EDITED');

  const after = await c.run(async () => {
    const h = globalThis.__h;
    const boxOf = (path) => {
      const label = h.tid('wc-node-' + path);
      const content = label && label.closest('.el-tree-node__content');
      return content ? content.querySelector('input[type="checkbox"]') : null;
    };
    // 等刷新真的发生：**看 docs 目录节点上的"变更"徽标**，而不是它子节点的状态标。
    // 目录行在根层永远渲染（折叠也看得见），而子节点要等目录被自动展开 —— 后者依赖
    // "新变更目录自动展开"的时序，用它当前置条件会把这条断言变成时序赌博（踩过）。
    const refreshed = await h.waitFor(() => {
      const el = h.tid('wc-dir-count-docs');
      return el && h.vtext(el).includes('变更') ? true : null;
    }, 30000);
    return {
      refreshed: !!refreshed,
      checked: !!boxOf('characters/hero.psd')?.checked,
      label: h.vtext(h.tid('commit-submit')),
      // 诊断：监听降级会让"改盘不动界面"，先看同步状态与树上是否真有 docs 徽标
      sync: h.tid('sync-state') ? h.vtext(h.tid('sync-state')) : '',
      docsBadge: h.tid('wc-dir-count-docs') ? h.vtext(h.tid('wc-dir-count-docs')) : '(无)',
      docsNode: !!h.tid('wc-node-docs'),
    };
  });

  expect('刷新确实发生了（读到了新改动）', after.refreshed === true, JSON.stringify(after).slice(0, 200));
  expect('**刷新后原来的勾选没有丢**', after.checked === true, JSON.stringify(after).slice(0, 200));
  expect('提交集仍是 1 项（用户手改过之后不会被新变更冲掉）', /提交（1）/.test(after.label || ''), after.label);

  // 收尾：把 readme.txt 写回原样，别把这个"脏文件"留给后面的步骤（否则它们的
  // "提交集大小"预期会被带偏 —— 这一步踩过一次）
  writeFileSync(join(WC_DIR, 'docs', 'readme.txt'), 'REMOTE-README-V1');
});

step('提交：树上勾选 → 说明留空 → 提交成功（自动补锁）', async (c) => {
  const r = await c.run(async () => {
    const h = globalThis.__h;
    const node = h.tid('wc-node-characters/hero.psd');
    if (!node) return { err: '树上找不到目标节点' };
    // **勾选要点复选框本身**：点节点标签只是"选中"（v0.4.17 起勾选与选中分离），
    // 两者混用正是"选了目录却锁了别的文件"那类问题的根源。
    const box = node.closest('.el-tree-node__content')?.querySelector('input[type="checkbox"]');
    if (!box) return { err: '找不到该节点的复选框' };
    const checkedBeforeClick = box.checked;
    if (!box.checked) box.click(); // 上一步已经勾过的话就别反选掉
    await h.sleep(400);
    const diag = {
      checkedBeforeClick,
      status: h.tid('wc-status-characters/hero.psd') ? h.vtext(h.tid('wc-status-characters/hero.psd')) : '',
      hint: h.tid('commit-hint') ? h.vtext(h.tid('commit-hint')) : '',
      blockedNow: h.tid('commit-blocked') ? h.vtext(h.tid('commit-blocked')) : '',
      checkedNodes: h
        .$$('[data-testid^="wc-node-"]')
        .filter((el) => el.closest('.el-tree-node__content')?.querySelector('input[type="checkbox"]')?.checked)
        .map((el) => el.getAttribute('data-testid')),
    };

    // 顺便验证"提交后自动解锁"：先**手动**锁上这个文件（右键 → 加锁；点标签只是选中）
    h.openMenu('characters/hero.psd');
    const lockItem = await h.waitFor(() => h.menuItem('lock') || null, 8000);
    if (lockItem) lockItem.click();
    const lockedBefore = await h.waitFor(
      () => (h.tid('wc-lock-characters/hero.psd') ? true : null),
      20000,
    );

    const submit = h.tid('commit-submit');
    if (!submit) return { err: '找不到提交按钮' };
    if (submit.disabled) {
      return {
        err: '提交按钮禁用',
        blocked: h.tid('commit-blocked') ? h.vtext(h.tid('commit-blocked')) : '',
        ...diag,
      };
    }
    // **说明可空**：先确认空说明下按钮**不再**被拦（v0.4.17 去掉了客户端必填校验），
    // 再填上说明提交——历史断言与修订号沿用这条提交，别把流程搅乱。
    const emptyOk = !submit.disabled;
    h.setInput('commit-message', 'Electron 冒烟提交');
    await h.sleep(200);
    const label = h.vtext(h.tid('commit-submit'));
    h.tid('commit-submit').click();
    const done = await h.waitFor(() => {
      const f = h.tid('flash');
      return f && h.vtext(f).includes('提交成功') ? h.vtext(f) : null;
    }, 40000);
    // 提交成功后，刚才手动加的锁应当被自动解掉
    const unlockedAfter = await h.waitFor(
      () => (h.tid('wc-lock-characters/hero.psd') ? null : true),
      30000,
    );
    return {
      emptyOk,
      label,
      lockedBefore: !!lockedBefore,
      unlockedAfter: !!unlockedAfter,
      flash: done || '',
      rev: h.tid('head-rev') ? h.vtext(h.tid('head-rev')) : '',
    };
  });
  expect('空说明时提交按钮不被拦（客户端不再要求必填）', r.emptyOk === true, JSON.stringify(r).slice(0, 200));
  expect('勾选后提交按钮可用且显示提交集大小', /提交（1）/.test(r.label), `${r.label} ${JSON.stringify(r).slice(0, 200)}`);
  expect('提交成功并提示修订号', r.flash.includes('提交成功'), r.flash);
  expect('提交前该文件确实被手动锁着', r.lockedBefore === true, JSON.stringify(r).slice(0, 200));
  expect('**提交后手动加的锁被自动解开**', r.unlockedAfter === true, JSON.stringify(r).slice(0, 200));
  expect('顶栏修订号前进到 r2', r.rev === 'r2', r.rev);
});

step('提交后服务端无残留锁（自动补的锁与手动锁都已释放）', async () => {
  const locks = await api('GET', `/api/v1/repos/${REPO}/locks`);
  expect(
    '提交完成后服务端无残留锁',
    (locks.items ?? []).length === 0,
    JSON.stringify(locks.items),
  );
});
let checked = '';
step('树上删除：按**勾选项**执行，而不是当前高亮那一行', async (c) => {
  // 造一个待删除的新文件：直接在系统文件夹里写（客户端**不提供**"新建文件"）
  writeFileSync(join(WC_DIR, 'characters', 'to-delete.txt'), 'DELETE-ME');

  const r = await c.run(async () => {
    const h = globalThis.__h;
    await h.goto('工作副本');
    await h.waitFor(() => h.tid('wc-delete'), 15000);
    if (!(await h.waitFor(() => h.tid('wc-node-characters/to-delete.txt') || null, 30000))) {
      return { err: '新写的文件没有出现在树上' };
    }

    // 勾选它（点复选框），再把"当前节点"切到**另一个**文件上 —— 故意让两者不一致
    const node = h.tid('wc-node-characters/to-delete.txt');
    node.closest('.el-tree-node__content')?.querySelector('input[type="checkbox"]')?.click();
    await h.sleep(300);
    h.tid('wc-node-characters/hero.psd').click();
    await h.sleep(300);
    const current = h.vtext(h.tid('wc-current'));
    const delLabel = h.vtext(h.tid('wc-delete'));

    h.tid('wc-delete').click();
    // 等**文案**出来再读（弹框先挂 DOM、内容后渲染，早读只能读到按钮上的"确定"）
    const confirmText = await h.waitFor(() => {
      const m = [...document.querySelectorAll('.el-message-box')].pop();
      const t = m ? h.vtext(m) : '';
      return t.includes('勾选') ? t : null;
    }, 15000);
    const okBtn = await h.waitFor(() => {
      const m = [...document.querySelectorAll('.el-message-box')].pop();
      return (m && m.querySelector('.el-button--primary')) || null;
    }, 15000);
    if (okBtn) okBtn.click();

    const gone = await h.waitFor(
      () => (h.tid('wc-node-characters/to-delete.txt') ? null : true),
      30000,
    );
    // 删除后勾选要清空（与「还原」一致：整批动作做完，勾选集就该交还）
    await h.sleep(300);
    const checkedLeft = h
      .$$('[data-testid^="wc-node-"]')
      .filter((el) => el.closest('.el-tree-node__content')?.querySelector('input[type="checkbox"]')?.checked)
      .length;
    return {
      current,
      delLabel,
      confirmText: confirmText || '',
      gone: !!gone,
      checkedLeft,
      delDisabled: h.tid('wc-delete') ? h.tid('wc-delete').disabled : false,
      heroStillThere: !!h.tid('wc-node-characters/hero.psd'),
    };
  });

  expect('删除按钮显示勾选数量', /删除（1）/.test(r.delLabel || ''), r.delLabel);
  expect('当前高亮是另一个文件（故意与勾选不一致）', (r.current || '').includes('hero.psd'), r.current);
  expect('确认框说明"勾选的 N 项"', (r.confirmText || '').includes('勾选的 1 项'), r.confirmText);
  expect('被勾选的文件从树上消失', r.gone === true, JSON.stringify(r).slice(0, 300));
  expect('**高亮那个文件没有被删**（回归：删除跟勾选，而跟高亮）', r.heroStillThere === true, JSON.stringify(r).slice(0, 300));
  expect('**删除后勾选被清空**（与「还原」一致）', r.checkedLeft === 0, JSON.stringify(r).slice(0, 300));
  expect('没有勾选时删除按钮禁用', r.delDisabled === true, JSON.stringify(r).slice(0, 200));
  expect('磁盘上确实删掉了', !existsSync(join(WC_DIR, 'characters', 'to-delete.txt')));
  expect('高亮那个文件仍在磁盘上', existsSync(join(WC_DIR, 'characters', 'hero.psd')));
});

step('更新：拉取他人提交的新文件', async (c) => {
  // base_rev 取服务端当前 HEAD，别写死——否则自己还会撞 OUT_OF_DATE
  const info = await api('GET', `/api/v1/repos/${REPO}/info`);
  const rev = await remoteCommit('props/table.png', 'REMOTE-TABLE', info.head_rev);

  const r = await c.run(async (expectedRev) => {
    const h = globalThis.__h;
    await h.goto('工作副本');
    await h.waitFor(() => h.tid('wc-update'), 15000);
    h.clickTid('wc-update');
    const done = await h.waitFor(() => {
      const f = h.tid('flash');
      const t = f ? h.vtext(f) : '';
      return t.includes('更新到') ? t : null;
    }, 40000);
    await h.sleep(500);
    return {
      flash: done || '',
      rev: h.tid('head-rev') ? h.vtext(h.tid('head-rev')) : '',
      expectedRev,
      summary: h.tid('status-summary') ? h.vtext(h.tid('status-summary')) : '',
    };
  }, rev);

  expect('更新提示带目标修订号', r.flash.includes(`更新到 r${r.expectedRev}`), JSON.stringify(r).slice(0, 200));
  expect('顶栏修订号同步', r.rev === `r${r.expectedRev}`, r.rev);

  let pulled = '';
  try {
    pulled = readFileSync(join(WC_DIR, 'props', 'table.png'), 'utf8');
  } catch {
    pulled = '';
  }
  expect('他人提交的文件被拉到本地磁盘', pulled === 'REMOTE-TABLE', JSON.stringify(pulled));
});

step('冲突：本地与远端同时改动 → 更新报冲突并登记', async (c) => {
  // 本地直接改磁盘（真实用户操作）
  writeFileSync(join(WC_DIR, 'props', 'table.png'), 'LOCAL-TABLE');
  writeFileSync(join(WC_DIR, 'docs', 'readme.txt'), 'LOCAL-README');

  // 远端同时改一个、删一个
  let head = await headRev();
  head = await remoteApply([{ path: 'props/table.png', op: 'modify', content: 'REMOTE-TABLE-V2' }], head);
  head = await remoteApply([{ path: 'docs/readme.txt', op: 'delete' }], head);

  const r = await c.run(async () => {
    const h = globalThis.__h;
    await h.goto('工作副本');
    await h.waitFor(() => h.tid('wc-update'), 15000);
    h.clickTid('wc-update');
    const flash = await h.waitFor(() => {
      const f = h.tid('flash');
      const t = f ? h.vtext(f) : '';
      return t.includes('冲突') ? t : null;
    }, 40000);

    h.clickTid('menu-conflicts');
    const ready = await h.waitFor(() => h.tid('conflict-table'), 25000);
    return {
      flash: flash || '',
      ready: !!ready,
      rows: h.rowTexts(),
      head: h.tid('head-conflicts') ? h.vtext(h.tid('head-conflicts')) : '',
    };
  });

  expect('更新后提示存在冲突', r.flash.includes('冲突'), r.flash);
  expect('冲突页打开并列出两条', r.ready === true && r.rows.length >= 2, JSON.stringify(r.rows).slice(0, 300));
  expect('顶栏提示 2 个冲突待解决', r.head.includes('2'), r.head);
});

step('冲突解决：文本合并 + 保留被远端删除的本地文件', async (c) => {
  const first = await c.run(async () => {
    const h = globalThis.__h;
    const row = h.rows().find((x) => h.vtext(x).includes('props/table.png'));
    if (!row) return { err: '找不到 props/table.png 冲突行', rows: h.rowTexts() };
    row.click();
    const loaded = await h.waitFor(() => h.tid('conflict-merge-input'), 20000);
    return {
      mergeable: !!loaded,
      base: h.tid('conflict-base') ? h.vtext(h.tid('conflict-base')) : '',
      mine: h.tid('conflict-mine') ? h.vtext(h.tid('conflict-mine')) : '',
      theirs: h.tid('conflict-theirs') ? h.vtext(h.tid('conflict-theirs')) : '',
    };
  });
  expect('三方对比：基线是共同祖先', first.base.includes('REMOTE-TABLE'), JSON.stringify(first).slice(0, 300));
  expect('三方对比：我的版本是本地改动', first.mine.includes('LOCAL-TABLE'), JSON.stringify(first).slice(0, 200));
  expect('三方对比：服务端版本是远端内容', first.theirs.includes('REMOTE-TABLE-V2'), JSON.stringify(first).slice(0, 200));
  expect('文本冲突可以用合并方式解决', first.mergeable === true, JSON.stringify(first).slice(0, 200));

  // 三方对比是本功能的门面，单独留一张图给人眼过一遍（断言全绿但版式崩掉是发生过的）
  await c.shot(SHOTS, '10a-冲突三方对比');

  const merged = await c.run(async () => {
    const h = globalThis.__h;
    h.setInput(h.tid('conflict-merge-input'), 'MERGED-TABLE');
    h.clickTid('conflict-save-merged');
    const gone = await h.waitFor(
      () => (h.rowTexts().some((t) => t.includes('props/table.png')) ? null : true),
      30000,
    );
    return { gone: !!gone, rows: h.rowTexts() };
  });
  expect('保存合并结果后该冲突消失', merged.gone === true, JSON.stringify(merged.rows).slice(0, 200));

  const second = await c.run(async () => {
    const h = globalThis.__h;
    const row = h.rows().find((x) => h.vtext(x).includes('docs/readme.txt'));
    if (!row) return { err: '找不到 readme 冲突行', rows: h.rowTexts() };
    row.click();
    await h.sleep(500);
    const hint = h.tid('conflict-hint') ? h.vtext(h.tid('conflict-hint')) : '';
    const theirs = h.tid('conflict-theirs') ? h.vtext(h.tid('conflict-theirs')) : '';
    const deleteLabel = h.tid('conflict-take-theirs') ? h.vtext(h.tid('conflict-take-theirs')) : '';
    h.clickTid('conflict-take-mine');
    const cleared = await h.waitFor(() => (h.tid('conflicts-empty') ? true : null), 30000);
    return { hint, theirs, deleteLabel, cleared: !!cleared };
  });
  expect('服务端删除型冲突给出对应语义', second.hint.includes('服务端已删除'), second.hint);
  expect('服务端侧显示"不存在"', second.theirs.includes('不存在'), second.theirs);
  expect('按钮文案是"接受服务端删除"', second.deleteLabel.includes('接受服务端删除'), second.deleteLabel);
  expect('保留本地后冲突列表清空', second.cleared === true, JSON.stringify(second).slice(0, 200));
});

step('解决后提交：合并结果与被保留的文件都入库', async (c) => {
  const r = await c.run(async () => {
    const h = globalThis.__h;
    await h.goto('工作副本');
    // 不再点"刷新状态"（按钮已移除）：等树渲染好即可，状态由自动同步维护
    await h.waitFor(() => h.tid('wc-tree'), 15000);
    await h.sleep(600);
    h.setInput(h.tid('commit-message'), '解决冲突后提交');
    const submit = h.tid('commit-submit');
    if (!submit) return { err: '找不到提交按钮' };
    if (submit.disabled) {
      return { err: '提交按钮仍禁用', blocked: h.tid('commit-blocked') ? h.vtext(h.tid('commit-blocked')) : '' };
    }
    submit.click();
    const flash = await h.waitFor(() => {
      const f = h.tid('flash');
      const t = f ? h.vtext(f) : '';
      return t.includes('提交成功') ? t : null;
    }, 40000);
    return { flash: flash || '' };
  });
  expect('解决冲突后可以正常提交', r.flash.includes('提交成功'), JSON.stringify(r).slice(0, 250));

  const info = await api('GET', `/api/v1/repos/${REPO}/info`);
  expect('服务端再次前进', info.head_rev >= 4, `head=${info.head_rev}`);

  const docs = await api('GET', `/api/v1/repos/${REPO}/tree?prefix=docs`);
  expect(
    '被远端删除但选择保留的文件回到仓库',
    (docs.items ?? []).some((i) => i.path === 'docs/readme.txt'),
    JSON.stringify(docs.items),
  );
});

step('历史：修订列表 → 旧版文件明细 → 下载此版本', async (c) => {
  const dlDir = join(WORK, 'downloads');

  const listed = await c.run(async () => {
    const h = globalThis.__h;
    h.clickTid('menu-history');
    const ready = await h.waitFor(() => h.tid('history-table'), 25000);
    if (!ready) return { err: '历史页未就绪' };
    const counted = await h.waitFor(() => {
      const el = h.tid('history-count');
      const t = el ? h.vtext(el) : '';
      return t && t !== '0 条' ? t : null;
    }, 20000);
    const body = h.body();
    // 表头必须写全称：以前第一列的表头就一个字母 `r`，没人看得懂（真实反馈）
    const head = h.tid('history-table')?.querySelector('.el-table__header');
    const headerText = head ? h.vtext(head) : '';
    return {
      count: counted || '',
      headerText,
      hasClientCommit: body.includes('Electron 冒烟提交'),
      hasConflictCommit: body.includes('解决冲突后提交'),
      detailRev: h.tid('detail-rev') ? h.vtext(h.tid('detail-rev')) : '',
    };
  });
  expect('历史页加载修订列表', /^\d+ 条$/.test(listed.count), JSON.stringify(listed).slice(0, 250));
  expect('历史里能看到客户端提交的说明', listed.hasClientCommit === true);
  expect('历史里能看到解决冲突后的提交', listed.hasConflictCommit === true);
  expect('自动选中最新修订并展开明细', listed.detailRev.startsWith('r'), listed.detailRev);
  expect(
    '修订列表的**表头写全称「修订」**（不再只摆一个 r）',
    (listed.headerText || '').includes('修订'),
    listed.headerText,
  );

  const picked = await c.run(async (dir) => {
    const h = globalThis.__h;
    h.setInput(h.tid('download-dir'), dir);
    await h.sleep(200);

    // 选中 r3（那时 props/table.png 还是 REMOTE-TABLE，而现在 HEAD 上是 MERGED-TABLE）
    const table = h.tid('history-table');
    const cell = table && table.querySelector('[data-testid="rev-3"]');
    const row = cell && cell.closest('.el-table__row');
    if (!row) return { err: '历史列表里找不到 r3' };
    row.click();

    const rev = await h.waitFor(() => {
      const el = h.tid('detail-rev');
      return el && h.vtext(el) === 'r3' ? true : null;
    }, 20000);
    if (!rev) return { err: '选中 r3 后明细没跟上' };

    // 明细是根目录，先进 props 目录
    const detail = h.tid('detail-table');
    const dirRow = detail && Array.from(detail.querySelectorAll('.el-table__row')).find((r) => h.vtext(r).includes('props'));
    const link = dirRow && dirRow.querySelector('.el-link, a');
    if (!link) return { err: '明细表里找不到 props 目录' };
    link.click();

    const btn = await h.waitFor(() => h.tid('download-props/table.png'), 20000);
    if (!btn) return { err: '进入 props 后没有下载按钮', rows: h.rowTexts() };
    btn.click();
    const flash = await h.waitFor(() => {
      const f = h.tid('flash');
      const t = f ? h.vtext(f) : '';
      return t.includes('已保存') ? t : null;
    }, 30000);
    return { flash: flash || '' };
  }, dlDir);

  expect('选中历史修订并进入其目录', !picked.err, JSON.stringify(picked).slice(0, 250));
  expect('点"下载此版本"提示保存路径', (picked.flash || '').includes('已保存'), picked.flash);

  // 真正的验收：磁盘上拿到的确实是**旧版本内容**，而工作副本里是新内容
  let saved = '';
  try {
    saved = readFileSync(join(dlDir, 'table.png'), 'utf8');
  } catch {
    saved = '';
  }
  expect('下载到的是 r3 当时的旧内容', saved === 'REMOTE-TABLE', JSON.stringify(saved));
  const current = readFileSync(join(WC_DIR, 'props', 'table.png'), 'utf8');
  expect('工作副本里仍是合并后的新内容（没有被覆盖）', current === 'MERGED-TABLE', JSON.stringify(current));
});

step('设置：改并发与忽略规则 → 缓存统计与清理', async (c) => {
  const r = await c.run(async () => {
    const h = globalThis.__h;
    h.clickTid('menu-settings');
    const ready = await h.waitFor(() => h.inputOf('settings-concurrency'), 20000);
    if (!ready) return { err: '设置页未就绪' };

    const before = {
      blobs: h.vtext(h.tid('cache-blobs')),
      bytes: h.vtext(h.tid('cache-bytes')),
      tmp: h.vtext(h.tid('cache-tmp')),
    };

    // 改并发数并保存（**忽略规则不在设置里**：它是按副本的，见工作副本页的「忽略规则」）
    const ignoreGoneFromSettings = !h.tid('settings-ignore') && !h.body().includes('追加忽略规则');
    h.setInput('settings-concurrency', '6');
    h.clickTid('settings-save');
    const saved = await h.waitFor(() => {
      const f = h.tid('flash');
      const t = f ? h.vtext(f) : '';
      return t.includes('设置已保存') ? t : null;
    }, 20000);

    // 设置页**不该**再展示「最近打开的工作副本」（真实反馈：它属于"打开副本"流程，
    // 仓库页与「打开已有副本」弹窗各有一份，设置页是重复）
    const recentCardGone = !h.tid('settings-recent') && !h.body().includes('最近打开的工作副本');

    // 刷新缓存统计，再点"清理残留临时文件"（没有残留时按钮是禁用的，用整清）
    h.clickTid('cache-refresh');
    await h.sleep(600);
    h.clickTid('cache-clear-all');
    const confirmed = await h.waitFor(
      () => (document.querySelector('.el-message-box') ? document.querySelector('.el-message-box') : null),
      8000,
    );
    let after = null;
    if (confirmed) {
      const btn = Array.from(confirmed.querySelectorAll('button')).find((b) => h.vtext(b).includes('清空'));
      if (btn) btn.click();
      const done = await h.waitFor(() => {
        const el = h.tid('cache-last-clear');
        const t = el ? h.vtext(el) : '';
        return t.includes('删除') ? t : null;
      }, 30000);
      after = { lastClear: done || '', blobs: h.vtext(h.tid('cache-blobs')), bytes: h.vtext(h.tid('cache-bytes')) };
    }
    return { saved: saved || '', before, after, recentCardGone, ignoreGoneFromSettings };
  });

  expect('设置页就绪并显示缓存占用', r.before?.blobs !== undefined, JSON.stringify(r).slice(0, 250));
  expect('保存设置成功', (r.saved || '').includes('设置已保存'), r.saved);
  expect(
    '**设置页不再展示「最近打开的工作副本」**',
    r.recentCardGone === true,
    JSON.stringify(r).slice(0, 200),
  );
  expect('清空缓存需要二次确认且确认后执行', (r.after?.lastClear || '').includes('删除'), JSON.stringify(r.after));
  expect('清空后缓存占用归零', r.after?.blobs === '0' && r.after?.bytes === '0 B', JSON.stringify(r.after));

  // 主进程侧的配置确实落盘了
  const cfgRaw = readFileSync(join(HOME_DIR, '.b-artifact', 'config.json'), 'utf8');
  const cfg = JSON.parse(cfgRaw);
  expect('配置落盘：并发数已更新', cfg.concurrency === 6, `concurrency=${cfg.concurrency}`);
  expect(
    '**设置页不再有忽略规则**（它按工作副本，不该放公共配置里）',
    r.ignoreGoneFromSettings === true,
    JSON.stringify(r).slice(0, 200),
  );
});

step('锁 A：树上加锁 → 树内标记 → 树上解锁（文件与目录两条路径）', async (c) => {
  // characters 下要有**两个**文件，才能验证"目录批量加锁"的计数与聚合标记
  // （另一个文件直接写进系统文件夹，客户端不提供"新建文件"）
  writeFileSync(join(WC_DIR, 'characters', 'palette.png'), 'PALETTE-V1');

  const r = await c.run(async () => {
    const h = globalThis.__h;
    await h.goto('工作副本');
    await h.waitFor(() => h.tid('wc-node-characters') || null, 20000);
    h.ensureExpanded('characters'); // 自带前置条件：不依赖上一步把谁展开过
    // 等第二个文件被自动同步推上树（否则下面的"批量锁 2 个"会数不到）
    await h.waitFor(() => h.tid('wc-node-characters/palette.png') || null, 30000);
    const node = await h.waitFor(() => h.tid('wc-node-characters/hero.psd'), 20000);
    if (!node) {
      // 诊断：把树上**实际渲染出来**的节点都列出来，区分"目录折叠了"与"节点根本没生成"
      return {
        err: '树上没有目标节点',
        rendered: h.$$('[data-testid^="wc-node-"]').map((el) => el.getAttribute('data-testid')).slice(0, 40),
        hasCharactersDir: !!h.tid('wc-node-characters'),
        dirBadge: h.tid('wc-dir-count-characters') ? h.vtext(h.tid('wc-dir-count-characters')) : '',
      };
    }

    // ── 1) 单个文件：**点节点内容区**（不是点复选框）→ 当前节点必须跟着变
    //    （用户报过"用复选框选了目录、点加锁却锁了某个文件"，根因就在这）
    node.click();
    await h.sleep(300);
    const current = h.vtext(h.tid('wc-current'));
    // 菜单必须"点别处就关"：这里点的是**树里的另一个位置**（el-tree 的点击带 .stop，
    // 冒泡阶段收不到，所以监听挂在捕获阶段 —— 用户报过"点树上别处菜单不关"）
    h.openMenu('characters/hero.psd');
    const probeMenu = await h.waitFor(() => h.menuItem('lock') || null, 8000);
    if (!probeMenu) return { err: '右键没有弹出菜单' };
    h.tid('wc-node-characters').click(); // 左键点树上别处
    await h.sleep(300);
    const menuClosedByClick = !h.tid('wc-ctx-menu');

    // 右键 → 加锁。**不弹备注框**：点了就锁（真实反馈：加锁是高频动作，不该每次都面对输入框）
    h.openMenu('characters/hero.psd');
    const fileMenu = await h.waitFor(() => h.menuItem('lock') || null, 8000);
    if (!fileMenu) return { err: '右键没有弹出菜单' };
    fileMenu.click();
    await h.sleep(300);
    const fileAskedComment = !!document.querySelector('.el-message-box');
    const fileTagged = await h.waitFor(() => {
      const el = h.tid('wc-lock-characters/hero.psd');
      return el ? h.vtext(el) : null;
    }, 30000);
    // 右键 → 解锁（单个）
    h.openMenu('characters/hero.psd');
    const unlockItem = await h.waitFor(() => h.menuItem('unlock') || null, 8000);
    const fileUnlockEnabled = !!unlockItem && !unlockItem.disabled;
    if (unlockItem) unlockItem.click();
    // 诊断：把接下来几秒内出现过的 flash 文案**按顺序**记下来。
    // 提示条会被后续操作覆盖，只看最后一次会漏掉"刷新失败"这种一闪而过的错误。
    const flashes = [];
    for (let i = 0; i < 8; i++) {
      const f = h.tid('flash');
      const t = f ? h.vtext(f) : '';
      if (t && flashes[flashes.length - 1] !== t) flashes.push(t);
      await h.sleep(150);
    }
    const fileCleared = await h.waitFor(
      () => (h.tid('wc-lock-characters/hero.psd') ? null : true),
      30000,
    );

    // ── 2) 目录：按整棵子树批量加锁 / 解锁
    h.tid('wc-node-characters').click();
    await h.sleep(300);
    const dirCurrent = h.vtext(h.tid('wc-current'));
    const dirHint = h.tid('wc-current-hint') ? h.vtext(h.tid('wc-current-hint')) : '';
    // 右键目录 → 菜单里的加锁项带着"还能加锁的文件数"
    h.openMenu('characters');
    const dirLockItem = await h.waitFor(() => h.menuItem('lock') || null, 8000);
    if (!dirLockItem) return { err: '右键目录没有弹出菜单' };
    const lockLabel = h.vtext(dirLockItem);
    dirLockItem.click();
    // 目录批量加锁同样不弹框
    await h.sleep(300);
    const dirAskedComment = !!document.querySelector('.el-message-box');
    // **等批量真正落地**再读标记：否则会读到"上一次的状态"（这里踩过一次竞态）
    const lockFlash = await h.waitFor(() => {
      const f = h.tid('flash');
      const t = f ? h.vtext(f) : '';
      return t.includes('已锁定') ? t : null;
    }, 30000);
    const dirTag = await h.waitFor(() => {
      const el = h.tid('wc-dirlock-mine-characters');
      const t = el ? h.vtext(el) : '';
      return /已锁\s*2/.test(t) ? t : null;
    }, 30000);
    // 右键目录 → 解锁（按子树）
    h.openMenu('characters');
    const dirUnlockItem = await h.waitFor(() => h.menuItem('unlock') || null, 8000);
    const unlockLabel = dirUnlockItem ? h.vtext(dirUnlockItem) : '';
    if (dirUnlockItem) dirUnlockItem.click();
    const dirCleared = await h.waitFor(
      () => (h.tid('wc-dirlock-mine-characters') ? null : true),
      30000,
    );

    return {
      current,
      flashes,
      menuClosedByClick,
      fileAskedComment,
      fileTagged: fileTagged || '',
      fileUnlockEnabled,
      fileCleared: !!fileCleared,
      dirAskedComment,
      dirCurrent,
      dirHint,
      lockLabel,
      lockFlash: lockFlash || '',
      dirTag: dirTag || '',
      unlockLabel,
      dirCleared: !!dirCleared,
    };
  });

  expect('树上能找到目标节点', !r.err, JSON.stringify(r).slice(0, 500));
  expect('点选节点后"当前"显示该文件（动作目标可见）', (r.current || '').includes('characters/hero.psd'), r.current || '');
  expect('**左键点树上别处会关掉右键菜单**', r.menuClosedByClick === true, JSON.stringify(r).slice(0, 200));
  expect('文件加锁**不弹备注框**', r.fileAskedComment === false, JSON.stringify(r).slice(0, 200));
  expect('加锁后树上出现「已锁·我」', (r.fileTagged || '').includes('已锁'), JSON.stringify(r).slice(0, 300));
  expect('文件上的解锁按钮可用', r.fileUnlockEnabled === true, JSON.stringify(r).slice(0, 200));
  expect('文件解锁后标记消失', r.fileCleared === true, JSON.stringify(r).slice(0, 200));
  expect('点选目录后"当前"跟着变成目录', (r.dirCurrent || '').includes('characters'), r.dirCurrent);
  expect('目录上给出"按整棵子树生效"的提示', (r.dirHint || '').includes('整棵子树'), r.dirHint);
  expect('目录的加锁按钮显示待加锁文件数', /（\d+）/.test(r.lockLabel || ''), r.lockLabel);
  expect('目录批量加锁也**不弹框**', r.dirAskedComment === false, JSON.stringify(r).slice(0, 200));
  expect('批量加锁给出"已锁定 N 个文件"的说明', (r.lockFlash || '').includes('已锁定'), r.lockFlash);
  expect('目录上出现「已锁 2」聚合标记', /已锁\s*2/.test(r.dirTag || ''), r.dirTag);
  expect('目录的解锁按钮按子树计数', /（\d+）/.test(r.unlockLabel || ''), r.unlockLabel);
  expect('树上解锁后聚合标记消失', r.dirCleared === true, JSON.stringify(r).slice(0, 200));
});
step('锁 B：别人持锁 → 树上可见 → 强制解锁（原因必填）→ 消失', async (c) => {
  // "同事"占住 props/table.png（REST 直连，模拟另一个人正在改这个文件）
  await heldByOther('props/table.png');

  const r = await c.run(async () => {
    const h = globalThis.__h;
    const dbl = (el) => el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    await h.goto('工作副本');

    // 锁列表每 15s 静默刷新一次。**折叠状态下也能看见**：目录节点上有「他人锁 N」聚合标记
    // （props 目录当时是干净的、默认折叠，所以这一条正是用户实际会看到的信号）。
    const dirTag = await h.waitFor(() => {
      const el = h.tid('wc-dirlock-other-props');
      return el ? h.vtext(el) : null;
    }, 40000);

    // 展开 props（用"只在未展开时才点"的助手），文件的锁标记才在 DOM 里
    h.ensureExpanded('props');
    const tag = await h.waitFor(() => {
      const el = h.tid('wc-lock-props/table.png');
      return el ? h.vtext(el) : null;
    }, 20000);
    if (!tag) return { err: '树上没有显示他人持锁', dirTag: dirTag || '' };

    // 右键它：菜单里"加锁"应禁用（别人占着），"强制解锁"可用
    h.openMenu('props/table.png');
    const menu = await h.waitFor(() => h.menuItem('force-unlock') || null, 8000);
    if (!menu) return { err: '右键没有弹出菜单', tag, dirTag: dirTag || '' };
    const lockDisabled = h.menuItem('lock').disabled;
    const breakLabel = h.vtext(menu);
    if (menu.disabled) return { err: '强制解锁项禁用（他人锁没被识别）', tag, dirTag: dirTag || '' };

    menu.click();
    const box = await h.waitFor(
      () => document.querySelector('.el-message-box__input input') || null,
      15000,
    );
    const askedReason = !!box;
    if (box) h.setInput(box, '人已离职，接管文件');
    const okBtn = await h.waitFor(
      () => document.querySelector('.el-message-box__btns .el-button--primary') || null,
      15000,
    );
    if (okBtn) okBtn.click();

    // 给足 45s：锁标记的刷新有两条路 —— 解锁后主动 `GET /locks`，以及每 15s 的静默轮询。
    // 前端偶发一次刷新失败也不该让断言变红（最坏等两轮轮询即可）。
    const gone = await h.waitFor(
      () => (h.tid('wc-lock-props/table.png') ? null : true),
      45000,
    );
    const dirTagGone = await h.waitFor(
      () => (h.tid('wc-dirlock-other-props') ? null : true),
      30000,
    );
    // 诊断：标记没消失时，要知道是"数据没刷新"还是"节点根本没渲染"
    const rendered = h
      .$$('[data-testid^="wc-node-"]')
      .map((el) => el.getAttribute('data-testid'))
      .slice(0, 20);
    return {
      tag: tag || '',
      dirTag: dirTag || '',
      dirTagGone: !!dirTagGone,
      lockDisabled,
      breakLabel,
      askedReason,
      gone: !!gone,
      rendered,
      flash: h.tid('flash') ? h.vtext(h.tid('flash')) : '',
    };
  });

  expect('折叠的目录上也显示「他人锁 N」', /他人锁\s*1/.test(r.dirTag || ''), JSON.stringify(r).slice(0, 300));
  expect('展开后看到他人持锁（已锁·e2eholder）', (r.tag || '').includes('已锁'), JSON.stringify(r).slice(0, 300));
  expect('被他人持锁时加锁按钮禁用', r.lockDisabled === true, JSON.stringify(r).slice(0, 200));
  expect('按钮文案是「强制解锁」', (r.breakLabel || '').includes('强制解锁'), r.breakLabel);
  expect('强制解锁要求填写原因', r.askedReason === true, JSON.stringify(r).slice(0, 200));
  // 失败时把服务端锁状态与树上实际渲染的节点一并打出来（"标记不消失"要知道是数据没刷新
  // 还是节点根本没渲染）
  const serverLocks = await api('GET', `/api/v1/repos/${REPO}/locks`);
  expect(
    '强制解锁后文件标记消失',
    r.gone === true,
    `${JSON.stringify(r).slice(0, 400)} | 服务端锁=${JSON.stringify((serverLocks.items ?? []).map((l) => l.path))}`,
  );
  expect('强制解锁后目录聚合标记也消失', r.dirTagGone === true, JSON.stringify(r).slice(0, 200));
});

step('目录树双击展开 / 收起（单击只选中）', async (c) => {
  const r = await c.run(async () => {
    const h = globalThis.__h;
    const dbl = (el) => el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true }));
    /** 收起/展开在 el-tree 里是 display 切换，**元素仍在 DOM**：必须判可见性。 */
    const visible = (el) => !!el && el.offsetParent !== null && el.getBoundingClientRect().height > 0;

    await h.goto('工作副本');
    const dir = await h.waitFor(() => h.tid('wc-node-characters'), 20000);
    if (!dir) return { err: '树上没有 characters 节点' };
    h.ensureExpanded('characters'); // 先确保是展开态，下面才好断言"双击→收起"
    await h.sleep(300);
    const childVisibleBefore = visible(h.tid('wc-node-characters/hero.psd'));

    // 单击只"选中"，不该改变展开状态
    dir.click();
    await h.sleep(300);
    const afterSingleClick = visible(h.tid('wc-node-characters/hero.psd'));
    const currentAfterSingle = h.vtext(h.tid('wc-current'));

    // 双击 → 收起。
    //
    // 断言"**节点状态**已收起"（展开箭头上的 expanded 类），而不是"子节点像素消失"：
    // 收起是个 CSS 过渡，在**被遮挡/无头的窗口里会被 rAF 节流**，过渡可能拖到好几秒 ——
    // 等像素会让这条断言变成时序赌博（踩过：状态早就收起了，DOM 还在慢慢过渡）。
    const caretExpanded = () => {
      const label = h.tid('wc-node-characters');
      const content = label && label.closest('.el-tree-node__content');
      const caret = content && content.querySelector('.el-tree-node__expand-icon');
      return !!caret && caret.classList.contains('expanded');
    };
    const wasExpanded = caretExpanded();
    dbl(dir);
    const collapsed = await h.waitFor(() => (caretExpanded() ? null : true), 10000);
    // 像素级再确认一次（在**重新展开之前**测，否则测的是展开后的状态 —— 我踩过这个顺序坑）
    const childHidden = await h.waitFor(
      () => (visible(h.tid('wc-node-characters/hero.psd')) ? null : true),
      30000,
    );
    // 再双击 → 展开
    dbl(h.tid('wc-node-characters'));
    const expanded = await h.waitFor(() => (caretExpanded() ? true : null), 10000);

    return {
      childVisibleBefore,
      afterSingleClick,
      currentAfterSingle,
      wasExpanded,
      collapsed: !!collapsed,
      expanded: !!expanded,
      childHidden: !!childHidden,
    };
  });

  expect('双击前子节点可见', r.childVisibleBefore === true, JSON.stringify(r).slice(0, 200));
  expect('双击前目录处于展开态', r.wasExpanded === true, JSON.stringify(r).slice(0, 200));
  expect('单击只选中、不改变展开状态', r.afterSingleClick === true, JSON.stringify(r).slice(0, 200));
  expect('单击把该目录设为"当前"', (r.currentAfterSingle || '').includes('characters'), r.currentAfterSingle);
  expect('双击目录后收起（节点状态已收起）', r.collapsed === true, JSON.stringify(r).slice(0, 200));
  expect('再次双击后展开（节点状态又展开）', r.expanded === true, JSON.stringify(r).slice(0, 200));
  expect('收起后子节点最终会隐藏（过渡完成后）', r.childHidden === true, JSON.stringify(r).slice(0, 200));
});
step('部分检出：在目录树上勾选 characters → 只检出该子树', async (c) => {
  const sparseDir = join(WORK, 'wc-sparse');

  const r = await c.run(async (dir, repo) => {
    const h = globalThis.__h;
    h.clickTid('menu-repos');

    const rows = await h.waitFor(() => {
      const all = h.rows();
      return all.some((x) => h.vtext(x).includes(repo)) ? all : null;
    }, 25000);
    if (!rows) return { err: '仓库列表未就绪', rows: h.rowTexts() };
    rows.find((x) => h.vtext(x).includes(repo)).click();

    const tree = await h.waitFor(() => h.tid('file-tree'), 20000);
    if (!tree) return { err: '目录树没有出现（选中仓库后应挂载）' };

    // 懒加载根一层
    const node = await h.waitFor(() => h.tid('node-characters'), 25000);
    if (!node) return { err: '根目录节点未加载出来' };

    const row = node.closest('.el-tree-node__content');
    const box = row && row.querySelector('input[type="checkbox"]');
    if (!box) return { err: '树节点上没有勾选框' };
    box.click();
    await h.sleep(500);

    const desc = h.tid('sparse-desc') ? h.vtext(h.tid('sparse-desc')) : '';
    const tags = h.$$('[data-testid="sparse-tag"]').map(h.vtext);
    const dirHint = h.tid('checkout-dir-hint') ? h.vtext(h.tid('checkout-dir-hint')) : '';

    h.setInput('checkout-dir', dir);
    h.clickTid('checkout-submit');
    const landed = await h.waitFor(() => {
      const el = h.tid('head-root');
      return el && h.vtext(el).includes('wc-sparse') ? true : null;
    }, 60000);
    await h.sleep(600);
    return {
      desc,
      tags,
      dirHint,
      landed: !!landed,
      // 部分检出后工作副本应当是干净的——前缀目录本身也必须有基线条目
      // （回归：曾漏登记前缀目录，界面上会多出一个"未纳管"目录）
      summary: h.tid('head-summary') ? h.vtext(h.tid('head-summary')) : '',
    };
  }, sparseDir, REPO);

  expect('仓库页挂上目录树并加载根节点', !r.err, JSON.stringify(r).slice(0, 300));
  expect('勾选目录后出现已选前缀标签', (r.tags || []).some((t) => t.includes('characters')), JSON.stringify(r.tags));
  expect('描述文案说明是部分检出', (r.desc || '').includes('部分检出'), r.desc);
  expect('部分检出完成（顶栏指向新工作副本）', r.landed === true, JSON.stringify(r).slice(0, 200));
  expect(
    '检出后没有"未纳管"条目（前缀目录也有基线）',
    !(r.summary || '').includes('未纳管') && (r.summary || '').includes('干净'),
    r.summary,
  );

  // 磁盘验收：只应拉下 characters/ 子树
  expect('前缀内的文件已落盘', existsSync(join(sparseDir, 'characters', 'hero.psd')));
  let hero = '';
  try {
    hero = readFileSync(join(sparseDir, 'characters', 'hero.psd'), 'utf8');
  } catch {
    hero = '';
  }
  expect('前缀内文件内容正确', hero === 'HERO-V1', JSON.stringify(hero));
  expect(
    '前缀外的目录一个都没拉下来',
    !existsSync(join(sparseDir, 'docs')) && !existsSync(join(sparseDir, 'props')),
    `docs=${existsSync(join(sparseDir, 'docs'))} props=${existsSync(join(sparseDir, 'props'))}`,
  );
});

step('关掉工作副本后，仓库页的目录树照样能加载（回归）', async (c) => {
  const r = await c.run(async (repo) => {
    const h = globalThis.__h;
    await h.goto('工作副本');
    h.clickTid('wc-close');
    const closed = await h.waitFor(() => (h.tid('wc-tree') ? null : true), 15000);

    await h.goto('仓库');
    const rows = await h.waitFor(() => (h.rowTexts().length > 0 ? h.rowTexts() : null), 20000);
    const target = h.rows().find((row) => h.vtext(row).includes(repo));
    if (target) target.click();
    const node = await h.waitFor(
      () => h.tid('node-characters') || h.tid('tree-error') || null,
      20000,
    );
    return {
      closed: !!closed,
      rows: rows || [],
      node: !!node,
      nodeText: h.tid('node-characters') ? h.vtext(h.tid('node-characters')) : '',
      treeError: h.tid('tree-error') ? h.vtext(h.tid('tree-error')) : '',
      emptyText: h.body().includes('暂无数据'),
    };
  }, REPO);

  expect('工作副本已关闭（此时没有任何副本打开）', r.closed === true, JSON.stringify(r).slice(0, 200));
  expect(
    '**没打开工作副本时，仓库页的目录树照样出来**',
    r.node === true && !r.treeError,
    JSON.stringify(r).slice(0, 300),
  );
  expect('不是「暂无数据」', r.emptyText === false, JSON.stringify(r).slice(0, 200));
});

step('服务端侧对账：提交与锁都留了痕迹', async (c) => {
  const log = await api('GET', `/api/v1/repos/${REPO}/log?limit=10`);
  const revs = (log.items ?? []).map((r) => r.rev).sort((a, b) => a - b);
  expect('服务端修订历史包含客户端提交', revs.includes(2), JSON.stringify(revs));

  const locks = await api('GET', `/api/v1/repos/${REPO}/locks`);
  expect('解锁后服务端无残留锁', (locks.items ?? []).length === 0, JSON.stringify(locks.items));
  void c;
});

// ---------------------------------------------------------------- 主流程

async function main() {
  if (!ELECTRON) {
    console.error('缺少 --electron 参数（打包产物就是它自己的可执行文件，--app 可省略）');
    process.exit(2);
  }
  if (SHOTS) mkdirSync(SHOTS, { recursive: true });

  console.log(`== 0. 造数（REST ${BASE}）==`);
  await loginViaApi();
  const rev = await seed();
  ok(`建仓库 ${REPO}（r${rev}）并提交 docs/readme.txt`);

  const env = { ...process.env };
  // 宿主环境会注入 ELECTRON_RUN_AS_NODE=1，那会让 Electron 退化成纯 Node、窗口永不出现
  delete env['ELECTRON_RUN_AS_NODE'];
  // 隔离配置与会话，别碰开发者真实的 ~/.b-artifact
  env['B_ARTIFACT_HOME'] = HOME_DIR;

  // 打包产物是自带 app 目录的可执行文件（.../b-artifact.app/Contents/MacOS/b-artifact），
  // 开发模式的 Electron 才需要额外传 app 目录。
  // `--user-data-dir` 指到临时目录：既隔离用户真实的 app 数据，
  // 也避免单实例锁与开发者本机正在跑的实例互相顶掉。
  const launchArgs = [
    ...(APP_DIR ? [APP_DIR] : []),
    `--user-data-dir=${join(WORK, 'userdata')}`,
    `--remote-debugging-port=${CDP_PORT}`,
    '--no-sandbox',
  ];
  const child = spawn(ELECTRON, launchArgs, { env, stdio: ['ignore', 'pipe', 'pipe'] });
  const logs = [];
  child.stdout.on('data', (b) => logs.push(String(b)));
  child.stderr.on('data', (b) => logs.push(String(b)));

  let wsUrl = null;
  for (let i = 0; i < 100; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`);
      const targets = await r.json();
      const page = targets.find((t) => t.type === 'page' && t.webSocketDebuggerUrl);
      if (page) {
        wsUrl = page.webSocketDebuggerUrl;
        break;
      }
    } catch {
      /* 还没起来 */
    }
    await sleep(300);
  }
  if (!wsUrl) {
    child.kill();
    console.error('无法连接 Electron 的调试端口。子进程输出：');
    console.error(logs.join('').slice(-2000));
    process.exit(2);
  }

  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => {
    ws.addEventListener('open', res, { once: true });
    ws.addEventListener('error', rej, { once: true });
  });
  const cdp = new Cdp(ws);
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');
  await cdp.send('Log.enable');
  await cdp.send('Runtime.evaluate', { expression: PRELUDE, returnByValue: true });

  try {
    for (const [i, s] of steps.entries()) {
      console.log(`== ${i + 1}. ${s.name} ==`);
      const failedBefore = failures.length;
      try {
        await s.fn(cdp);
      } catch (e) {
        fail(`${s.name}：执行异常`, e.message);
      }
      // 这一步新出现失败就把界面牌面打出来：省掉下一轮"猜为什么"的时间
      if (failures.length > failedBefore) {
        const dump = await cdp
          .run(() => {
            const h = globalThis.__h;
            const alertText = h
              .$$('.el-alert')
              .filter((a) => a.offsetWidth > 0)
              .map(h.vtext)
              .join(' | ');
            return {
              flash: alertText.slice(0, 300),
              head: h.tid('head-rev') ? h.vtext(h.tid('head-rev')) : '(无顶栏)',
              summary: h.tid('status-summary') ? h.vtext(h.tid('status-summary')) : '',
              rows: h.rowTexts().slice(0, 8),
              buttons: h
                .$$('button')
                .filter((b) => b.offsetWidth > 0)
                .map((b) => `${h.vtext(b)}${b.disabled ? '(禁用)' : ''}`)
                .slice(0, 14),
            };
          })
          .catch((e) => ({ dumpError: e.message }));
        console.log(`    · 界面状态：${JSON.stringify(dump)}`);
      }
      await cdp
        .shot(SHOTS, `${String(i + 1).padStart(2, '0')}-${s.name.slice(0, 14).replace(/[^\w\u4e00-\u9fa5]/g, '_')}`)
        .catch(() => {});
    }
  } finally {
    child.kill();
  }

  console.log();
  const noise = /favicon|ResizeObserver loop|DevTools|Autofill/i;
  const exceptions = cdp.exceptions.filter((e) => !noise.test(e));
  const consoleErrors = cdp.consoleErrors.filter((e) => !noise.test(e));
  if (exceptions.length) {
    console.log('渲染层未捕获异常：');
    for (const e of exceptions.slice(0, 8)) console.log(`  ! ${e.split('\n')[0]}`);
  }
  if (consoleErrors.length) {
    console.log('渲染层控制台 error：');
    for (const e of consoleErrors.slice(0, 8)) console.log(`  ! ${e.slice(0, 200)}`);
  }

  console.log(`---- 结果：${pass} 通过 / ${failures.length} 失败 ----`);
  if (failures.length || exceptions.length || consoleErrors.length) {
    console.log('主进程输出尾部：');
    console.log(logs.join('').slice(-800));
    process.exit(1);
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
