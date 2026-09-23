#!/bin/bash
# b-artifact M4 客户端冒烟（core + CLI 端到端，§12.8 交付门禁）
#
# 自建服务端 → CLI 走完 登录 / 建仓 / 检出 / 状态 / 提交 / 更新 / 冲突 / 删除 / 锁
# 全生命周期，全部断言 CLI 的 --json 结构化输出（不 scrape 文本）。
#
# 与 m1/m3 冒烟不同：本脚本**自己拉起服务端**（独立端口 + 临时数据目录），
# 可重复执行、不留残留。客户端的凭据与全局缓存通过 B_ARTIFACT_HOME 重定向到
# 临时目录，绝不会碰到开发者真实的 ~/.b-artifact。
#
# 用法：
#   scripts/m4_smoke.sh
#   PORT=18325 scripts/m4_smoke.sh
#
# 环境变量：PORT(默认18324) / NODE(默认node) / CARGO(默认cargo) / KEEP=1(保留工作目录)

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${PORT:-18324}"
NODE="${NODE:-node}"
CARGO="${CARGO:-cargo}"
BIN="$ROOT/server/target/debug/b-artifact-server"

ADMIN="m4admin"
BOB="m4bob"
PASS="smoke-PW-123456"
BASE="http://127.0.0.1:$PORT"
B="$BASE/api/v1"

PASS_N=0; FAIL_N=0
ok()   { PASS_N=$((PASS_N+1)); echo "  ✓ $1"; }
bad()  { FAIL_N=$((FAIL_N+1)); echo "  ✗ $1"; }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (期望 [$2] 实际 [$3])"; fi; }
jq_get() { python3 -c "import json,sys;d=json.load(sys.stdin);print($1)"; }
token() { python3 -c "import json;print(json.load(open('$AH/auth.json'))['token'])"; }

WORK=$(mktemp -d /tmp/b-artifact-m4.XXXXXX)
if [ "${KEEP:-0}" = "1" ]; then
  trap 'kill "$SERVER_PID" 2>/dev/null; echo "工作目录保留：$WORK"' EXIT
else
  trap 'kill "$SERVER_PID" 2>/dev/null; rm -rf "$WORK"' EXIT
fi

DATA="$WORK/data"
# 客户端凭据与全局缓存都落在 B_ARTIFACT_HOME 下的 .b-artifact（core/home.ts）
export B_ARTIFACT_HOME="$WORK/home"
AH="$B_ARTIFACT_HOME/.b-artifact"
mkdir -p "$AH"
SERVER_PID=""

# ---- 0. 编译并启动服务端 ----
echo "== 0. 启动服务端 =="
if [ ! -x "$BIN" ]; then
  echo "  编译服务端（首次约 1 分钟）…"
  (cd "$ROOT/server" && "$CARGO" build --quiet) || { echo "编译失败"; exit 1; }
fi
[ -x "$BIN" ] && ok "服务端二进制就绪" || { echo "缺少 $BIN"; exit 1; }

printf '%s\n%s\n' "$PASS" "$PASS" | "$BIN" --data-dir "$DATA" --create-admin "$ADMIN" > "$WORK/create-admin.log" 2>&1
grep -qi "error\|失败" "$WORK/create-admin.log" && { echo "建管理员失败："; cat "$WORK/create-admin.log"; exit 1; }
ok "创建管理员 $ADMIN"

"$BIN" --data-dir "$DATA" --listen "127.0.0.1:$PORT" > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 60); do
  if [ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/health" 2>/dev/null)" = "200" ]; then break; fi
  sleep 0.5
done
check "GET /health" "200" "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/health")"

# CLI：node 直驱 tsx，stderr 单独收（Node 的 SQLite ExperimentalWarning 不干扰断言）
CLI=( "$NODE" "$ROOT/client/node_modules/.bin/tsx" "$ROOT/client/src/cli.ts" )
# 原始 JSON 一并落盘，断言失败时能立刻看到 CLI 到底返回了什么
cli() { "${CLI[@]}" "$@" 2>>"$WORK/cli.err" | tee -a "$WORK/cli.log"; }

cd "$WORK"

# ---- 1. 登录 ----
echo "== 1. 登录与凭据 =="
R=$(cli login --server "$BASE" --username "$ADMIN" --password "$PASS" --json)
check "login --json → ok" "True" "$(echo "$R" | jq_get 'd["ok"]')"
check "login 用户名" "$ADMIN" "$(echo "$R" | jq_get 'd["username"]')"
check "首个登录者是管理员" "True" "$(echo "$R" | jq_get 'd["is_admin"]')"
R=$(cli login --server "$BASE" --username "$ADMIN" --password "wrong-password" --json)
case "$R" in *UNAUTHENTICATED*) ok "错误密码 → UNAUTHENTICATED";; *) bad "错误密码应报 UNAUTHENTICATED，实际：$R";; esac
# 重新登录回管理员（上面失败登录不改凭据，保险起见再登一次）
cli login --server "$BASE" --username "$ADMIN" --password "$PASS" --json > /dev/null

