/**
 * b-artifact 管理端 E2E（真实浏览器，§12.4 关键路径）
 *
 * 为什么是 CDP 而不是 Playwright：本机 npm 装 puppeteer/playwright 会被沙箱 broker 拦，
 * 而 Chrome for Testing 的 headless shell 是纯下载（不走 npm），Node 22 自带 WebSocket，
 * 不需要任何依赖就能驱动。见 skill `headless-chrome-ui-verify`。
 *
 * 覆盖的关键路径：
 *   登录 → 用户 CRUD → 用户组 CRUD → 权限矩阵编辑保存（含屏障开关）→
 *   有效权限预览器 → 谁有权限反查 → 锁列表与强制解锁 → purge 的 confirm_name 二次确认 →
 *   审计查询
 *
 * 造数（建仓库 / 提交文件 / 加锁）走 REST 直连，比点界面快得多，也更稳。
 *
 * 用法：
 *   node scripts/admin_e2e.mjs --base http://127.0.0.1:18324 --user admin --password 'xxx'
 * 可选：--shots <dir> 保存截图，--chrome <path> 指定浏览器
 */

import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdirSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { join } from 'node:path'
import { setTimeout as sleep } from 'node:timers/promises'

// ---------------------------------------------------------------- 参数

function arg(name, fallback = null) {
  const i = process.argv.indexOf(`--${name}`)
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : fallback
}

const BASE = arg('base', 'http://127.0.0.1:18324').replace(/\/$/, '')
const ADMIN = arg('user', 'admin')
const PASSWORD = arg('password', 'smoke-PW-123456')
const SHOTS = arg('shots', '')
const CHROME = arg('chrome', process.env.CHROME_BIN || '')
const CDP_PORT = Number(arg('cdp-port', '9333'))

const REPO = `e2e-repo-${Date.now().toString().slice(-6)}`
const GROUP = 'e2e-artists'
const NEW_USER = 'e2e-bob'

// ---------------------------------------------------------------- 断言

let pass = 0
const failures = []
function ok(label) {
  pass += 1
  console.log(`  ✓ ${label}`)
}
function fail(label, extra = '') {
  failures.push(label)
  console.log(`  ✗ ${label}${extra ? ` — ${extra}` : ''}`)
}
function expect(label, cond, extra = '') {
  cond ? ok(label) : fail(label, extra)
}

// ---------------------------------------------------------------- 造数（REST）

let token = ''
async function api(method, path, body, raw = false) {
  const headers = { Authorization: `Bearer ${token}` }
  if (body !== undefined && !raw) headers['Content-Type'] = 'application/json'
  const res = await fetch(`${BASE}${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : raw ? body : JSON.stringify(body),
  })
  const text = await res.text()
  let parsed
  try {
    parsed = JSON.parse(text)
  } catch {
    parsed = text
  }
  if (!res.ok) throw new Error(`${method} ${path} → ${res.status} ${text.slice(0, 200)}`)
  return parsed
}

async function loginViaApi() {
  const r = await fetch(`${BASE}/api/v1/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ username: ADMIN, password: PASSWORD }),
  })
  if (!r.ok) throw new Error(`API 登录失败：${r.status} ${await r.text()}`)
  token = (await r.json()).token
}

async function createRepo() {
  await api('POST', '/api/v1/repos', {
    name: REPO,
    description: '管理端 E2E',
  })
}

/** 走两阶段提交写入一个文件（§7.3），让仓库有内容可供浏览 / purge。返回新修订号。 */
async function commitFile(path, content, baseRev) {
  const bytes = new TextEncoder().encode(content)
  const hash = createHash('sha256').update(bytes).digest('hex')
  const commitId = crypto.randomUUID()
  const changes = [
    { path, op: 'add', kind: 'file', blob_hash: hash, size: bytes.length, mode: 0, mtime: 0 },
  ]
  // v0.4.17：先锁后提交（§5.2）
  await acquireLock(path, 'e2e 造数')
  const prep = await api('POST', `/api/v1/repos/${REPO}/commit/prepare`, {
    commit_id: commitId,
    base_rev: baseRev,
    message: 'e2e 造数',
    changes,
  })
  if ((prep.need_blobs ?? []).includes(hash)) {
    await api('PUT', `/api/v1/repos/${REPO}/blobs/${hash}`, bytes, true)
  }
  const res = await api('POST', `/api/v1/repos/${REPO}/commit`, {
    commit_id: commitId,
    commit_token: prep.commit_token,
    message: 'e2e 造数',
  })
  // 提完就放锁，免得造数留下的锁影响后面的界面断言
  await releaseLock(path)
  return res.rev ?? baseRev + 1
}

