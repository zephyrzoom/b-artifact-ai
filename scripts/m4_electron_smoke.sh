#!/bin/bash
# b-artifact 客户端 Electron 冒烟（真实应用 + 真实服务端 + 真实 IPC）
#
# 链路：构建客户端（主进程 / preload / 渲染层）→ 起真实服务端 → 启动真实 Electron 应用 →
# 从外部用 CDP 驱动渲染层，每一次点击都真的走
#   渲染层 → preload(contextBridge) → IPC 白名单校验 → 主进程 → core 引擎 → 服务端。
#
# 用法：
#   scripts/m4_electron_smoke.sh
#   PORT=18333 scripts/m4_electron_smoke.sh
#   KEEP=1 scripts/m4_electron_smoke.sh        # 保留工作目录与截图
#   SKIP_BUILD=1 scripts/m4_electron_smoke.sh  # 复用已构建产物
#
# 环境变量：PORT(默认18332) / NODE / CARGO / ELECTRON_BIN / SKIP_BUILD

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-18332}"
NODE="${NODE:-node}"
CARGO="${CARGO:-cargo}"
BIN="$ROOT/server/target/debug/b-artifact-server"
CLIENT="$ROOT/client"
DIST="$CLIENT/dist"

ADMIN="e2euser"
PASS="smoke-PW-123456"
BASE="http://127.0.0.1:$PORT"

WORK=$(mktemp -d /tmp/b-artifact-elec.XXXXXX)
if [ "${KEEP:-0}" = "1" ]; then
  trap 'kill "$SERVER_PID" 2>/dev/null; echo "工作目录保留：$WORK"' EXIT
else
  trap 'kill "$SERVER_PID" 2>/dev/null; rm -rf "$WORK"' EXIT
fi
SERVER_PID=""
DATA="$WORK/data"
SHOTS="$WORK/shots"

echo "== 0. 准备 Electron 二进制 =="
if [ -z "${ELECTRON_BIN:-}" ]; then
  ELECTRON_BIN="$CLIENT/node_modules/electron/dist/Electron.app/Contents/MacOS/Electron"
fi
if [ ! -x "$ELECTRON_BIN" ]; then
  echo "  找不到 Electron 二进制：$ELECTRON_BIN"
  echo "  npm 的 postinstall 下载常被沙箱拦，用 scripts/setup_electron.sh 离线落位"
  "$ROOT/scripts/setup_electron.sh" || exit 1
  ELECTRON_BIN="$CLIENT/node_modules/electron/dist/Electron.app/Contents/MacOS/Electron"
fi
[ -x "$ELECTRON_BIN" ] || { echo "Electron 仍不可用"; exit 1; }
# 宿主可能注入了 ELECTRON_RUN_AS_NODE=1（会让 Electron 退化成纯 Node），这里清掉再问版本
echo "  ✓ $("${NODE}" -e 'console.log(process.argv[1])' "$ELECTRON_BIN") $(
  env -u ELECTRON_RUN_AS_NODE "$ELECTRON_BIN" --version 2>/dev/null | tail -1
)"

echo "== 1. 构建客户端 =="
if [ "${SKIP_BUILD:-0}" = "1" ] && [ -f "$DIST/main/index.js" ] && [ -f "$DIST/renderer/index.html" ]; then
  echo "  SKIP_BUILD=1，沿用既有 $DIST"
else
  if [ ! -d "$CLIENT/node_modules" ]; then
    (cd "$CLIENT" && npm install --no-audit --no-fund) || { echo "npm install 失败"; exit 1; }
  fi
  (cd "$CLIENT" && npm run build > "$WORK/build.log" 2>&1) || { echo "构建失败："; tail -30 "$WORK/build.log"; exit 1; }
fi
for f in "$DIST/main/index.js" "$DIST/preload/index.cjs" "$DIST/renderer/index.html"; do
  [ -f "$f" ] || { echo "缺少构建产物：$f"; exit 1; }
done
echo "  ✓ main / preload / renderer 三份产物就绪"

echo "== 2. 启动服务端 =="
if [ ! -x "$BIN" ]; then
  echo "  编译服务端（首次约 1 分钟）…"
  (cd "$ROOT/server" && "$CARGO" build --quiet) || { echo "编译失败"; exit 1; }
fi

printf '%s\n%s\n' "$PASS" "$PASS" | "$BIN" --data-dir "$DATA" --create-admin "$ADMIN" > "$WORK/create-admin.log" 2>&1
grep -qi "error\|失败" "$WORK/create-admin.log" && { echo "建管理员失败："; cat "$WORK/create-admin.log"; exit 1; }
echo "  ✓ 管理员 $ADMIN"

"$BIN" --data-dir "$DATA" --listen "127.0.0.1:$PORT" > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 60); do
  if [ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/health" 2>/dev/null)" = "200" ]; then break; fi
  sleep 0.5
done
if [ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/health")" != "200" ]; then
  echo "服务端未就绪："; tail -20 "$WORK/server.log"; exit 1
fi
echo "  ✓ GET /health 200"

echo "== 3. 跑 Electron 冒烟 =="
mkdir -p "$SHOTS"
set +e
"$NODE" "$ROOT/scripts/electron_smoke.mjs" \
  --base "$BASE" --user "$ADMIN" --password "$PASS" \
  --electron "$ELECTRON_BIN" --app "$CLIENT" --work "$WORK" --shots "$SHOTS"
RC=$?
set -e

if [ -n "$(ls -A "$SHOTS" 2>/dev/null)" ]; then
  echo "截图：$SHOTS"
fi
if [ "$RC" -ne 0 ]; then
  echo "服务端日志尾部："; tail -20 "$WORK/server.log"
  [ "${KEEP:-0}" = "1" ] || echo "（用 KEEP=1 重跑可保留工作目录与截图）"
fi
exit "$RC"
