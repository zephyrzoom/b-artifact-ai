#!/usr/bin/env node
/**
 * 客户端构建：主进程 / preload（esbuild）→ 渲染层（Vite）。
 *
 * 为什么主进程不用 Vite：主进程只依赖 `electron` 与 Node 内置模块，没有 CSS / HMR 需求，
 * esbuild 一条命令就够，而且能精确控制输出格式。
 *
 * 输出（client/dist/）：
 *   main/index.js        主进程（ESM；package.json 是 type: module）
 *   preload/index.cjs    preload（**必须 CJS**：sandbox: true 的 preload 不支持 ESM）
 *   renderer/            Vue 应用（由 vite build 产出）
 *
 * 用法：node scripts/build.mjs [main|renderer|all]（默认 all）
 */

import { build } from 'esbuild';
import { spawnSync } from 'node:child_process';
import { existsSync, rmSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DIST = join(ROOT, 'dist');

async function buildMain() {
  const common = {
    bundle: true,
    platform: 'node',
    target: 'node22',
    external: ['electron'],
    sourcemap: true,
    logLevel: 'warning',
    // 只打进我们自己的代码；node_modules 一律外置（Electron 运行时能直接 require）
    packages: 'external',
  };

  await build({
    ...common,
    entryPoints: [join(ROOT, 'src/main/index.ts')],
    outfile: join(DIST, 'main/index.js'),
    format: 'esm',
  });

  await build({
    ...common,
    entryPoints: [join(ROOT, 'src/preload.ts')],
    // sandbox: true 的 preload 只能是 CommonJS
    outfile: join(DIST, 'preload/index.cjs'),
    format: 'cjs',
  });

  console.log('  ✓ dist/main/index.js + dist/preload/index.cjs');
}

function buildRenderer() {
  // **不要 spawn `node_modules/.bin/vite`**：那是给 sh 用的无扩展名脚本。
  // Windows 上 `npm ci` 生成的是 `vite` / `vite.cmd` / `vite.ps1`，只有 `.cmd` 能跑，
  // 而 spawnSync 不带 shell 时跑不了无扩展名的 `vite`；就算改指 `vite.cmd`，Node
  // ≥18.20.2/20.12.2/22 因 CVE-2024-27980 的修复也会直接 EINVAL。两种写法在 Windows 上
  // 都是死路，于是 dist/renderer 静默缺失、打包出一个开不了界面的 exe（真实踩过）。
  // 直接用当前 node 跑 vite 的 JS 入口，三平台行为一致。
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
 * 产物断言：**打包前**确认三份产物齐全。
 *
 * 为什么要单独加这道：`rmSync(DIST)` 之后若渲染层那一步失败，dist/ 里会剩下
 * main + preload，看起来「构建过」。electron-builder 照收这种半成品，最后由用户
 * 双击 exe 才看到「渲染层尚未构建」——本项目栽过一次（Windows CI 腿）。宁可在这里
 * 退出非 0，让构建失败当场可见。
 */
function assertArtifacts() {
  const required = [
    join(DIST, 'main', 'index.js'),
    join(DIST, 'preload', 'index.cjs'),
    join(DIST, 'renderer', 'index.html'),
  ];
  const missing = required.filter((p) => !existsSync(p));
  if (missing.length > 0) {
    throw new Error(
      `构建产物缺失：\n${missing.map((p) => `  - ${relative(ROOT, p)}`).join('\n')}`,
    );
  }
  console.log('  ✓ 产物齐全（main / preload / renderer）');
}

const what = process.argv[2] ?? 'all';
if (!existsSync(join(ROOT, 'node_modules'))) {
  console.error('缺少 node_modules，先跑 npm install');
  process.exit(1);
}

if (what === 'all') rmSync(DIST, { recursive: true, force: true });

if (what === 'all' || what === 'main') await buildMain();
if (what === 'all' || what === 'renderer') buildRenderer();

// 只在全量构建后断言：单独构建 main / renderer 时产物天然不齐，不是失败
if (what === 'all') assertArtifacts();
