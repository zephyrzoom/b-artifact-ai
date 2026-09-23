#!/usr/bin/env node
/**
 * 启动 Electron 应用。
 *
 * 存在理由：某些宿主环境（含 WorkBuddy 自身）会在环境变量里注入
 * `ELECTRON_RUN_AS_NODE=1`，此时 Electron 二进制会退化成纯 Node 跑，
 * 表现是 `--version` 打出 Node 版本、窗口永远不出现。启动前必须清掉它。
 *
 * 用法：node scripts/start.mjs [--dev]
 */

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const env = { ...process.env };
delete env['ELECTRON_RUN_AS_NODE'];

if (!existsSync(join(ROOT, 'dist/main/index.js'))) {
  console.error('缺少构建产物，先跑：npm run build');
  process.exit(1);
}

// 解析 electron 包自带的二进制路径（path.txt 指向 Electron.app/...）
const electronEntry = join(ROOT, 'node_modules/electron/index.js');
const { default: electronPath } = await import(`file://${electronEntry}`).then(
  (m) => ({ default: m.default ?? m }),
);

const args = [ROOT, ...process.argv.slice(2)];
const child = spawn(typeof electronPath === 'string' ? electronPath : process.execPath, args, {
  cwd: ROOT,
  env,
  stdio: 'inherit',
});
child.on('exit', (code) => process.exit(code ?? 0));