# ---- 2. 建仓 ----
echo "== 2. 建仓 =="
REPO="m4-$(date +%s)"
R=$(curl -s -w "\n%{http_code}" -X POST $B/repos -H "Content-Type: application/json" \
  -H "Authorization: Bearer $(token)" \
  -d "{\"name\":\"$REPO\",\"description\":\"M4 冒烟\"}")
check "POST /repos 建仓" "201" "$(echo "$R" | tail -1)"
R=$(cli repos --json)
check "cli repos 含新仓" "True" "$(echo "$R" | jq_get "any(i['name']=='$REPO' for i in d['items'])")"

# ---- 3. 检出空仓 ----
echo "== 3. 检出 =="
R=$(cli checkout --repo "$REPO" --dir wc-a --json)
check "checkout 空仓 → ok" "True" "$(echo "$R" | jq_get 'd["ok"]')"
check "checkout 初始修订" "0" "$(echo "$R" | jq_get 'd["rev"]')"
[ -f "$WORK/wc-a/.b-artifact/wc.db" ] && ok "wc.db 已建立" || bad "缺少 wc.db"
R=$(cli checkout --repo "$REPO" --dir wc-a --json)
case "$R" in *NOT_A_WORKING_COPY*) ok "重复检出被拒绝";; *) bad "重复检出应报 NOT_A_WORKING_COPY，实际：$R";; esac

# ---- 4. 本地改动与状态 ----
echo "== 4. 状态机 =="
mkdir -p "$WORK/wc-a/characters"
printf 'hero-v1' > "$WORK/wc-a/characters/hero.psd"
printf 'png-v1' > "$WORK/wc-a/characters/normal.png"
printf 'junk' > "$WORK/wc-a/scratch.tmp"
printf '*.tmp\n' > "$WORK/wc-a/.b-artifactignore"

R=$(cli status --dir wc-a --json)
check "status 仓库名" "$REPO" "$(echo "$R" | jq_get 'd["repo"]')"
check "新文件 → unversioned" "True" "$(echo "$R" | jq_get 'any(i["path"]=="characters/hero.psd" and i["status"]=="unversioned" for i in d["items"])')"
check ".b-artifactignore 生效（scratch.tmp 被忽略）" "True" "$(echo "$R" | jq_get 'any(i["path"]=="scratch.tmp" and i["status"]=="ignored" for i in d["items"])')"

R=$(cli add --dir wc-a characters/hero.psd characters/normal.png .b-artifactignore --json)
check "add 三个路径" "3" "$(echo "$R" | jq_get 'd["added"]')"
R=$(cli status --dir wc-a --json)
check "add 后 → added" "True" "$(echo "$R" | jq_get 'any(i["path"]=="characters/hero.psd" and i["status"]=="added" for i in d["items"])')"

# ---- 5. 提交 ----
echo "== 5. 提交 =="
R=$(cli commit --dir wc-a -m "init assets" --json)
check "commit → ok" "True" "$(echo "$R" | jq_get 'd["ok"]')"
check "提交后修订号" "1" "$(echo "$R" | jq_get 'd["rev"]')"
check "被忽略的文件没进提交" "False" "$(echo "$R" | jq_get "any('scratch.tmp' in p for p in d['committed'])")"
R=$(cli status --dir wc-a --json)
check "提交后全部 normal（除 ignored）" "True" "$(echo "$R" | jq_get 'all(i["status"] in ("normal","ignored") for i in d["items"])')"
R=$(cli commit --dir wc-a -m "again" --json)
case "$R" in *NO_CHANGES*) ok "无改动提交 → NO_CHANGES";; *) bad "无改动应报 NO_CHANGES，实际：$R";; esac
R=$(cli log --dir wc-a --json)
check "log 有提交记录" "True" "$(echo "$R" | jq_get 'len(d["items"]) >= 1')"
check "log 含提交说明" "init assets" "$(echo "$R" | jq_get 'd["items"][-1]["message"]')"

