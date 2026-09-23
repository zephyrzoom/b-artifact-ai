import { fileURLToPath, URL } from 'node:url';
import vue from '@vitejs/plugin-vue';
import { defineConfig } from 'vite';

// 渲染层由 Electron 以 file:// 加载，所以 base 必须是相对路径（'./'）——
// 用默认的 '/' 会让 index.html 去请求 file:///assets/xxx.js，直接白屏。
export default defineConfig({
  base: './',
  root: fileURLToPath(new URL('./src/renderer', import.meta.url)),
  plugins: [vue()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src/renderer', import.meta.url)),
      '@shared': fileURLToPath(new URL('./src/shared', import.meta.url)),
    },
  },
  build: {
    outDir: fileURLToPath(new URL('./dist/renderer', import.meta.url)),
    emptyOutDir: true,
    target: 'chrome130',
    chunkSizeWarningLimit: 1600,
  },
  server: {
    port: 5273,
    strictPort: true,
  },
});
