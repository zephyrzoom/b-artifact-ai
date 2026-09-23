#!/usr/bin/env node
/**
 * b-artifact CLI（§6.4：core 可被 CLI 复用，便于自动化与测试）。
 *
 * 设计取向：
 *   - 不加依赖，手写参数解析（客户端要能被内网一键分发，依赖越少越好）；
 *   - 所有命令支持 `--json`，冒烟脚本直接断言结构化输出，不 scrape 文本；
 *   - 凭据落 `~/.b-artifact/auth.json`，权限 0600。
 */

import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { existsSync } from 'node:fs';

import { ApiClient } from './core/api.js';
import { isWorkingCopy, WC_DIR } from './core/db.js';
import { WcError, ApiError } from './core/errors.js';
import { artifactHome } from './core/home.js';
import { WorkingCopy } from './core/wc.js';

// ---------- 参数解析 ----------

type FlagValue = string | boolean | string[];
interface Parsed {
  _: string[];
  flags: Record<string, FlagValue>;
}

function parseArgs(argv: string[]): Parsed {
  const out: Parsed = { _: [], flags: {} };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === '--') {
      out._.push(...argv.slice(i + 1));
      break;
    }
    if (a.startsWith('--')) {
      const body = a.slice(2);
      const eq = body.indexOf('=');
      if (eq >= 0) {
        pushFlag(out, body.slice(0, eq), body.slice(eq + 1));
      } else if (body === 'json' || body === 'break' || body === 'verbose') {
        pushFlag(out, body, true);
      } else {
        const next = argv[i + 1];
        if (next !== undefined && !next.startsWith('-')) {
          pushFlag(out, body, next);
          i++;
        } else {
          pushFlag(out, body, true);
        }
      }
    } else if (a === '-m' || a === '-d') {
      const next = argv[i + 1];
      pushFlag(out, a === '-m' ? 'message' : 'dir', next ?? '');
      i++;
    } else {
      out._.push(a);
    }
  }
  return out;
}

function pushFlag(out: Parsed, key: string, value: string | boolean): void {
  const prev = out.flags[key];
  if (prev === undefined) out.flags[key] = value;
  else if (Array.isArray(prev)) prev.push(String(value));
  else out.flags[key] = [String(prev), String(value)];
}

function str(f: FlagValue | undefined, dflt = ''): string {
  if (f === undefined) return dflt;
  if (Array.isArray(f)) return f[0] ?? dflt;
  if (typeof f === 'boolean') return dflt;
  return f;
}
function list(f: FlagValue | undefined): string[] {
  if (f === undefined) return [];
  return Array.isArray(f) ? f : [String(f)];
}
function bool(f: FlagValue | undefined): boolean {
  return f === true || f === 'true';
}

// ---------- 凭据 ----------

/** 凭据落 `~/.b-artifact/auth.json`（`B_ARTIFACT_HOME` 可重定向，便于自动化测试隔离）。 */
const AUTH_PATH = join(artifactHome(), 'auth.json');

interface StoredAuth {
  server: string;
  token: string;
  username: string;
}

async function loadAuth(): Promise<StoredAuth | null> {
  try {
    const raw = await readFile(AUTH_PATH, 'utf8');
    return JSON.parse(raw) as StoredAuth;
  } catch {
    return null;
  }
}

async function saveAuth(a: StoredAuth): Promise<void> {
  await mkdir(dirname(AUTH_PATH), { recursive: true });
  await writeFile(AUTH_PATH, JSON.stringify(a, null, 2), { mode: 0o600 });
}

// ---------- 输出 ----------

let jsonMode = false;

function out(data: unknown): void {
  if (jsonMode) {
    process.stdout.write(`${JSON.stringify(data, null, 2)}\n`);
    return;
  }
  if (typeof data === 'string') {
    process.stdout.write(`${data}\n`);
    return;
  }
  process.stdout.write(`${JSON.stringify(data, null, 2)}\n`);
}

function fail(msg: string, code = 1): never {
  if (jsonMode) process.stdout.write(`${JSON.stringify({ ok: false, error: msg })}\n`);
  else process.stderr.write(`错误：${msg}\n`);
  process.exit(code);
}

// ---------- 客户端 ----------