async function acquireLock(path, comment) {
  await api('POST', `/api/v1/repos/${REPO}/locks`, { path, comment })
}

async function releaseLock(path) {
  const seg = path.split('/').map(encodeURIComponent).join('/')
  await api('DELETE', `/api/v1/repos/${REPO}/locks/${seg}`).catch(() => {})
}

// ---------------------------------------------------------------- CDP

const sleepMs = (ms) => sleep(ms)

class Cdp {
  constructor(ws) {
    this.ws = ws
    this.id = 0
    this.pending = new Map()
    this.exceptions = []
    this.consoleErrors = []
    this.sessionId = null
    ws.addEventListener('message', (ev) => {
      const msg = JSON.parse(ev.data)
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id)
        this.pending.delete(msg.id)
        msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result)
        return
      }
      if (msg.method === 'Runtime.exceptionThrown') {
        const d = msg.params.exceptionDetails
        this.exceptions.push(d.exception?.description || d.text || JSON.stringify(d).slice(0, 300))
      }
      if (msg.method === 'Runtime.consoleAPICalled' && msg.params.type === 'error') {
        this.consoleErrors.push(
          (msg.params.args || []).map((a) => a.value ?? a.description ?? '').join(' '),
        )
      }
    })
  }

  send(method, params = {}) {
    const id = ++this.id
    const payload = { id, method, params }
    if (this.sessionId) payload.sessionId = this.sessionId
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject })
      this.ws.send(JSON.stringify(payload))
    })
  }

  async attach() {
    const { targetId } = await this.send('Target.createTarget', { url: 'about:blank' })
    const { sessionId } = await this.send('Target.attachToTarget', { targetId, flatten: true })
    this.sessionId = sessionId
    await this.send('Page.enable')
    await this.send('Runtime.enable')
    await this.send('Log.enable')
    // 每个新 document 都注入交互助手（const 声明进全局词法环境，后续 evaluate 可见）
    await this.send('Page.addScriptToEvaluateOnNewDocument', { source: PRELUDE })
  }

  /** 在页面里跑一段真实 JS（函数会被字符串化后执行，拿不到外层作用域——这正是我们要的）。 */
  async run(fn, ...args) {
    const expr = `(${fn.toString()})(${args.map((a) => JSON.stringify(a)).join(',')})`
    const r = await this.send('Runtime.evaluate', {
      expression: expr,
      awaitPromise: true,
      returnByValue: true,
    })
    if (r.exceptionDetails) {
      const d = r.exceptionDetails
      throw new Error(d.exception?.description || d.text || 'evaluate 失败')
    }
    return r.result.value
  }

  async goto(url) {
    await this.send('Page.navigate', { url })
    await sleepMs(250)
  }

  async screenshot(dir, name) {
    if (!dir) return
    const { data } = await this.send('Page.captureScreenshot', { format: 'png' })
    writeFileSync(join(dir, `${name}.png`), Buffer.from(data, 'base64'))
  }
}

