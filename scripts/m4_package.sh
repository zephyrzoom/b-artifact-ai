#!/bin/bash
# b-artifact 客户端打包门禁（§14 的 M4 交付物：Electron 可运行包）
#
# 三件事一次做完：
#   ① 构建（主进程 / preload / 渲染层）+ 生成图标；
#   ② electron-builder 出 `.app` 与 dmg / zip 安装包；
#   ③ **拿打包产物跑一遍完整的 Electron 冒烟**（同一套 143 项断言）。
#
# 为什么 ③ 是必须的：asar 打包会改变文件布局——preload 路径、渲染层的相对资源路径、
# `__dirname` 推导的 CLIENT_ROOT 全都可能失效。只跑源码树里的冒烟是发现不了的
# （本项目已经栽过两次"组件单测/开发模式全绿、真机上不对"）。
#
# 用法：
#   scripts/m4_package.sh                 # .app + zip，并跑打包产物冒烟
#   PORT=18401 scripts/m4_package.sh
#   SKIP_SMOKE=1 scripts/m4_package.sh    # 只打包不验证（不推荐）
#
# 环境变量：PORT(默认18402) / NODE / CARGO / SKIP_SMOKE / KEEP
#
# 关于 dmg：默认不出（`electron-builder.yml` 里写明了原因）——
# 它需要额外下载 dmgbuild 工具包，而那个 host 在本沙箱完全不可达。要出就在联网环境
# 给 mac.target 加上 dmg。

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-18402}"
NODE="${NODE:-node}"
CARGO="${CARGO:-cargo}"
CLIENT="$ROOT/client"
BIN="$ROOT/server/target/debug/b-artifact-server"
APP="$CLIENT/release/mac/b-artifact.app"
APP_BIN="$APP/Contents/MacOS/b-artifact"

ADMIN="pkguser"
PASS="smoke-PW-123456"
BASE="http://127.0.0.1:$PORT"

WORK=$(mktemp -d /tmp/b-artifact-pkg.XXXXXX)
if [ "${KEEP:-0}" = "1" ]; then
  trap 'kill "$SERVER_PID" 2>/dev/null; echo "工作目录保留：$WORK"' EXIT
else
  trap 'kill "$SERVER_PID" 2>/dev/null; rm -rf "$WORK"' EXIT
fi
SERVER_PID=""
SHOTS="$WORK/shots"

echo "== 1. 构建客户端 =="
if [ ! -d "$CLIENT/node_modules" ]; then
  (cd "$CLIENT" && npm install --no-audit --no-fund) || { echo "npm install 失败"; exit 1; }
fi
(cd "$CLIENT" && npm run build > "$WORK/build.log" 2>&1 && npm run icon >> "$WORK/build.log" 2>&1) \
  || { echo "构建失败："; tail -20 "$WORK/build.log"; exit 1; }
[ -f "$CLIENT/build/icon.png" ] || { echo "缺少图标 $CLIENT/build/icon.png"; exit 1; }
echo "  ✓ dist/（main + preload + renderer）与 build/icon.png"

echo "== 2. electron-builder 打包 =="
# 走配置里的 target（当前是 zip）。首次会在 ~/Library/Caches 里下载图标工具包；
# Electron 分发用本地的（见 electron-builder.yml 的 electronDist）。
(cd "$CLIENT" && npx electron-builder > "$WORK/pack.log" 2>&1) \
  || { echo "打包失败："; tail -30 "$WORK/pack.log"; exit 1; }
[ -x "$APP_BIN" ] || { echo "打包产物缺失：$APP_BIN"; tail -20 "$WORK/pack.log"; exit 1; }
echo "  ✓ ${APP}（$(du -sh "$APP" | cut -f1)）"
echo "  ✓ asar $(du -h "$APP/Contents/Resources/app.asar" | cut -f1)（应只含 dist/ 与 package.json）"

ARTIFACTS=$(ls "$CLIENT/release"/*.dmg "$CLIENT/release"/*.zip 2>/dev/null)
if [ -n "$ARTIFACTS" ]; then
  echo "$ARTIFACTS" | while read -r f; do echo "  ✓ $(basename "$f") $(du -h "$f" | cut -f1)"; done
fi

if [ "${SKIP_SMOKE:-0}" = "1" ]; then
  echo "== 3. 跳过冒烟（SKIP_SMOKE=1）=="
  echo "---- 产物在 $CLIENT/release ----"
  exit 0
fi

echo "== 3. 启动服务端（供打包产物冒烟用）=="
if [ ! -x "$BIN" ]; then
  (cd "$ROOT/server" && "$CARGO" build --quiet) || { echo "编译失败"; exit 1; }
fi
printf '%s\n%s\n' "$PASS" "$PASS" | "$BIN" --data-dir "$WORK/data" --create-admin "$ADMIN" > "$WORK/create-admin.log" 2>&1
grep -qi "error\|失败" "$WORK/create-admin.log" && { echo "建管理员失败："; cat "$WORK/create-admin.log"; exit 1; }

"$BIN" --data-dir "$WORK/data" --listen "127.0.0.1:$PORT" > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 60); do
  if [ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/health" 2>/dev/null)" = "200" ]; then break; fi
  sleep 0.5
done
[ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/health")" = "200" ] \
  || { echo "服务端未就绪："; tail -20 "$WORK/server.log"; exit 1; }
echo "  ✓ GET /health 200"

echo "== 4. 用【打包产物】跑冒烟 =="
mkdir -p "$SHOTS"
set +e
# 注意不传 --app：打包产物自带 app 目录
"$NODE" "$ROOT/scripts/electron_smoke.mjs" \
  --base "$BASE" --user "$ADMIN" --password "$PASS" \
  --electron "$APP_BIN" --work "$WORK" --shots "$SHOTS"
RC=$?
set -e

if [ "$RC" -ne 0 ]; then
  echo "服务端日志尾部："; tail -10 "$WORK/server.log"
  echo "（打包日志见 $WORK/pack.log）"
  [ "${KEEP:-0}" = "1" ] || echo "（用 KEEP=1 重跑可保留工作目录与截图）"
else
  echo "截图：$SHOTS"
  echo "---- 安装包在 $CLIENT/release ----"
fi
exit "$RC"
