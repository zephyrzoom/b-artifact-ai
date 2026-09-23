// 渲染层测试的全局准备。
//
// 注意：这个 setup 文件对**所有**测试生效（node 环境也会加载），
// 所以 DOM 相关的东西必须先判断 `window` 存在。

import { config } from '@vue/test-utils';

// Element Plus 的部分组件挂载时会观察尺寸 / 查询媒体特性，happy-dom 没有实现
if (typeof globalThis.ResizeObserver === 'undefined') {
  class ResizeObserverStub {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  }
  globalThis.ResizeObserver = ResizeObserverStub as unknown as typeof ResizeObserver;
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
  })) as unknown as typeof window.matchMedia;
}

// 真实运行时由 Element Plus 插件注册 `v-loading`；测试里不装插件，给个空指令免得刷警告
config.global.directives = {
  loading: { mounted() {}, updated() {}, unmounted() {} },
};