/** 注入到每个页面的交互助手。 */
const PRELUDE = `
const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => Array.from(r.querySelectorAll(s));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const vtext = (el) => (el && el.textContent ? el.textContent : '').replace(/\\s+/g, ' ').trim();
const bodyText = () => vtext(document.body);
async function waitFor(fn, ms = 12000) {
  const t0 = Date.now();
  for (;;) {
    let v = null;
    try { v = await fn(); } catch { v = null; }
    if (v) return v;
    if (Date.now() - t0 > ms) return null;
    await sleep(120);
  }
}
function setInput(el, value) {
  const proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
  Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, value);
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
}
function buttonByText(text, root = document) {
  return $$('button', root).find((b) => vtext(b).includes(text)) || null;
}
function clickButton(text, root = document) {
  const b = buttonByText(text, root);
  if (!b) throw new Error('找不到按钮：' + text);
  b.click();
  return true;
}
function isVisible(el) {
  if (!el) return false;
  const ov = el.closest('.el-overlay');
  if (ov && getComputedStyle(ov).display === 'none') return false;
  return el.offsetWidth > 0 || el.offsetHeight > 0 || getComputedStyle(el).display !== 'none';
}
function openLayer(sel) {
  return $$(sel).filter(isVisible)[0] || null;
}
function openDialog() { return openLayer('.el-dialog'); }
/** 按标题定位对话框：同一页可能出现多个（已关闭的仍在 DOM 里，靠标题区分最稳）。 */
function dialogByTitle(text) {
  return $$('.el-dialog').filter(isVisible).find((d) => vtext(d).includes(text)) || null;
}
function drawerByTitle(text) {
  return $$('.el-drawer').filter(isVisible).find((d) => vtext(d).includes(text)) || null;
}
function openDrawer() { return openLayer('.el-drawer'); }
function fieldsOf(root) {
  return $$('input, textarea', root || document).map((el) => ({
    tag: el.tagName,
    value: el.value,
    placeholder: el.placeholder || '',
  }));
}
function rowTexts() { return $$('.el-table__row').map(vtext); }
function hasRowText(t) { return rowTexts().some((r) => r.includes(t)); }
function inputByPlaceholder(ph, root = document) {
  return $$('input, textarea', root).find((i) => (i.placeholder || '').includes(ph)) || null;
}
function tabByText(text) {
  return $$('.el-tabs__item').find((t) => vtext(t).includes(text)) || null;
}
`

// ---------------------------------------------------------------- 步骤

const steps = []
const step = (name, fn) => steps.push({ name, fn })

step('登录：错误密码被拒，正确密码进入概览', async (c) => {
  await c.goto(`${BASE}/admin/login`)
  await c.run(async () => {
    await waitFor(() => inputByPlaceholder('用户名'));
  })
  // 错误密码 → 停在登录页 + 出现错误提示
  await c.run(async (base) => {
    setInput(inputByPlaceholder('用户名'), 'e2e-nobody');
    setInput(inputByPlaceholder('密码'), 'definitely-wrong');
    clickButton('登录');
    const msg = await waitFor(() => ($$('.el-message--error').length ? vtext($$('.el-message--error')[0]) : null), 8000);
    return { msg: msg || '', path: location.pathname };
  }, BASE)
  const bad = await c.run(() => ({ path: location.pathname, hasLogin: !!inputByPlaceholder('用户名') }))
  expect('错误密码不跳转（仍在登录页）', bad.hasLogin === true && bad.path.includes('/login'))

  const good = await c.run(
    async (user, pw) => {
      setInput(inputByPlaceholder('用户名'), user);
      setInput(inputByPlaceholder('密码'), pw);
      clickButton('登录');
      const landed = await waitFor(() => (location.pathname === '/admin/' ? true : null), 12000);
      if (!landed) return { landed: false, path: location.pathname, text: bodyText().slice(0, 200) };
      await waitFor(() => (bodyText().includes('概览') ? true : null), 8000);
      return { landed: true, text: bodyText().slice(0, 300) };
    },
    ADMIN,
    PASSWORD,
  )
  expect('登录成功并落到概览页', good.landed === true, JSON.stringify(good).slice(0, 200))
  expect('侧边菜单渲染（概览/仓库/用户组/审计日志）', ['概览', '仓库', '用户组', '审计日志'].every((t) => good.text.includes(t)))
})

step('用户 CRUD：新建 → 列表可见 → 编辑显示名 → 删除', async (c) => {
  await c.goto(`${BASE}/admin/users`)
  const created = await c.run(
    async (username, pw) => {
      const ready = await waitFor(() => buttonByText('新建用户'), 12000);
      if (!ready) return { err: '页面未就绪' };
      clickButton('新建用户');
      const dlg = await waitFor(() => openDialog());
      if (!dlg) return { err: '对话框未出现' };
      const inputs = () => $$('input', openDialog());
      setInput(inputs()[0], username);
      setInput(inputs()[1], pw);
      setInput(inputs()[2], 'E2E Bob');
      clickButton('保存', openDialog());
      const shown = await waitFor(() => (hasRowText(username) ? true : null), 12000);
      return { shown: !!shown, rows: rowTexts().length, text: bodyText().slice(0, 200) };
    },
    NEW_USER,
    PASSWORD,
  )
  expect('新建用户后出现在列表', created.shown === true, JSON.stringify(created).slice(0, 200))

  const edited = await c.run(async (username) => {
    const row = $$('.el-table__row').find((r) => vtext(r).includes(username));
    if (!row) return { err: '找不到新建的用户行' };
    const editBtn = $$('button', row).find((b) => vtext(b).includes('编辑'));
    if (!editBtn) return { err: '找不到编辑按钮' };
    editBtn.click();
    const dlg = await waitFor(() => openDialog());
    if (!dlg) return { err: '编辑对话框未出现' };
    const nameInput = $$('input', openDialog())[1];
    setInput(nameInput, 'E2E Bob 2');
    clickButton('保存', openDialog());
    const shown = await waitFor(() => (hasRowText('E2E Bob 2') ? true : null), 12000);
    return { shown: !!shown };
  }, NEW_USER)
  expect('编辑显示名后列表刷新', edited.shown === true, JSON.stringify(edited).slice(0, 200))
})