# ---- 6. 稀疏检出第二个工作副本 ----
echo "== 6. 稀疏检出 =="
R=$(cli checkout --repo "$REPO" --dir wc-b --sparse characters --json)
check "稀疏 checkout → ok" "True" "$(echo "$R" | jq_get 'd["ok"]')"
check "稀疏前缀记录" "characters" "$(echo "$R" | jq_get 'd["sparse_paths"][0]')"
[ -f "$WORK/wc-b/characters/hero.psd" ] && ok "稀疏前缀内文件已检出" || bad "稀疏文件缺失"
[ -f "$WORK/wc-b/.b-artifactignore" ] && bad "稀疏前缀外文件不应检出" || ok "稀疏前缀外文件未检出"
check "稀疏检出内容正确" "hero-v1" "$(cat "$WORK/wc-b/characters/hero.psd")"

# ---- 7. 双向修改与更新 ----
echo "== 7. 更新 =="
printf 'hero-v2-from-B' > "$WORK/wc-b/characters/hero.psd"
R=$(cli add --dir wc-b characters/hero.psd --json)
check "B 侧 add 已纳管文件（记为 modify）" "1" "$(echo "$R" | jq_get 'd["added"]')"
R=$(cli commit --dir wc-b -m "B: hero v2" --json)
check "B 侧提交 → rev 2" "2" "$(echo "$R" | jq_get 'd["rev"]')"

R=$(cli update --dir wc-a --json)
check "A 侧 update → rev 2" "2" "$(echo "$R" | jq_get 'd["rev"]')"
check "A 侧更新了 hero.psd" "True" "$(echo "$R" | jq_get "any('characters/hero.psd' in p for p in d['updated'])")"
check "A 侧内容同步" "hero-v2-from-B" "$(cat "$WORK/wc-a/characters/hero.psd")"
R=$(cli status --dir wc-a --json)
check "更新后工作副本干净" "True" "$(echo "$R" | jq_get 'all(i["status"] in ("normal","ignored") for i in d["items"])')"

# ---- 8. 冲突 ----
echo "== 8. 冲突处理 =="
printf 'hero-A-local' > "$WORK/wc-a/characters/hero.psd"
printf 'hero-B-second' > "$WORK/wc-b/characters/hero.psd"
R=$(cli commit --dir wc-b -m "B: hero v3" --json)
check "B 侧抢先提交 → rev 3" "3" "$(echo "$R" | jq_get 'd["rev"]')"

R=$(cli update --dir wc-a --json)
check "A 侧 update 报告冲突" "True" "$(echo "$R" | jq_get "any('characters/hero.psd' in p for p in d['conflicts'])")"
check "工作文件取服务端版本" "hero-B-second" "$(cat "$WORK/wc-a/characters/hero.psd")"
check "本地改动另存 .mine" "hero-A-local" "$(cat "$WORK/wc-a/characters/hero.psd.mine")"
R=$(cli status --dir wc-a --json)
check "status 标记 conflicted" "True" "$(echo "$R" | jq_get 'any(i["path"]=="characters/hero.psd" and i["status"]=="conflicted" for i in d["items"])')"
R=$(cli commit --dir wc-a -m "A: 不该成功" --json)
case "$R" in *CONFLICT*) ok "带冲突提交被拒绝";; *) bad "冲突提交应报 CONFLICT，实际：$R";; esac

rm -f "$WORK/wc-a/characters/hero.psd.mine"
R=$(cli revert --dir wc-a characters/hero.psd --json)
check "revert 冲突文件" "True" "$(echo "$R" | jq_get 'd["ok"]')"
check "revert 回到基线内容" "hero-B-second" "$(cat "$WORK/wc-a/characters/hero.psd")"
R=$(cli status --dir wc-a --json)
check "解决后恢复干净" "True" "$(echo "$R" | jq_get 'all(i["status"] in ("normal","ignored") for i in d["items"])')"

# ---- 9. 删除与传播 ----
echo "== 9. 删除同步 =="
R=$(cli remove --dir wc-b characters/normal.png --json)
check "remove → ok" "True" "$(echo "$R" | jq_get 'd["ok"]')"
[ -f "$WORK/wc-b/characters/normal.png" ] && bad "remove 后文件仍在" || ok "remove 后文件已删"
R=$(cli commit --dir wc-b -m "B: drop normal.png" --json)
check "删除提交 → rev 4" "4" "$(echo "$R" | jq_get 'd["rev"]')"
R=$(cli update --dir wc-a --json)
check "A 侧删除已传播" "True" "$(echo "$R" | jq_get "any('characters/normal.png' in p for p in d['deleted'])")"
[ -f "$WORK/wc-a/characters/normal.png" ] && bad "A 侧文件未删" || ok "A 侧文件已删"

