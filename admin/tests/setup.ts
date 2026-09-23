// Vitest 全局准备：补齐 happy-dom 缺失、而 Element Plus / 组件运行时会用到的浏览器 API。

import { config } from '@vue/test-utils'

// 真实运行时由 Element Plus 插件注册 `v-loading`；测试里不装插件，给它一个空指令，
// 免得每个挂载用例都刷一屏 "Failed to resolve directive: loading"。
config.global.directives = {
  loading: { mounted() {}, updated() {}, unmounted() {} },
}

// Element Plus 的若干组件（table / dialog / drawer）在挂载时观察尺寸
if (typeof globalThis.ResizeObserver === 'undefined') {
  class ResizeObserverStub {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  }
  globalThis.ResizeObserver = ResizeObserverStub as unknown as typeof ResizeObserver
}

if (typeof window !== 'undefined' && typeof window.matchMedia !== 'function') {
  window.matchMedia = ((query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addListener: () => {},
    removeListener: () => {},
    addEventListener: () => {},
    removeEventListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia
}
