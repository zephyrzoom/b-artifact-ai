/**
 * Electron 主进程入口（§6.4）。
 *
 * 这一层刻意做得很薄：只负责把 Electron 的对象接到前面那些**已被单测覆盖**的模块上，
 * 所以这里没有业务判断，只有装配——真跑不起来的风险集中在窗口/生命周期，
 * 由 `scripts/m4_electron_smoke.sh` 用真实应用覆盖。
 */

import { existsSync, mkdirSync, watch as fsWatch } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { app, BrowserWindow, dialog, ipcMain, safeStorage, shell } from 'electron';

import { ApiClient } from '../core/api.js';
import { WorkingCopy } from '../core/wc.js';
import { buildHandlers, type HandlerDeps } from './handlers.js';
import { registerIpc, emitEvent } from './ipc.js';
import { ConfigStore } from './store.js';
import { electronSecretBox, SessionStore } from './session.js';
import {
  buildWindowOptions,
  DEV_URL_ENV,
  hardenWebContents,
  rendererTarget,
} from './window.js';

const HERE = dirname(fileURLToPath(import.meta.url));
/** 编译产物布局：dist/main/index.js → 上两级是 client/。 */
const CLIENT_ROOT = resolve(HERE, '..', '..');
const RENDERER_DIR = join(CLIENT_ROOT, 'dist', 'renderer');
const PRELOAD = join(HERE, '..', 'preload', 'index.cjs');

const devUrl = process.env[DEV_URL_ENV] ?? '';

let mainWindow: BrowserWindow | null = null;

function createWindow(): BrowserWindow {
  const win = new BrowserWindow(
    buildWindowOptions({
      preloadPath: PRELOAD,
      rendererFile: join(RENDERER_DIR, 'index.html'),
      devUrl,
    }),
  );

  const target = rendererTarget({ rendererFile: join(RENDERER_DIR, 'index.html'), devUrl });
  const allowed = devUrl ? [devUrl] : ['file://'];
  hardenWebContents(win.webContents, win.webContents.session, allowed);

  win.once('ready-to-show', () => win.show());
  win.on('closed', () => {
    mainWindow = null;
  });

  if (target.startsWith('http')) {
    void win.loadURL(target);
  } else if (existsSync(target)) {
    void win.loadFile(target);
  } else {
    // 还没构建渲染层：给一段能自解释的提示，而不是白屏
    void win.loadURL(
      `data:text/html;charset=utf-8,${encodeURIComponent(
        '<body style="font:14px -apple-system,sans-serif;padding:40px;line-height:1.8">' +
          '<h2>渲染层尚未构建</h2><p>先跑 <code>npm run build:renderer</code>，或用 ' +
          `<code>${DEV_URL_ENV}=http://127.0.0.1:5273</code> 走 dev server。</p></body>`,
      )}`,
    );
  }

  return win;
}

function buildDeps(): HandlerDeps {
  const config = new ConfigStore();
  config.load();
  const session = new SessionStore(electronSecretBox(safeStorage));

  return {
    config,
    session,
    appInfo: {
      version: app.getVersion(),
      electron: process.versions['electron'] ?? '',
      node: process.versions['node'] ?? '',
      chrome: process.versions['chrome'] ?? '',
      platform: process.platform,
      arch: process.arch,
    },
    makeClient: ({ baseUrl, token }) =>
      new ApiClient(token === undefined ? { baseUrl } : { baseUrl, token }),
    // 打开既有工作副本：服务端地址以 wc.db 里的 meta 为准，这里只提供 token
    openWorkingCopy: (root) => {
      const st = session.get();
      return WorkingCopy.open({
        root,
        client: new ApiClient({ baseUrl: st?.server ?? '', token: st?.token ?? undefined }),
        cacheDir: config.get().cacheDir,
      });
    },
    checkoutWorkingCopy: async ({ repo, dir, sparse }) => {
      const st = session.get();
      if (!st?.token) throw new Error('尚未登录');
      const client = new ApiClient({ baseUrl: st.server, token: st.token });
      // 缓存目录取配置值（§6.5 设置项）
      return WorkingCopy.checkout({
        root: dir,
        client,
        repo,
        user: st.username,
        sparse,
        cacheDir: config.get().cacheDir,
      });
    },
    pickDir: async (title) => {
      const win = mainWindow;
      const r = win
        ? await dialog.showOpenDialog(win, { title, properties: ['openDirectory', 'createDirectory'] })
        : await dialog.showOpenDialog({ title, properties: ['openDirectory', 'createDirectory'] });
      return r.canceled || r.filePaths.length === 0 ? null : r.filePaths[0]!;
    },
    revealPath: (p) => shell.showItemInFolder(p),
    /**
     * 文件监听（§6.5 自动同步）。
     *
     * `recursive: true` 在 macOS / Windows 上由系统原生支持；**Linux 上会直接抛
     * `ERR_FEATURE_UNAVAILABLE_ON_PLATFORM`** —— 那种情况下 `createWatcher` 会捕获并
     * 标记降级，界面露出"手动刷新"入口，不会让状态停在过期视图上。
     *
     * filename 在少数平台返回 Buffer，统一成 UTF-8 字符串；拿不到就报 `''`,
     * 让上层"宁可多扫一次"。
     */
    watch: (root, onEvent) => {
      const w = fsWatch(root, { recursive: true }, (_event, filename) => {
        const raw = typeof filename === 'string'
          ? filename
          : filename
            ? Buffer.from(filename).toString('utf8')
            : '';
        onEvent(raw.replace(/\\/g, '/'));
      });
      return { close: () => w.close() };
    },
    emit: (channel, payload) => emitEvent(mainWindow?.webContents ?? null, channel, payload),
    log: (m) => console.log(`[main] ${m}`),
  };
}

// 单实例锁：第二次启动把已有窗口带到前台（§6.4）
if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', () => {
    if (mainWindow) {
      if (mainWindow.isMinimized()) mainWindow.restore();
      mainWindow.focus();
    }
  });

  app.whenReady().then(() => {
    // 依赖只装配一次：ConfigStore / SessionStore 都在内存里缓存状态，
    // 建两份会让"设置改了但另一份看不到"这类问题找上门。
    const deps = buildDeps();
    deps.config.load();
    deps.session.reset();

    // 缓存目录必须存在，否则引擎第一次写就会失败
    try {
      mkdirSync(deps.config.get().cacheDir, { recursive: true });
    } catch {
      /* 交给后续具体操作报错，不阻断启动 */
    }

    registerIpc(ipcMain, buildHandlers(deps));
    mainWindow = createWindow();

    app.on('activate', () => {
      if (BrowserWindow.getAllWindows().length === 0) mainWindow = createWindow();
    });
  });

  app.on('window-all-closed', () => {
    if (process.platform !== 'darwin') app.quit();
  });
}
