import { fileURLToPath, URL } from 'node:url';
import vue from '@vitejs/plugin-vue';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  plugins: [vue()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src/renderer', import.meta.url)),
      '@shared': fileURLToPath(new URL('./src/shared', import.meta.url)),
    },
  },
  // Vite 5 的内置模块名单里没有 `node:sqlite`（Node 22.5+ 才加入），不显式外部化
  // 会被当成第三方包去 resolve，报 "Failed to load url sqlite"。
  ssr: { external: ['node:sqlite'] },
  test: {
    environment: 'node',
    include: ['tests/**/*.test.ts'],
    // 工作副本引擎会起真实文件 I/O，单测之间无共享状态；并发跑能压出竞态。
    pool: 'forks',
    testTimeout: 30_000,
    // 渲染层需要 DOM：只有 tests/renderer/** 走 happy-dom，引擎侧继续在 node 里跑
    environmentMatchGlobs: [['tests/renderer/**', 'happy-dom']],
    setupFiles: ['tests/renderer/setup.ts'],
  },
});