async function clientFor(serverFlag: string): Promise<{ api: ApiClient; auth: StoredAuth }> {
  const auth = await loadAuth();
  const server = serverFlag || auth?.server || '';
  if (!server) fail('未指定服务端地址：先执行 `login --server <url> --username <u> --password <p>`');
  const api = new ApiClient({ baseUrl: server, token: auth?.token });
  if (!auth) fail('未登录：先执行 login');
  return { api, auth };
}

/** 定位工作副本根：从 --dir 或 cwd 向上找 `.b-artifact/wc.db`。 */
function findRoot(start: string): string {
  let dir = resolve(start);
  for (;;) {
    if (isWorkingCopy(dir)) return dir;
    const parent = resolve(dir, '..');
    if (parent === dir) return '';
    dir = parent;
  }
}

// ---------- 命令 ----------

const HELP = `b-artifact 客户端 CLI

用法：
  login     --server <url> --username <u> --password <p>
  repos
  checkout  --repo <name> --dir <path> [--sparse <prefix>...]
  status    [--dir <path>]
  add       <paths...>
  remove    <paths...>
  revert    <paths...>
  commit    -m <message> [paths...]
  update    [--dir <path>]
  lock      <path> [-m <comment>]                    # v0.4.17：只剩文件锁
  unlock    <path> [--break --reason <r>]
  locks     [path]
  log       [--limit n]

全局选项：
  --json            以 JSON 输出（供脚本断言）
`;

