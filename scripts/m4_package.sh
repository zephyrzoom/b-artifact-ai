#!/bin/bash
# b-artifact 客户端打包门禁（v0.5.0：Tauri 2 + Rust）
#
# 四件事一条链路：
#   ① 构建渲染层（vite → dist/renderer，随后被编译期内嵌进二进制）；
#   ② `tauri build` 出 `.app` 与 `.dmg`；
#   ③ **拿打包产物自检**：首屏挂载 + 一次 IPC 往返（`--self-check`）；
#   ④ 体积门禁：超出预算直接失败（防"无声退化回大包"）。
#
# 为什么 ③ 必须用打包产物：内嵌资源、协议、CSP、二进制名全都只在打包后成立。
# Electron 时代这条链路上栽过两次（"开发模式全绿、真机上白屏"）。
#
# 用法：
#   scripts/m4_package.sh
#   SKIP_SMOKE=1 scripts/m4_package.sh     # 只打包不验证（不推荐）
#
# 环境变量：CARGO / SKIP_SMOKE / KEEP
#
# **交付形态是 zip（不是 dmg）**：`tauri.conf.json` 的 `bundle.targets` 只留 `app`。
# 原因不是偷懒：tauri-bundler 的 `bundle_dmg.sh` 在本机（macOS 13 / BSD grep）上，
# 用 `grep` 匹配挂载设备名时报 `brackets ([ ]) not balanced`，最终
# `unable to proceed with final disk image creation`（真跑过，不是猜的）。
# zip 是 macOS 上合法的分发形态（解压后拖进「应用程序」即可），与 Electron 时代一致。
#
# 体积预算（实测基线见 docs/架构设计方案.md §6.4）：
#   zip ≤ 20 MB、.app（目录）≤ 40 MB —— 超了就是又塞了不该塞的东西。

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLIENT="$ROOT/client"
BUNDLE="$CLIENT/src-tauri/target/release/bundle"
APP="$BUNDLE/macos/b-artifact.app"
APP_BIN="$APP/Contents/MacOS/b-artifact-app"
ZIP_MAX_MB=20
APP_MAX_MB=40

PASS_N=0
FAIL_N=0
ok()  { PASS_N=$((PASS_N + 1)); echo "  ✓ $1"; }
bad() { FAIL_N=$((FAIL_N + 1)); echo "  ✗ $1"; }

WORK=$(mktemp -d /tmp/b-artifact-pkg.XXXXXX)
if [ "${KEEP:-0}" = "1" ]; then
  trap 'echo "工作目录保留：$WORK"' EXIT
else
  trap 'rm -rf "$WORK"' EXIT
fi

echo "== 1. 构建渲染层 =="
if [ ! -d "$CLIENT/node_modules" ]; then
  (cd "$CLIENT" && npm ci --no-audit --no-fund) || { echo "npm ci 失败"; exit 1; }
fi
(cd "$CLIENT" && npm run build > "$WORK/build.log" 2>&1) \
  || { echo "构建失败："; tail -20 "$WORK/build.log"; exit 1; }
[ -f "$CLIENT/dist/renderer/index.html" ] && ok "dist/renderer/index.html" \
  || { echo "缺少渲染层入口"; exit 1; }

echo "== 2. tauri build（.app + zip） =="
(cd "$CLIENT" && npx tauri build > "$WORK/pack.log" 2>&1) \
  || { echo "打包失败："; tail -40 "$WORK/pack.log"; exit 1; }
[ -x "$APP_BIN" ] && ok "生成的 .app 可执行文件" || bad "缺少 $APP_BIN"
# 打 zip（ditto 保留 .app 的符号链接与权限；zip -r 会破坏 bundle 结构）
ZIP="$BUNDLE/b-artifact_0.5.0_$(uname -m).zip"
rm -f "$ZIP"
if ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP" 2>"$WORK/zip.log"; then
  ok "生成的 zip（$(basename "$ZIP")）"
else
  bad "zip 打包失败（见 $WORK/zip.log）"
fi

echo "== 3. 打包产物自检 =="
if [ "${SKIP_SMOKE:-0}" = "1" ]; then
  echo "  - 跳过（SKIP_SMOKE=1）"
else
  export B_ARTIFACT_HOME="$WORK/home"
  set +e
  OUT=$("$APP_BIN" --self-check 2>&1)
  RC=$?
  set -e
  echo "$OUT" | sed 's/^/    /'
  case "$RC" in
    0) ok "self-check 通过（首屏挂载 + IPC 双向）" ;;
    *) bad "self-check 失败（rc=${RC}）" ;;
  esac
fi

echo "== 4. 体积门禁 =="
APP_MB=$(du -sm "$APP" 2>/dev/null | cut -f1)
echo "    .app = ${APP_MB:-?} MB（预算 ${APP_MAX_MB}）/ 二进制 $(ls -l "$APP_BIN" | awk '{printf "%.1f", $5/1048576}') MB"
[ "${APP_MB:-999}" -le "$APP_MAX_MB" ] && ok ".app 体积在预算内" \
  || bad ".app 体积超预算（${APP_MB:-?} MB > $APP_MAX_MB MB）"
if [ -f "${ZIP:-}" ]; then
  ZIP_MB=$(( $(stat -f%z "$ZIP") / 1048576 ))
  echo "    zip = $ZIP_MB MB（预算 ${ZIP_MAX_MB}）"
  [ "$ZIP_MB" -le "$ZIP_MAX_MB" ] && ok "zip 体积在预算内" \
    || bad "zip 体积超预算（$ZIP_MB MB > $ZIP_MAX_MB MB）"
fi

echo "---- 结果：${PASS_N} 通过 / ${FAIL_N} 失败 ----"
[ "$FAIL_N" -eq 0 ] || exit 1
