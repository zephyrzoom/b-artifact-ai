/**
 * 窗口装配与安全基线（§6.4）。
 *
 * 这里只放**可测的纯装配逻辑**：窗口参数、导航白名单、权限一律拒绝。
 * 真正的 `new BrowserWindow(...)` 留在 `index.ts`，由 Electron 冒烟覆盖。
 */

/** 开发模式下渲染层由 Vite dev server 提供。 */
export const DEV_URL_ENV = 'B_ARTIFACT_DEV_URL';

export interface WindowOptions<T> {
  width: number;
  height: number;
  minWidth: number;
  minHeight: number;
  show: boolean;
  backgroundColor: string;
  title: string;
  autoHideMenuBar: boolean;
  webPreferences: T;
}

export interface WindowBuildInput {
  preloadPath: string;
  /** 渲染层的 file:// 入口（生产）。 */
  rendererFile: string;
  /** 开发模式 URL（优先于 rendererFile）。 */
  devUrl?: string;
  show?: boolean;
}

/**
 * 安全基线：`contextIsolation: true`、`nodeIntegration: false`、`sandbox: true`。
 * 任何一条被改掉，渲染层就能直接够到 Node —— 三条都有单测钉住。
 */
export function buildWindowOptions(input: WindowBuildInput): WindowOptions<Record<string, unknown>> {
  return {
    width: 1280,
    height: 820,
    minWidth: 960,
    minHeight: 600,
    // 先隐藏，等 ready-to-show 再显示，避免白屏闪烁
    show: input.show ?? false,
    backgroundColor: '#f5f7fa',
    title: 'b-artifact',
    autoHideMenuBar: true,
    webPreferences: {
      preload: input.preloadPath,
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webSecurity: true,
      allowRunningInsecureContent: false,
      experimentalFeatures: false,
      spellcheck: false,
      // 窗口被遮挡/最小化时，Chromium 会把页面定时器**节流**（隐藏超过 5 分钟后降到
      // 一分钟一次）。这个客户端恰恰靠后台定时器活着：15s 拉一次锁列表（§6.5
      // "锁状态要主动刷新"）——节流之后"同事刚锁了文件"要很久才显示，自动同步的
      // 观感也跟着变差。**E2E 也栽在这上面**：窗口不可见时脚本从"变慢"到"看着像卡死"。
      backgroundThrottling: false,
    },
  };
}

/** 渲染层入口：`file://` 还是 dev server。 */
export function rendererTarget(input: Pick<WindowBuildInput, 'rendererFile' | 'devUrl'>): string {
  return input.devUrl && input.devUrl.length > 0 ? input.devUrl : input.rendererFile;
}

/** 允许的导航来源：生产是 file://（应用自身），开发是 dev server。 */
export function isNavigationAllowed(url: string, allowed: readonly string[]): boolean {
  return allowed.some((prefix) => url.startsWith(prefix));
}

export interface HardenedWebContents {
  on(event: 'will-navigate', listener: (event: { preventDefault(): void }, url: string) => void): void;
  setWindowOpenHandler(handler: (details: { url: string }) => { action: 'deny' | 'allow' }): void;
}
export interface HardenedSession {
  setPermissionRequestHandler(
    handler: (contents: unknown, permission: string, callback: (granted: boolean) => void) => void,
  ): void;
}

export interface HardenResult {
  /** 被拦下的导航（供冒烟断言"外链打不开"）。 */
  blockedNavigations: string[];
  /** 被拒绝的权限请求。 */
  deniedPermissions: string[];
}

/**
 * 上锁：
 *   - `will-navigate` 只放行白名单前缀，其余 `preventDefault()`（防止被诱导跳到外部站点）；
 *   - 新窗口一律拒绝，改为让主进程决定（避免 popup 绕过 preload 沙箱）；
 *   - 权限请求（摄像头 / 麦克风 / 通知 / geolocation…）全部拒绝——本应用不需要任何一项。
 */
export function hardenWebContents(
  contents: HardenedWebContents,
  session: HardenedSession,
  allowedPrefixes: readonly string[],
): HardenResult {
  const result: HardenResult = { blockedNavigations: [], deniedPermissions: [] };

  contents.on('will-navigate', (event, url) => {
    if (!isNavigationAllowed(url, allowedPrefixes)) {
      result.blockedNavigations.push(url);
      event.preventDefault();
    }
  });

  contents.setWindowOpenHandler(({ url }) => {
    result.blockedNavigations.push(url);
    return { action: 'deny' };
  });

  session.setPermissionRequestHandler((_contents, permission, callback) => {
    result.deniedPermissions.push(permission);
    callback(false);
  });

  return result;
}