step('用户组 CRUD：新建组并保存成员', async (c) => {
  await c.goto(`${BASE}/admin/groups`)
  const created = await c.run(
    async (name) => {
      const ready = await waitFor(() => buttonByText('新建组'), 12000);
      if (!ready) return { err: '页面未就绪' };
      clickButton('新建组');
      const dlg = await waitFor(() => openDialog());
      if (!dlg) return { err: '对话框未出现' };
      setInput($$('input', openDialog())[0], name);
      setInput($$('textarea', openDialog())[0], 'E2E 用组');
      clickButton('保存', openDialog());
      const shown = await waitFor(() => (hasRowText(name) ? true : null), 12000);
      return { shown: !!shown };
    },
    GROUP,
  )
  expect('新建组后出现在列表', created.shown === true, JSON.stringify(created).slice(0, 200))

  const members = await c.run(
    async (group, user) => {
      const row = $$('.el-table__row').find((r) => vtext(r).includes(group));
      if (!row) return { err: '找不到组行' };
      const b = $$('button', row).find((x) => vtext(x).includes('成员'));
      if (!b) return { err: '找不到成员按钮' };
      b.click();
      const drawer = await waitFor(() => openDrawer());
      if (!drawer) return { err: '成员抽屉未出现' };
      // el-transfer 的左侧列表项点击即可移入
      const item = await waitFor(() => $$('.el-transfer-panel .el-checkbox', openDrawer()).find((el) => vtext(el).includes(user)), 8000);
      if (!item) return { err: 'transfer 中找不到该用户' };
      item.querySelector('input')?.click();
      clickButton('保存成员', openDrawer());
      const okMsg = await waitFor(() => ($$('.el-message--success').length ? vtext($$('.el-message--success')[0]) : null), 8000);
      return { saved: !!okMsg, msg: okMsg || '' };
    },
    GROUP,
    NEW_USER,
  )
  expect('成员保存成功（el-transfer 交互可用）', members.saved === true, JSON.stringify(members).slice(0, 200))
})

step('权限矩阵：新增规则 → 切换继承屏障 → 列表反映', async (c) => {
  await c.goto(`${BASE}/admin/repos/${REPO}`)
  const loaded = await c.run(async () => {
    const ready = await waitFor(() => tabByText('权限'), 15000);
    if (!ready) return { err: '仓库详情页未加载' };
    tabByText('权限').click();
    const editorReady = await waitFor(() => buttonByText('新增规则'), 15000);
    return { editorReady: !!editorReady, text: bodyText().slice(0, 200) };
  })
  expect('进入"权限"页签并加载权限矩阵', loaded.editorReady === true, JSON.stringify(loaded).slice(0, 200))

  const added = await c.run(async () => {
    clickButton('新增规则');
    const dlg = await waitFor(() => openDialog());
    if (!dlg) return { err: '新增规则对话框未出现' };
    clickButton('保存', openDialog());
    const okMsg = await waitFor(() => ($$('.el-message--success').length ? true : null), 10000);
    const shown = await waitFor(() => (hasRowText('所有人') ? true : null), 12000);
    return { saved: !!okMsg, shown: !!shown, rows: rowTexts() };
  })
  expect('新增规则保存后出现在表格', added.shown === true, JSON.stringify(added).slice(0, 300))
  expect('规则默认落在仓库根（显示为 /）', (added.rows || []).some((r) => r.includes('/') && r.includes('所有人')))

  const barrier = await c.run(async () => {
    const findRow = () => $$('.el-table__row').find((r) => vtext(r).includes('所有人'));
    const row = findRow();
    if (!row) return { err: '找不到规则行' };
    const sw = row.querySelector('.el-switch');
    if (!sw) return { err: '找不到继承开关' };
    const before = vtext(row);
    sw.click();
    // 断言必须限定在**这一行**里：表格下方的说明文字本身就含"屏障"二字，用全页文本会假通过
    const after = await waitFor(() => {
      const r = findRow();
      if (!r) return null;
      const t = vtext(r);
      return t.includes('屏障') && t !== before ? t : null;
    }, 12000);
    return { before, after: after || '', rows: rowTexts() };
  })
  expect('关闭继承后该行出现"屏障"标记（屏障可视化）', !!barrier.after, JSON.stringify(barrier).slice(0, 300))
})