async function main(): Promise<void> {
  const argv = process.argv.slice(2);
  if (argv.length === 0 || argv[0] === 'help' || argv[0] === '--help') {
    process.stdout.write(HELP);
    return;
  }
  const { _, flags } = parseArgs(argv);
  const cmd = _[0]!;
  const rest = _.slice(1);
  jsonMode = bool(flags['json']);

  switch (cmd) {
    case 'login': {
      const server = str(flags['server']);
      const username = str(flags['username']);
      const password = str(flags['password']);
      if (!server || !username || !password) fail('login 需要 --server / --username / --password');
      const api = new ApiClient({ baseUrl: server });
      const r = await api.login(username, password);
      await saveAuth({ server, token: r.token, username });
      out({ ok: true, username: r.user.username, is_admin: r.user.is_admin, expires_at: r.expires_at });
      return;
    }

    case 'repos': {
      const { api } = await clientFor(str(flags['server']));
      const items = await api.listRepos();
      if (jsonMode) {
        out({ ok: true, items });
        return;
      }
      for (const r of items) {
        process.stdout.write(
          `${r.name.padEnd(24)} rev=${String(r.head_rev).padEnd(6)} role=${r.my_role.padEnd(7)} ` +
            `rw=${r.my_permissions.read ? 'r' : '-'}${r.my_permissions.write ? 'w' : '-'}  ${r.description}\n`,
        );
      }
      return;
    }

    case 'checkout': {
      const { api, auth } = await clientFor(str(flags['server']));
      const repo = str(flags['repo']);
      const dir = resolve(str(flags['dir'], process.cwd()));
      if (!repo) fail('checkout 需要 --repo');
      const sparse = list(flags['sparse']);
      const wcx = await WorkingCopy.checkout({
        root: dir,
        client: api,
        repo,
        user: auth.username,
        sparse: sparse.length > 0 ? sparse : undefined,
      });
      // 先取修订号再 close：close 之后 wc.db 句柄已关，读 revision 会炸
      const rev = wcx.revision;
      const sparsePaths = wcx.sparsePaths;
      wcx.close();
      out({
        ok: true,
        repo,
        dir,
        rev,
        sparse_paths: sparsePaths,
        user: auth.username,
        meta: WC_DIR,
      });
      return;
    }

    case 'status': {
      const root = findRoot(resolve(str(flags['dir'], process.cwd())));
      if (!root) fail('当前目录不在工作副本内');
      const { api } = await clientFor(str(flags['server']));
      const wcx = openWc(root, api);
      const meta = wcx.wc.getMeta();
      const items = await wcx.status({ forceHash: bool(flags['force-hash']) });
      wcx.close();
      if (jsonMode) {
        out({ ok: true, root, rev: meta.revision, repo: meta.repo, items });
        return;
      }
      if (items.length === 0) {
        process.stdout.write('工作副本干净\n');
        return;
      }
      for (const it of items) {
        process.stdout.write(`${it.status.padEnd(12)} ${it.path}\n`);
      }
      return;
    }

    case 'add':
    case 'remove':
    case 'revert': {
      const root = findRoot(resolve(str(flags['dir'], process.cwd())));
      if (!root) fail('当前目录不在工作副本内');
      const { api } = await clientFor(str(flags['server']));
      const wcx = openWc(root, api);
      const paths = rest.length > 0 ? rest : list(flags['path']);
      if (paths.length === 0) fail(`${cmd} 需要至少一个路径`);
      if (cmd === 'add') out({ ok: true, added: wcx.add(paths) });
      else if (cmd === 'remove') {
        await wcx.remove(paths);
        out({ ok: true, removed: paths });
      } else {
        await wcx.revert(paths);
        out({ ok: true, reverted: paths });
      }
      wcx.close();
      return;
    }

    case 'commit': {
      const root = findRoot(resolve(str(flags['dir'], process.cwd())));
      if (!root) fail('当前目录不在工作副本内');
      const { api } = await clientFor(str(flags['server']));
      const wcx = openWc(root, api);
      const message = str(flags['message']);
      if (!message) fail('commit 需要 -m <message>');
      const paths = rest.length > 0 ? rest : undefined;
      const r = await wcx.commit({ message, paths });
      wcx.close();
      out({ ok: true, rev: r.rev, committed: r.committed, replayed: r.replayed });
      return;
    }

    case 'update': {
      const root = findRoot(resolve(str(flags['dir'], process.cwd())));
      if (!root) fail('当前目录不在工作副本内');
      const { api } = await clientFor(str(flags['server']));
      const wcx = openWc(root, api);
      const r = await wcx.update();
      wcx.close();
      out({ ok: true, ...r });
      return;
    }

    case 'lock': {
      const root = findRoot(resolve(str(flags['dir'], process.cwd())));
      if (!root) fail('当前目录不在工作副本内');
      const { api } = await clientFor(str(flags['server']));
      const wcx = openWc(root, api);
      const path = rest[0] ?? str(flags['path']);
      if (!path) fail('lock 需要一个路径');
      const info = await wcx.lock(path, { comment: str(flags['message']) });
      wcx.close();
      out({ ok: true, ...info });
      return;
    }

    case 'unlock': {
      const root = findRoot(resolve(str(flags['dir'], process.cwd())));
      if (!root) fail('当前目录不在工作副本内');
      const { api } = await clientFor(str(flags['server']));
      const wcx = openWc(root, api);
      const path = rest[0] ?? str(flags['path']);
      if (!path) fail('unlock 需要一个路径');
      await wcx.unlock(path, { breakLock: bool(flags['break']), reason: str(flags['reason']) });
      wcx.close();
      out({ ok: true, path });
      return;
    }

    case 'locks': {
      const root = findRoot(resolve(str(flags['dir'], process.cwd())));
      if (!root) fail('当前目录不在工作副本内');
      const { api } = await clientFor(str(flags['server']));
      const wcx = openWc(root, api);
      const items = await wcx.listLocks(rest[0]);
      wcx.close();
      out({ ok: true, items });
      return;
    }

    case 'log': {
      const root = findRoot(resolve(str(flags['dir'], process.cwd())));
      if (!root) fail('当前目录不在工作副本内');
      const { api } = await clientFor(str(flags['server']));
      const wcx = openWc(root, api);
      const limit = Number(str(flags['limit'], '20')) || 20;
      const r = await wcx.api.log(wcx.repo, { limit });
      wcx.close();
      if (jsonMode) {
        out({ ok: true, items: r.items, total: r.total });
        return;
      }
      for (const it of r.items) {
        process.stdout.write(
          `r${String(it.rev).padEnd(6)} ${it.author.padEnd(14)} ${it.created_at}  ${it.message}\n`,
        );
      }
      return;
    }

    default:
      fail(`未知命令：${cmd}\n\n${HELP}`);
  }
}

/** 打开工作副本：repo 与服务端地址都取 wc.db 里的记录。 */
function openWc(root: string, api: ApiClient): WorkingCopy {
  return WorkingCopy.open({ root, client: api });
}

main().catch((e: unknown) => {
  if (e instanceof ApiError) {
    fail(`[${e.code}] ${e.message}（HTTP ${e.status}）`);
  }
  if (e instanceof WcError) {
    fail(`[${e.code}] ${e.message}`);
  }
  fail(e instanceof Error ? e.message : String(e));
});