# ---- 10. 锁 ----
echo "== 10. 锁 =="
R=$(cli lock --dir wc-a characters/hero.psd -m "改贴图" --json)
check "lock → ok" "True" "$(echo "$R" | jq_get 'd["ok"]')"
check "lock 返回 token" "True" "$(echo "$R" | jq_get 'len(d["token"]) > 0')"
R=$(cli locks --dir wc-a --json)
check "locks 列表含该锁" "True" "$(echo "$R" | jq_get 'any(i["path"]=="characters/hero.psd" for i in d["items"])')"
check "锁备注落库" "改贴图" "$(echo "$R" | jq_get '[i["comment"] for i in d["items"] if i["path"]=="characters/hero.psd"][0]')"
R=$(cli unlock --dir wc-a characters/hero.psd --json)
check "unlock → ok" "True" "$(echo "$R" | jq_get 'd["ok"]')"
R=$(cli locks --dir wc-a --json)
check "解锁后列表为空" "0" "$(echo "$R" | jq_get 'len(d["items"])')"

# 第二个用户：抢锁应被拒，强制解锁需要 --break
TOKEN=$(token)
AUTH="Authorization: Bearer $TOKEN"
R=$(curl -s -w "\n%{http_code}" -X POST $B/admin/users -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"username\":\"$BOB\",\"password\":\"$PASS\"}")
check "建第二用户 → 201" "201" "$(echo "$R" | tail -1)"
BOB_ID=$(echo "$R" | head -1 | jq_get 'd["id"]')
R=$(curl -s -w "\n%{http_code}" -X POST $B/admin/repos/$REPO/acl -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"path_prefix\":\"\",\"subject_type\":\"user\",\"subject_id\":$BOB_ID,\"level\":\"write\",\"inherit\":true}")
check "为第二用户授予写权限 → 200" "200" "$(echo "$R" | tail -1)"

cli lock --dir wc-a characters/hero.psd -m "A 占用" --json > /dev/null
cli login --server "$BASE" --username "$BOB" --password "$PASS" --json > /dev/null
R=$(cli lock --dir wc-a characters/hero.psd --json)
case "$R" in *LOCKED*) ok "他人持锁 → LOCKED";; *) bad "他人持锁应报 LOCKED，实际：$R";; esac
R=$(cli unlock --dir wc-a characters/hero.psd --json)
case "$R" in *PERMISSION*) ok "解他人锁被拒（锁不属于自己）";; *) bad "解他人锁应报 PERMISSION_DENIED，实际：$R";; esac
R=$(cli unlock --dir wc-a characters/hero.psd --break --reason "人已离职" --json)
case "$R" in *PERMISSION*) ok "无 admin 权限不能强制解锁";; *) bad "无 admin 权限强制解锁应被拒，实际：$R";; esac
# 切回管理员（仓库创建者，对该目录有 admin）才能真正强制解锁
cli login --server "$BASE" --username "$ADMIN" --password "$PASS" --json > /dev/null
R=$(cli unlock --dir wc-a characters/hero.psd --break --reason "人已离职" --json)
check "管理员 --break 可强制解锁" "True" "$(echo "$R" | jq_get 'd["ok"]')"
R=$(cli locks --dir wc-a --json)
check "强制解锁后无锁" "0" "$(echo "$R" | jq_get 'len(d["items"])')"

# ---- 11. 边界 ----
echo "== 11. 边界与错误处理 =="
mkdir -p "$WORK/outside"
R=$(cd "$WORK/outside" && "$NODE" "$ROOT/client/node_modules/.bin/tsx" "$ROOT/client/src/cli.ts" status --json 2>/dev/null)
case "$R" in *"不在工作副本"*) ok "工作副本外执行 status 报错";; *) bad "工作副本外应报错，实际：$R";; esac
R=$(cli lock --dir wc-a ../escape.psd --json)
# 注意：bash 3.2（macOS 自带）不把多字节字符当变量名终止符，`$R）` 会被当成变量名的一部分，
# 所以这里必须写成 ${R}。
case "$R" in *LOCKED*|*PERMISSION*|*INVALID*|*NOT_FOUND*) ok "非法路径加锁被拒（${R}）";; *) bad "非法路径加锁应被拒，实际：${R}";; esac

# 全局缓存与 pristine 不变量
CACHE="$AH/cache"
[ -d "$CACHE" ] && ok "全局缓存目录已建立" || bad "缺少全局缓存"
[ -d "$WORK/wc-a/.b-artifact/pristine" ] && ok "pristine 目录已建立" || bad "缺少 pristine"
if [ -d "$CACHE" ]; then
  N=$(find "$CACHE" -type f ! -name '*.tmp*' | head -20 | wc -l | tr -d ' ')
  [ "$N" -gt 0 ] && ok "缓存中有 blob（$N+）" || bad "缓存为空"
fi

echo
echo "---- 结果：$PASS_N 通过 / $FAIL_N 失败 ----"
[ "$FAIL_N" -eq 0 ] || { echo "服务端日志尾部："; tail -20 "$WORK/server.log"; exit 1; }