step('有效权限预览器：解析结果带结论与回溯链', async (c) => {
  const r = await c.run(async () => {
    clickButton('有效权限预览器');
    const drawer = await waitFor(() => openDrawer());
    if (!drawer) return { err: '预览抽屉未出现' };
    const d = () => drawerByTitle('有效权限预览器');
    const pathInput = await waitFor(() => inputByPlaceholder('src/assets', d()), 8000);
    if (!pathInput) return { err: '找不到路径输入框' };
    setInput(pathInput, 'src/logo.png');
    clickButton('解析', d());
    const got = await waitFor(() => ((vtext(d()) || '').includes('最终权限：') ? true : null), 12000);
    const dr = d();
    return {
      got: !!got,
      // 只在抽屉内取文本：ACL 表格下方的说明文字里也含"未命中"，用全页会假通过
      text: dr ? vtext(dr) : '',
      steps: dr ? $$('.el-step', dr).map(vtext) : [],
    };
  })
  expect('预览器返回结论', r.got === true, JSON.stringify(r).slice(0, 200))
  expect('结论显示级别（只读/读写/无）', /最终权限：(只读|读写|管理|无)（\w+）/.test(r.text || ''))
  expect('回溯链逐步给出中文结论', (r.steps || []).length > 0 && (r.steps || []).every((s) => /命中|未命中|屏障截断|无规则/.test(s)), JSON.stringify(r.steps).slice(0, 200))
})

step('谁有权限：反查结果含来源归因', async (c) => {
  const r = await c.run(async () => {
    // 先关掉预览抽屉，避免两个 overlay 抢 DOM
    const closeBtn = $$('.el-drawer__close-btn').filter(isVisible)[0];
    if (closeBtn) closeBtn.click();
    await sleep(400);
    clickButton('谁有权限');
    const drawer = await waitFor(() => drawerByTitle('谁有权限'), 8000);
    if (!drawer) return { err: '反查抽屉未出现' };
    clickButton('查询', drawer);
    // 只在**这个抽屉里**等行：ACL 规则表也有行，用全页计数会假通过
    const rows = await waitFor(() => {
      const t = drawerByTitle('谁有权限');
      if (!t) return null;
      const r = $$('.el-table__row', t).map(vtext);
      return r.length > 0 ? r : null;
    }, 12000);
    return { got: !!rows, rows: rows || [] };
  })
  expect('反查返回用户列表', r.got === true, JSON.stringify(r).slice(0, 300))
  expect('列表含"来源"归因（直接规则 / 通过组 / 系统管理员穿透）', (r.rows || []).some((t) => /直接规则|通过组|系统管理员穿透/.test(t)))
})

