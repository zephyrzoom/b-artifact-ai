import { fileURLToPath, URL } from 'node:url';
import vue from '@vitejs/plugin-vue';
import { defineConfig } from 'vitest/config';

// 引擎（原 src/core，现为 client/src-tauri 的 Rust 代码）由 `cargo test` 覆盖；
// 这里只跑渲染层：视图/组件/纯函数工具，201 项，与迁移前完全一致。
export default defineConfig({
  plugins: [vue()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src/renderer', import.meta.url)),
      '@shared': fileURLToPath(new URL('./src/shared', import.meta.url)),
    },
  },
  test: {
    environment: 'happy-dom',
    include: ['tests/renderer/**/*.test.ts'],
    setupFiles: ['tests/renderer/setup.ts'],
    pool: 'forks',
    testTimeout: 30_000,
  },
});
