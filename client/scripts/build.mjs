#!/usr/bin/env node
/**
 * 客户端构建（Tauri）。
 *
 * 与 Electron 时代的差别：Rust 侧由 `cargo` / `tauri build` 编译，这里**只打渲染层**。
 * `tauri.conf.json` 的 `build.frontendDist` 指向 `dist/renderer`，编译期由 tauri-codegen
 * 把前端产物内嵌进二进制——没有 asar，也没有 preload/main 两份产物。
 *
 * 用法：node scripts/build.mjs
 */

import { spawnSync } from 'node:child_process';
import { existsSync, rmSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DIST = join(ROOT, 'dist');
const RENDERER = join(DIST, 'renderer');

if (!existsSync(join(ROOT, 'node_modules'))) {
  console.error('缺少 node_modules，先跑 npm install');
  process.exit(1);
}

/**
 * **不要 spawn `node_modules/.bin/vite`**：那是给 sh 用的无扩展名脚本。
 * Windows 上 `npm ci` 生成的是 `vite` / `vite.cmd` / `vite.ps1`，只有 `.cmd` 能跑，
 * 而 spawnSync 不带 shell 时跑不了无扩展名的 `vite`；就算改指 `vite.cmd`，Node
 * ≥18.20.2/20.12.2/22 因 CVE-2024-27980 的修复也会直接 EINVAL。两种写法在 Windows 上
 * 都是死路，于是 dist/renderer 静默缺失、打出一个开不了界面的应用（真实踩过）。
 * 直接用当前 node 跑 vite 的 JS 入口，三平台行为一致。
 */
function buildRenderer() {
  rmSync(RENDERER, { recursive: true, force: true });
  const viteEntry = join(ROOT, 'node_modules', 'vite', 'bin', 'vite.js');
  if (!existsSync(viteEntry)) {
    throw new Error(`缺少 ${viteEntry}，先跑 npm install`);
  }
  const r = spawnSync(process.execPath, [viteEntry, 'build'], {
    cwd: ROOT,
    stdio: 'inherit',
  });
  if (r.error) throw new Error(`renderer 构建无法启动：${r.error.message}`);
  if (r.status !== 0) throw new Error(`renderer 构建失败（exit ${r.status}）`);
  console.log('  ✓ dist/renderer/');
}

/**
 * 产物断言：**打包前**确认渲染层入口在。
 *
 * Electron 时代这道门禁挡住过"只有 main+preload、没有 renderer"的半成品包；换成 Tauri
 * 之后同样的失误依然可能发生（vite 挂了但没人看退出码），所以保留。
 */
function assertArtifacts() {
  const required = [join(RENDERER, 'index.html')];
  const missing = required.filter((p) => !existsSync(p));
  if (missing.length > 0) {
    throw new Error(
      `构建产物缺失：\n${missing.map((p) => `  - ${relative(ROOT, p)}`).join('\n')}`,
    );
  }
  console.log('  ✓ 产物齐全（renderer/index.html）');
}

buildRenderer();
assertArtifacts();