step('锁列表与强制解锁：强制解锁需填原因并留痕', async (c) => {
  await c.goto(`${BASE}/admin/repos/${REPO}`)
  const listed = await c.run(async (path) => {
    const ready = await waitFor(() => tabByText('锁'), 15000);
    if (!ready) return { err: '详情页未加载' };
    tabByText('锁').click();
    const has = await waitFor(() => (hasRowText(path) ? true : null), 12000);
    return { has: !!has, rows: rowTexts() };
  }, 'src/logo.png')
  expect('锁列表显示服务端已存在的锁', listed.has === true, JSON.stringify(listed).slice(0, 300))

  const broke = await c.run(async (path) => {
    const row = $$('.el-table__row').find((r) => vtext(r).includes(path));
    if (!row) return { err: '找不到锁行' };
    const b = $$('button', row).find((x) => vtext(x).includes('强制解锁'));
    if (!b) return { err: '找不到强制解锁按钮' };
    b.click();
    const box = await waitFor(() => openLayer('.el-message-box'), 8000);
    if (!box) return { err: '强制解锁确认框未出现（原因必填）' };
    const input = $('input', box);
    if (!input) return { err: '确认框没有原因输入框' };
    setInput(input, '短');            // 先给一个不合法原因
    clickButton('确定', box);
    await sleep(400);
    const stillOpen = !!openLayer('.el-message-box');
    setInput(input, 'E2E 强制解锁原因');   // 合法原因（≥4 字符）
    clickButton('确定', openLayer('.el-message-box') || document);
    const cleared = await waitFor(() => (hasRowText(path) ? null : true), 12000);
    return { blockedShortReason: stillOpen, cleared: !!cleared, rows: rowTexts() };
  }, 'src/logo.png')
  expect('原因不足 4 字时确认框不放行', broke.blockedShortReason === true, JSON.stringify(broke).slice(0, 200))
  expect('填原因后强制解锁成功（锁从列表消失）', broke.cleared === true, JSON.stringify(broke).slice(0, 300))
})

step('purge 二次确认：confirm_name 不匹配时按钮禁用', async (c) => {
  const r = await c.run(
    async (repo) => {
      const dlgTitle = '清除历史（不可恢复）';
      clickButton('清除历史');
      const dlg = await waitFor(() => dialogByTitle(dlgTitle), 8000);
      if (!dlg) return { err: 'purge 对话框未出现', dialogs: $$('.el-dialog').filter(isVisible).map(vtext).slice(0, 3) };
      const d = () => dialogByTitle(dlgTitle);
      const confirmBtn = () => buttonByText('我确认', d());
      const initialDisabled = confirmBtn().disabled;

      const f = () => fieldsOf(d());
      setInput($$('input, textarea', d())[0], 'src');          // 目录前缀
      setInput($$('input, textarea', d())[1], 'E2E 合规清除');  // 原因
      const wrongNameDisabled = confirmBtn().disabled;

      setInput($$('input, textarea', d())[2], repo + '-x');    // 错误仓库名
      const wrongConfirmDisabled = confirmBtn().disabled;
      setInput($$('input, textarea', d())[2], ' ' + repo);     // 带空格也必须拒绝（逐字符比较）
      const spacedDisabled = confirmBtn().disabled;

      return { initialDisabled, wrongNameDisabled, wrongConfirmDisabled, spacedDisabled, fields: f() };
    },
    REPO,
  )
  expect('初始不可提交', r.initialDisabled === true, JSON.stringify(r))
  expect('只填前缀与原因不可提交（确认名未填）', r.wrongNameDisabled === true, JSON.stringify(r))
  expect('确认名错误时不可提交', r.wrongConfirmDisabled === true, JSON.stringify(r))
  expect('确认名带空格时也不放行（逐字符比较）', r.spacedDisabled === true, JSON.stringify(r))
})

step('purge 执行：输入正确仓库名后生效并回显统计', async (c) => {
  const r = await c.run(
    async (repo) => {
      const dlgTitle = '清除历史（不可恢复）';
      const d = () => dialogByTitle(dlgTitle);
      if (!d()) return { err: 'purge 对话框已关闭' };
      const before = fieldsOf(d());
      setInput($$('input, textarea', d())[2], repo);
      await sleep(300);
      const btn = buttonByText('我确认', d());
      const enabled = btn ? !btn.disabled : false;
      if (!enabled) return { enabled, before, after: fieldsOf(d()), btnText: btn ? vtext(btn) : null };
      btn.click();
      const shown = await waitFor(() => (vtext(d()) || '').includes('删除路径数') ? true : null, 15000);
      return { enabled: true, shown: !!shown, text: (vtext(d()) || '').slice(0, 400) };
    },
    REPO,
  )
  expect('确认名正确后按钮可用', r.enabled === true, JSON.stringify(r).slice(0, 200))
  expect('purge 执行并回显统计（删除路径数 / 受影响修订）', r.shown === true, JSON.stringify(r).slice(0, 300))
})

