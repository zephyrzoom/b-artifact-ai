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
import { dirname, join, resolve } from 'node:path';
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
  const r = spawnSync(join(ROOT, 'node_modules/.bin/vite'), ['build'], {
    cwd: ROOT,
    stdio: 'inherit',
  });
  if (r.status !== 0) throw new Error(`renderer 构建失败（exit ${r.status}）`);
  console.log('  ✓ dist/renderer/');
}

const what = process.argv[2] ?? 'all';
if (!existsSync(join(ROOT, 'node_modules'))) {
  console.error('缺少 node_modules，先跑 npm install');
  process.exit(1);
}

if (what === 'all') rmSync(DIST, { recursive: true, force: true });

if (what === 'all' || what === 'main') await buildMain();
if (what === 'all' || what === 'renderer') buildRenderer();
