#!/usr/bin/env node
/**
 * 打包门禁：断言打包产物的 app.asar 里**含渲染层入口**。
 *
 * 为什么需要这道门禁：electron-builder 只按 `files` 收集文件，**不校验应用能不能起得来**。
 * 一份「构建到一半」的 dist/（例如渲染层那步失败，只剩 main + preload）会被顺顺利利打包、
 * 上传、发布，直到用户双击 exe 才看到「渲染层尚未构建」。本项目真实栽过一次：
 * Windows CI 腿的构建失败被 pwsh 的 `$LASTEXITCODE` 覆盖成绿灯（详见 build.yml 注释）。
 *
 * 本脚本零外部依赖，直接读 asar 的 pickle header 解析出文件树——不 spawn `npx asar`
 * （它在三平台上的可执行文件名各不相同，又是一个 Windows 陷阱）。
 *
 * 用法：
 *   node scripts/check_asar.mjs <app.asar 路径>
 *   node scripts/check_asar.mjs            # 不给参数则自动在 client/release 下找
 *
 * 找不到 asar / 缺任一产物 → 退出 1，让 CI 当场红。
 */

import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

/** 必须出现在 asar 里的文件（相对 asar 根，posix 分隔）。 */
const REQUIRED = ['dist/main/index.js', 'dist/preload/index.cjs', 'dist/renderer/index.html'];

/**
 * 解析 asar 头部，收集所有文件的 posix 相对路径。
 *
 * asar 布局：8 字节 Pickle 前缀，其后是 `{headerSize, header}` 的 JSON 串
 * （偏移见下）。结构：4 字节 uint32 = 4（Pickle 长度），4 字节 = header 区总长，
 * 再 4 字节 = JSON 串长度，然后才是 JSON。本项目只需要读 JSON，不解文件内容。
 */
function listAsarFiles(asarPath) {
  const buf = readFileSync(asarPath);
  // 16 = 4(Pickle len) + 4(header 总长) + 4(JSON 串长度) + 4(JSON 串长度再重复一次)
  const headerJsonLen = buf.readUInt32LE(12);
  const header = JSON.parse(buf.slice(16, 16 + headerJsonLen).toString('utf8'));

  const out = [];
  const walk = (node, prefix) => {
    for (const [name, child] of Object.entries(node.files ?? {})) {
      const p = prefix ? `${prefix}/${name}` : name;
      if (child.files) walk(child, p);
      else out.push(p);
    }
  };
  walk(header, '');
  return out;
}

/** 在 release 下递归找 app.asar（跳过 Electron 自带的 default_app.asar）。 */
function findAsar(dir = join(ROOT, 'client', 'release'), depth = 0) {
  if (depth > 4 || !existsSync(dir)) return null;
  for (const entry of readdirSync(dir)) {
    const p = join(dir, entry);
    let st;
    try {
      st = statSync(p);
    } catch {
      continue;
    }
    if (!st.isDirectory()) {
      if (entry === 'app.asar') return p;
      continue;
    }
    const found = findAsar(p, depth + 1);
    if (found) return found;
  }
  return null;
}

const arg = process.argv[2];
const asarPath = arg ? resolve(arg) : findAsar();
if (!asarPath || !existsSync(asarPath)) {
  console.error(`找不到 app.asar${arg ? `：${asarPath}` : '（在 client/release 下）'}`);
  console.error('先跑 npm run dist:dir 或 electron-builder --dir');
  process.exit(1);
}

let files;
try {
  files = listAsarFiles(asarPath);
} catch (e) {
  console.error(`解析 asar 失败：${e.message}`);
  process.exit(1);
}

const missing = REQUIRED.filter((r) => !files.includes(r));
if (missing.length > 0) {
  console.error(`asar 缺少必需产物（${asarPath}）：`);
  for (const m of missing) console.error(`  - ${m}`);
  console.error('产物内容：');
  for (const f of files) console.error(`  ${f}`);
  process.exit(1);
}

console.log(`asar 校验通过：${asarPath}`);
for (const r of REQUIRED) console.log(`  ✓ ${r}`);