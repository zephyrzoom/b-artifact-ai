import { fileURLToPath, URL } from 'node:url'
import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vitest/config'

// base 与生产一致（'/admin/'），这样 `import.meta.env.BASE_URL` 在测试里也是真值——
// http.ts 的 401 跳转就是基于它拼的。
export default defineConfig({
  base: '/admin/',
  plugins: [vue()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  test: {
    environment: 'happy-dom',
    include: ['tests/**/*.test.ts'],
    setupFiles: ['tests/setup.ts'],
    restoreMocks: true,
    coverage: {
      provider: 'v8',
      reporter: ['text-summary', 'text'],
      // 只统计我们自己写的逻辑；Element Plus / echarts 之类的第三方不计入
      include: ['src/**/*.{ts,vue}'],
      exclude: ['src/main.ts', 'src/env.d.ts', 'src/api/types.ts'],
    },
  },
})
