import { fileURLToPath, URL } from 'node:url';
import vue from '@vitejs/plugin-vue';
import { defineConfig } from 'vite';

// 渲染层由 Tauri 以自定义协议加载（mac/Linux 是 `tauri://localhost`，Windows 是
// `http://tauri.localhost`），所以 base 仍必须是相对路径（'./'）：用默认的 '/'
// 会让 index.html 去请求协议根的 /assets/xxx.js，直接白屏。
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
    // 系统 WebView 三平台版本差得远（WKWebView / WebView2 / WebKitGTK），
    // 不能用 chrome130 这种只对 Chromium 成立的 target。
    target: 'es2020',
    chunkSizeWarningLimit: 1600,
  },
  server: {
    port: 5273,
    strictPort: true,
  },
});