step('审计日志：管理动作全部留痕', async (c) => {
  await c.goto(`${BASE}/admin/audit`)
  const r = await c.run(async () => {
    const ready = await waitFor(() => ($$('.el-table__row').length > 0 ? true : null), 15000);
    if (!ready) return { err: '审计表格为空' };
    const text = bodyText();
    return {
      rows: rowTexts().length,
      hasAcl: text.includes('设置权限') || text.includes('修改权限'),
      hasBreak: text.includes('强制解锁'),
      hasPurge: text.includes('清除历史'),
      hasCreate: text.includes('创建用户'),
    };
  })
  expect('审计列表加载', r.rows > 0, JSON.stringify(r).slice(0, 200))
  expect('含权限变更留痕（acl.set / acl.update）', r.hasAcl === true)
  expect('含强制解锁留痕（lock.break）', r.hasBreak === true)
  expect('含 purge 留痕（repo.purge）', r.hasPurge === true)
  expect('含用户创建留痕（user.create）', r.hasCreate === true)
})

// ---------------------------------------------------------------- 主流程

async function main() {
  if (!CHROME) {
    console.error('缺少浏览器路径：用 --chrome 指定，或设置 CHROME_BIN')
    process.exit(2)
  }
  if (SHOTS) mkdirSync(SHOTS, { recursive: true })

  console.log(`== 0. 造数（REST 直连 ${BASE}）==`)
  await loginViaApi()
  await createRepo()
  let rev = await commitFile('src/logo.png', 'E2E-LOGO-CONTENT', 0)
  rev = await commitFile('docs/readme.txt', 'E2E-README', rev)
  await acquireLock('src/logo.png', 'E2E 造数用锁')
  ok(`建仓库 ${REPO}（HEAD r${rev}）并提交 2 个文件 + 1 把锁`)

  const chrome = spawn(
    CHROME,
    [
      '--headless',
      '--disable-gpu',
      '--no-sandbox',
      '--hide-scrollbars',
      `--remote-debugging-port=${CDP_PORT}`,
      `--user-data-dir=/tmp/b-artifact-e2e-profile-${process.pid}`,
      '--window-size=1440,1000',
      'about:blank',
    ],
    { stdio: 'ignore' },
  )

  let wsUrl = null
  for (let i = 0; i < 80; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${CDP_PORT}/json/version`)
      wsUrl = (await r.json()).webSocketDebuggerUrl
      if (wsUrl) break
    } catch {
      /* 还没起来 */
    }
    await sleepMs(300)
  }
  if (!wsUrl) {
    chrome.kill()
    console.error('无法启动 headless Chrome')
    process.exit(2)
  }

  const ws = new WebSocket(wsUrl)
  await new Promise((res, rej) => {
    ws.addEventListener('open', res, { once: true })
    ws.addEventListener('error', rej, { once: true })
  })
  const cdp = new Cdp(ws)
  await cdp.attach()

  try {
    for (const [i, s] of steps.entries()) {
      console.log(`== ${i + 1}. ${s.name} ==`)
      try {
        await s.fn(cdp)
      } catch (e) {
        fail(`${s.name}：执行异常`, e.message)
      }
      await cdp.screenshot(SHOTS, `${String(i + 1).padStart(2, '0')}-${s.name.slice(0, 12).replace(/[^\w\u4e00-\u9fa5]/g, '_')}`)
    }
  } finally {
    chrome.kill()
  }

  console.log()
  if (cdp.exceptions.length) {
    console.log('页面未捕获异常：')
    for (const e of cdp.exceptions.slice(0, 10)) console.log(`  ! ${e.split('\n')[0]}`)
  }
  const noise = /favicon|ResizeObserver loop/i
  const realErrors = cdp.consoleErrors.filter((e) => !noise.test(e))
  if (realErrors.length) {
    console.log('控制台 error：')
    for (const e of realErrors.slice(0, 10)) console.log(`  ! ${e.slice(0, 200)}`)
  }

  console.log(`---- 结果：${pass} 通过 / ${failures.length} 失败 ----`)
  const crashed = cdp.exceptions.filter((e) => !noise.test(e))
  if (failures.length || crashed.length || realErrors.length) {
    if (crashed.length) console.log(`（另有 ${crashed.length} 条未捕获异常）`)
    process.exit(1)
  }
}

main().catch((e) => {
  console.error(e)
  process.exit(2)
})
