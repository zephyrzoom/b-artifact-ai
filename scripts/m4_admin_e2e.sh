#!/bin/bash
# b-artifact 管理端 E2E（真实浏览器 + 真实服务端）
#
# 链路：构建 admin/dist → 起真实服务端（含 /admin 静态托管）→ 用 Chrome for Testing 的
# headless shell 走完 §12.4 的关键路径（登录 / 用户 / 组 / 权限矩阵 / 预览器 / 锁与强制解锁 /
# purge 二次确认 / 审计）。
#
# 与 m1/m3/m4 冒烟一样：自建服务端、独立端口、临时数据目录，可重复执行、不留残留。
#
# 用法：
#   scripts/m4_admin_e2e.sh
#   PORT=18331 scripts/m4_admin_e2e.sh
#   KEEP=1 scripts/m4_admin_e2e.sh          # 保留工作目录与截图
#
# 环境变量：PORT(默认18328) / NODE / CARGO / CHROME_BIN / SKIP_BUILD=1

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-18328}"
NODE="${NODE:-node}"
CARGO="${CARGO:-cargo}"
BIN="$ROOT/server/target/debug/b-artifact-server"
DIST="$ROOT/admin/dist"
CHROME_CACHE="$HOME/.cache/b-artifact-tools"

ADMIN="e2eadmin"
PASS="smoke-PW-123456"
BASE="http://127.0.0.1:$PORT"

WORK=$(mktemp -d /tmp/b-artifact-e2e.XXXXXX)
if [ "${KEEP:-0}" = "1" ]; then
  trap 'kill "$SERVER_PID" 2>/dev/null; echo "工作目录保留：$WORK"' EXIT
else
  trap 'kill "$SERVER_PID" 2>/dev/null; rm -rf "$WORK"' EXIT
fi
SERVER_PID=""
DATA="$WORK/data"
SHOTS="$WORK/shots"

echo "== 0. 准备浏览器 =="
if [ -z "${CHROME_BIN:-}" ]; then
  CHROME_BIN="$(ls -d "$CHROME_CACHE"/chrome-headless-shell-*/chrome-headless-shell 2>/dev/null | head -1)"
fi
if [ -z "${CHROME_BIN:-}" ] || [ ! -x "$CHROME_BIN" ]; then
  echo "  下载 Chrome for Testing headless shell（纯下载，不走 npm）…"
  mkdir -p "$CHROME_CACHE"
  if [ ! -f "$CHROME_CACHE/cft.json" ]; then
    curl -sS https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json \
      -o "$CHROME_CACHE/cft.json" || { echo "取不到版本清单"; exit 1; }
  fi
  URL=$("$NODE" -e '
    const fs=require("fs");
    const d=JSON.parse(fs.readFileSync(process.argv[1],"utf8"));
    const st=d.channels.Stable;
    const plat=process.arch==="arm64"?"mac-arm64":"mac-x64";
    const hit=st.downloads["chrome-headless-shell"].find(x=>x.platform===plat);
    if(!hit){console.error("无匹配平台");process.exit(1)}
    console.log(hit.url);
  ' "$CHROME_CACHE/cft.json") || exit 1
  (cd "$CHROME_CACHE" && curl -sSL -o cbh.zip "$URL" && unzip -oq cbh.zip -d .) || { echo "下载/解压失败"; exit 1; }
  xattr -dr com.apple.quarantine "$CHROME_CACHE"/chrome-headless-shell-* 2>/dev/null
  CHROME_BIN="$(ls -d "$CHROME_CACHE"/chrome-headless-shell-*/chrome-headless-shell 2>/dev/null | head -1)"
fi
[ -x "${CHROME_BIN:-}" ] || { echo "找不到浏览器：$CHROME_BIN"; exit 1; }
echo "  ✓ ${CHROME_BIN##*/chrome-headless-shell-*/}$("$CHROME_BIN" --version 2>/dev/null | sed 's/Google Chrome for Testing //')"

echo "== 1. 构建管理端 =="
if [ "${SKIP_BUILD:-0}" = "1" ] && [ -f "$DIST/index.html" ]; then
  echo "  SKIP_BUILD=1，沿用既有 $DIST"
else
  if [ ! -d "$ROOT/admin/node_modules" ]; then
    (cd "$ROOT/admin" && npm install --no-audit --no-fund) || { echo "npm install 失败"; exit 1; }
  fi
  (cd "$ROOT/admin" && npx vite build > "$WORK/build.log" 2>&1) || { echo "构建失败："; tail -20 "$WORK/build.log"; exit 1; }
fi
[ -f "$DIST/index.html" ] && echo "  ✓ $DIST/index.html" || { echo "缺少构建产物"; exit 1; }

echo "== 2. 启动服务端（含 /admin 托管）=="
if [ ! -x "$BIN" ]; then
  echo "  编译服务端（首次约 1 分钟）…"
  (cd "$ROOT/server" && "$CARGO" build --quiet) || { echo "编译失败"; exit 1; }
fi

printf '%s\n%s\n' "$PASS" "$PASS" | "$BIN" --data-dir "$DATA" --create-admin "$ADMIN" > "$WORK/create-admin.log" 2>&1
grep -qi "error\|失败" "$WORK/create-admin.log" && { echo "建管理员失败："; cat "$WORK/create-admin.log"; exit 1; }
echo "  ✓ 管理员 $ADMIN"

"$BIN" --data-dir "$DATA" --listen "127.0.0.1:$PORT" --admin-dir "$DIST" > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 60); do
  if [ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/health" 2>/dev/null)" = "200" ]; then break; fi
  sleep 0.5
done
if [ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/health")" != "200" ]; then
  echo "服务端未就绪，日志："; tail -20 "$WORK/server.log"; exit 1
fi
echo "  ✓ GET /health 200"
CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/admin/")
[ "$CODE" = "200" ] && echo "  ✓ GET /admin/ 200" || echo "  ! GET /admin/ 返回 ${CODE}（继续，前端可能仍可加载）"

echo "== 3. 跑 E2E =="
mkdir -p "$SHOTS"
set +e
"$NODE" "$ROOT/scripts/admin_e2e.mjs" \
  --base "$BASE" --user "$ADMIN" --password "$PASS" \
  --chrome "$CHROME_BIN" --shots "$SHOTS"
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
