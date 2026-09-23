#!/bin/bash
# b-artifact M3 管理端冒烟（curl 版，§12.8 交付门禁）
#
# 覆盖两块：
#   1) §8.1 静态托管：`/admin` 目录服务、SPA 回退、缓存头、路径穿越防护
#   2) §7.2/§8.3 管理 API：统计、审计、用户、组、仓库设置、ACL（含有效权限
#      预览器与反查）、purge 全链路、维护（rebuild-refcount / GC）
#
# 前置：服务端必须以 `--admin-dir admin/dist` 启动（且已 `npm run build`），否则
#       第 1 组断言会全部失败并给出提示。
#
# 用法：
#   server --data-dir /tmp/m3 --create-admin smokeadmin
#   server --data-dir /tmp/m3 --listen 127.0.0.1:18321 --admin-dir admin/dist
#   BASE_URL=http://127.0.0.1:18321 SMOKE_USER=smokeadmin SMOKE_PASS=xxx scripts/m3_smoke.sh
#
# 注意（踩过的坑）：**带 JSON body 的 curl 必须写在顶层赋值里**。
#   若写成 `check "a" "b" "$(curl ... -d "{\"k\":\"v\",\"k2\":\"v2\"}")"`，
#   bash 在嵌套 `$( )` 内会二次处理反斜杠转义，`{...}` 退化成花括号展开，
#   body 被拆成多个参数（实测 `-d '"k":"v"' -d '"k2":"v2"'`）→ 服务端 422。
#   统一写法：`R=$(curl -s -w "\n%{http_code}" ... -d "{...}")` 再用
#   `$(echo "$R" | tail -1)` 取状态码、`$(echo "$R" | head -1 | jq_get '...')` 取字段。

set -u

BASE="${BASE_URL:-http://127.0.0.1:18321}"
USER_NAME="${SMOKE_USER:-smokeadmin}"
PASS="${SMOKE_PASS:-smoke-PW-123456}"
B="$BASE/api/v1"
PASS_N=0; FAIL_N=0

ok()   { PASS_N=$((PASS_N+1)); echo "  ✓ $1"; }
bad()  { FAIL_N=$((FAIL_N+1)); echo "  ✗ $1"; }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (期望 [$2] 实际 [$3])"; fi; }
# 传参必须用单引号，例如 jq_get 'd["token"]'
jq_get() { python3 -c "import json,sys;d=json.load(sys.stdin);print($1)"; }

WORK=$(mktemp -d /tmp/b-artifact-m3.XXXXXX)
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

echo "== 0. 健康检查与登录 =="
check "GET /health" "200" "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/health")"
LOGIN=$(curl -s -X POST $B/auth/login -H 'Content-Type: application/json' \
  -d "{\"username\":\"$USER_NAME\",\"password\":\"$PASS\"}")
TOKEN=$(echo "$LOGIN" | jq_get 'd["token"]')
[ -n "$TOKEN" ] && ok "登录获取 token" || { bad "登录失败: $LOGIN"; echo; exit 1; }
AUTH="Authorization: Bearer $TOKEN"
ME=$(echo "$LOGIN" | jq_get 'd["user"]["id"]')

echo "== 1. 静态托管（§8.1）=="
CODE=$(curl -s -o index.html -w '%{http_code}' "$BASE/admin/")
check "GET /admin/ → 200" "200" "$CODE"
if [ "$CODE" = "200" ]; then
  grep -q '<div id="app">' index.html && ok "返回 SPA index.html" || bad "index.html 内容异常"
  grep -q 'src="/admin/assets/' index.html && ok "资源路径带 /admin/ 前缀" || bad "资源前缀错误（base 配置不对）"
  check "index.html 不缓存" "no-cache" "$(curl -s -o /dev/null -D - "$BASE/admin/" | tr -d '\r' | awk -F': ' '/[Cc]ache-[Cc]ontrol/{print $2}')"

  # 取一个真实资源文件名验证 MIME / 缓存头
  ASSET=$(grep -o '/admin/assets/[^"]*\.js' index.html | head -1)
  if [ -n "$ASSET" ]; then
    check "GET $ASSET → 200" "200" "$(curl -s -o /dev/null -w '%{http_code}' "$BASE$ASSET")"
    CT=$(curl -s -o /dev/null -D - "$BASE$ASSET" | tr -d '\r' | awk -F': ' '/[Cc]ontent-[Tt]ype/{print $2}')
    case "$CT" in *javascript*) ok "JS 资源 MIME 正确";; *) bad "JS 资源 MIME: $CT";; esac
    CC=$(curl -s -o /dev/null -D - "$BASE$ASSET" | tr -d '\r' | awk -F': ' '/[Cc]ache-[Cc]ontrol/{print $2}')
    case "$CC" in *immutable*) ok "带 hash 资源长缓存";; *) bad "资源缓存头: $CC";; esac
  else
    bad "未能从 index.html 提取资源路径"
  fi
else
  bad "管理端未构建或 --admin-dir 未指向 admin/dist"
fi

check "GET /admin（无尾斜杠）→ 200" "200" "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/admin")"
CODE=$(curl -s -o deep.html -w '%{http_code}' "$BASE/admin/repos/demo/acl")
check "SPA 深链 /admin/repos/demo/acl → 200" "200" "$CODE"
grep -q '<div id="app">' deep.html && ok "深链回退到 index.html" || bad "深链未回退"
check "不存在的资源 → 404" "404" "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/admin/assets/nope.js")"
check "路径穿越 → 400" "400" "$(curl -s -o /dev/null -w '%{http_code}' --path-as-is "$BASE/admin/../../etc/passwd")"
check "GET / → 管理端重定向" "307" "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/")"

echo "== 2. 统计与审计 =="
R=$(curl -s $B/admin/stats -H "$AUTH")
[ "$(echo "$R" | jq_get 'd["repos"]')" -ge 0 ] 2>/dev/null && ok "GET /admin/stats" || bad "stats: $R"
echo "$R" | jq_get 'd["storage"]["dedup_ratio"]' >/dev/null 2>&1 && ok "含存储收益字段" || bad "storage 字段缺失"
check "commit_trend 是数组" "True" "$(echo "$R" | jq_get 'isinstance(d["commit_trend"], list)')"

R=$(curl -s "$B/admin/audit?limit=5" -H "$AUTH")
check "GET /admin/audit" "True" "$(echo "$R" | jq_get 'isinstance(d["items"], list)')"
check "审计含 purge 之外的登录动作" "True" "$(echo "$R" | jq_get 'any("login" in i["action"] for i in d["items"])')"
curl -s -o audit.csv -w '%{http_code}' "$B/admin/audit?limit=1000&format=csv" -H "$AUTH" > csv_code.txt
check "审计导出 CSV → 200" "200" "$(cat csv_code.txt)"
head -c 3 audit.csv | grep -q $'\xef\xbb\xbf' && ok "CSV 带 UTF-8 BOM" || bad "CSV 缺 BOM"

echo "== 3. 用户与组 =="
U="smokeu-$(date +%s)"
R=$(curl -s -w "\n%{http_code}" -X POST $B/admin/users -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"username\":\"$U\",\"password\":\"smoke-PW-123456\",\"display_name\":\"冒烟用户\"}")
check "POST /admin/users → 201" "201" "$(echo "$R" | tail -1)"
UID_=$(echo "$R" | head -1 | jq_get 'd["id"]')
R=$(curl -s -w "\n%{http_code}" -X POST $B/admin/users -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"username\":\"$U\",\"password\":\"smoke-PW-123456\"}")
check "重名建用户 → 409" "409" "$(echo "$R" | tail -1)"
R=$(curl -s -w "\n%{http_code}" -X POST $B/admin/users -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"username":"x","password":"123"}')
check "弱密码 → 400" "400" "$(echo "$R" | tail -1)"
R=$(curl -s -X PUT $B/admin/users/$UID_ -H "$AUTH" -H 'Content-Type: application/json' -d '{"display_name":"改名后"}')
check "PUT 改显示名" "True" "$(echo "$R" | jq_get 'd["updated"]')"
check "POST 重置密码 → 204" "204" "$(curl -s -o /dev/null -w '%{http_code}' -X POST $B/admin/users/$UID_/password -H "$AUTH" -H 'Content-Type: application/json' -d '{"new_password":"smoke-PW-654321"}')"
check "禁用自己 → 400" "400" "$(curl -s -o /dev/null -w '%{http_code}' -X PUT $B/admin/users/$ME -H "$AUTH" -H 'Content-Type: application/json' -d '{"disabled":true}')"

G="smokeg-$(date +%s)"
R=$(curl -s -w "\n%{http_code}" -X POST $B/admin/groups -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"name\":\"$G\",\"comment\":\"冒烟组\"}")
check "POST /admin/groups → 201" "201" "$(echo "$R" | tail -1)"
GID=$(echo "$R" | head -1 | jq_get 'd["id"]')
R=$(curl -s -X PUT $B/admin/groups/$GID/members -H "$AUTH" -H 'Content-Type: application/json' -d "{\"user_ids\":[$UID_]}")
check "PUT 组成员" "1" "$(echo "$R" | jq_get 'd["members"]')"
check "GET 组成员" "$U" "$(curl -s $B/admin/groups/$GID/members -H "$AUTH" | jq_get 'd["items"][0]["username"]')"

echo "== 4. 仓库 / ACL / 预览器 =="
REPO="m3-$(date +%s)"
R=$(curl -s -w "\n%{http_code}" -X POST $B/repos -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"name\":\"$REPO\"}")
check "建仓 → 201" "201" "$(echo "$R" | tail -1)"
R=$(curl -s -X PUT $B/admin/repos/$REPO/settings -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"description":"M3 冒烟"}')
check "PUT 仓库设置" "True" "$(echo "$R" | jq_get 'd["updated"]')"
check "设置生效 description" "M3 冒烟" "$(curl -s $B/repos/$REPO/info -H "$AUTH" | jq_get 'd["description"]')"
# v0.4.17：废弃字段被忽略而不是 400（老脚本不必同批升级）
check "废弃 lock_policy 被忽略" "True" "$(curl -s -X PUT $B/admin/repos/$REPO/settings -H "$AUTH" -H 'Content-Type: application/json' -d '{"lock_policy":"loose"}' | jq_get 'd["updated"]')"
check "info 不再下发 lock_policy" "True" "$(curl -s $B/repos/$REPO/info -H "$AUTH" | jq_get '"lock_policy" not in d')"

# 默认规则 + 新增一条 group read 与一条屏障规则
R=$(curl -s -w "\n%{http_code}" -X POST $B/admin/repos/$REPO/acl -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"path_prefix\":\"secret\",\"subject_type\":\"group\",\"subject_id\":$GID,\"level\":\"read\",\"inherit\":true}")
check "POST acl（组 read）→ 200" "200" "$(echo "$R" | tail -1)"
RULE2=$(echo "$R" | head -1 | jq_get 'd["id"]')
R=$(curl -s -w "\n%{http_code}" -X POST $B/admin/repos/$REPO/acl -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"path_prefix":"secret","subject_type":"everyone","level":"none","inherit":false}')
check "POST acl（屏障）→ 200" "200" "$(echo "$R" | tail -1)"
BARRIER=$(echo "$R" | head -1 | jq_get 'd["id"]')

R=$(curl -s "$B/admin/repos/$REPO/acl" -H "$AUTH")
check "ACL 列表含 subject_label" "True" "$(echo "$R" | jq_get 'all("subject_label" in i for i in d["items"])')"
check "ACL 列表含 shadowed 标记" "True" "$(echo "$R" | jq_get 'any("shadowed" in i for i in d["items"])')"

# 预览器：目标用户是组成员 → 在 secret 命中组规则
R=$(curl -s "$B/admin/repos/$REPO/acl/preview?user_id=$UID_&path=secret/a.bin" -H "$AUTH")
check "预览器：组成员获得 read" "read" "$(echo "$R" | jq_get 'd["level"]')"
check "预览器：给出回溯步骤" "True" "$(echo "$R" | jq_get 'len(d["steps"]) > 0')"
check "预览器：步骤含命中层" "hit" "$(echo "$R" | jq_get '[s["outcome"] for s in d["steps"] if s["outcome"]=="hit"][0]')"

# 系统管理员穿透一切（§4.2）
R=$(curl -s "$B/admin/repos/$REPO/acl/preview?user_id=$ME&path=secret/a.bin" -H "$AUTH")
check "管理员穿透 → admin" "admin" "$(echo "$R" | jq_get 'd["level"]')"

R=$(curl -s "$B/admin/repos/$REPO/acl/who?path=secret&level=read" -H "$AUTH")
check "反查：组成员在列表里" "True" "$(echo "$R" | jq_get 'any(i["id"]=='"$UID_"' for i in d["items"])')"
check "反查：给出归属说明" "True" "$(echo "$R" | jq_get 'any("组" in i["via"] for i in d["items"])')"

check "目录树接口" "True" "$(curl -s "$B/admin/repos/$REPO/dirs" -H "$AUTH" | jq_get 'isinstance(d["items"], list)')"

echo "== 5. 提交一版数据用于 purge =="
# §3.6 前置：purge 要求「系统管理员 + 目录 admin」两者并列。上面刚给 secret 加了
# 继承屏障（everyone=none），当前管理员在 secret 上只有 none → 必须显式补 admin 规则。
R=$(curl -s -w "\n%{http_code}" -X POST $B/admin/repos/$REPO/acl -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"path_prefix\":\"secret\",\"subject_type\":\"user\",\"subject_id\":$ME,\"level\":\"admin\",\"inherit\":true}")
check "补管理员 admin 规则（purge 前置）→ 200" "200" "$(echo "$R" | tail -1)"
python3 -c "
import hashlib, time
data = (b'purge me ' * 500) + str(time.time_ns()).encode()
open('p.bin','wb').write(data)
print(hashlib.sha256(data).hexdigest())" > h.txt
H=$(cat h.txt)
SZ=$(stat -f%z p.bin)
check "PUT blob → 200" "200" "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "$B/repos/$REPO/blobs/$H" -H "$AUTH" -H 'Content-Type: application/octet-stream' --data-binary @p.bin)"
# 先锁后提交（§5.2 v0.4.17）
check "加锁 secret/p.bin" "True" "$(curl -s -X POST $B/repos/$REPO/locks -H "$AUTH" -H 'Content-Type: application/json' -d '{"path":"secret/p.bin"}' | jq_get 'd["token"] is not None')"
CID="$(uuidgen | tr 'A-Z' 'a-z')"
PREP=$(curl -s -X POST $B/repos/$REPO/commit/prepare -H "$AUTH" -H 'Content-Type: application/json' -d "{
  \"commit_id\":\"$CID\", \"base_rev\":0, \"message\":\"待清除数据\",
  \"changes\":[{\"path\":\"secret/p.bin\",\"op\":\"add\",\"kind\":\"file\",\"blob_hash\":\"$H\",\"size\":$SZ}]
}")
check "prepare need_blobs 为空" "0" "$(echo "$PREP" | jq_get 'len(d["need_blobs"])')"
CT=$(echo "$PREP" | jq_get 'd["commit_token"]')
R=$(curl -s -X POST $B/repos/$REPO/commit -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"commit_id\":\"$CID\",\"commit_token\":\"$CT\"}")
check "commit → rev 1" "1" "$(echo "$R" | jq_get 'd["rev"]')"
check "tree 可见 secret/p.bin" "secret/p.bin" "$(curl -s "$B/repos/$REPO/tree?prefix=secret" -H "$AUTH" | jq_get 'd["items"][0]["path"]')"

echo "== 6. purge（§3.6）=="
R=$(curl -s -w "\n%{http_code}" -X POST $B/admin/repos/$REPO/purge -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"prefix":"secret","reason":"测试","confirm_name":"wrong"}')
check "confirm_name 不符 → 400" "400" "$(echo "$R" | tail -1)"
R=$(curl -s -w "\n%{http_code}" -X POST $B/admin/repos/$REPO/purge -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"prefix\":\"secret\",\"reason\":\"短\",\"confirm_name\":\"$REPO\"}")
check "原因过短 → 400" "400" "$(echo "$R" | tail -1)"
R=$(curl -s -X POST $B/admin/repos/$REPO/purge -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"prefix\":\"secret\",\"reason\":\"冒烟清除敏感数据\",\"confirm_name\":\"$REPO\"}")
check "purge → 删除路径 ≥1" "True" "$(echo "$R" | jq_get 'd["paths_removed"] >= 1')"
check "purge → 影响 r1" "1" "$(echo "$R" | jq_get 'd["revisions_affected"]')"
echo "$R" | jq_get 'd["gc_scheduled_at"]' >/dev/null 2>&1 && ok "purge 返回 GC 计划时间" || bad "gc_scheduled_at 缺失"
check "purge 后 tree 为空" "0" "$(curl -s "$B/repos/$REPO/tree?prefix=secret" -H "$AUTH" | jq_get 'len(d["items"])')"
check "purge 后 log 仍保留修订（只抹路径）" "True" "$(curl -s "$B/repos/$REPO/log" -H "$AUTH" | jq_get 'len(d["items"]) >= 1')"
check "purge 进审计" "True" "$(curl -s "$B/admin/audit?action=repo.purge&limit=5" -H "$AUTH" | jq_get 'len(d["items"]) >= 1')"

echo "== 7. 维护（§3.7）=="
R=$(curl -s $B/admin/maintenance -H "$AUTH")
check "GET /admin/maintenance" "True" "$(echo "$R" | jq_get '"queued" in d')"
check "宽限期 24h" "86400" "$(echo "$R" | jq_get 'd["grace_secs"]')"
R=$(curl -s -X POST $B/admin/maintenance/rebuild-refcount -H "$AUTH")
check "rebuild-refcount" "True" "$(echo "$R" | jq_get 'd["blobs"] >= 1')"
R=$(curl -s -X POST $B/admin/maintenance/gc -H "$AUTH" -H 'Content-Type: application/json' -d '{"limit":100}')
# §3.7 关键语义 3：purge 引发的队列项 grace=0，不守 24h 撤销期 → 这里应真的删掉 blob
check "run gc（purge 项立即可回收）" "True" "$(echo "$R" | jq_get 'd["deleted"] >= 1')"
check "gc 复查 pending_commits 字段" "True" "$(echo "$R" | jq_get '"skipped_pending" in d')"

echo "== 8. 清理 =="
# 顺序有讲究：删仓会连带清掉该仓的 acl_rules，组才不再被引用（否则 409 GROUP_IN_USE）
check "删除测试仓库" "True" "$(curl -s -X DELETE $B/admin/repos/$REPO -H "$AUTH" | jq_get 'd["deleted"]')"
check "删除测试组" "True" "$(curl -s -X DELETE $B/admin/groups/$GID -H "$AUTH" | jq_get 'd["deleted"]')"
check "删除测试用户" "True" "$(curl -s -X DELETE $B/admin/users/$UID_ -H "$AUTH" | jq_get 'd["deleted"]')"

echo
# 注意：$PASS_N 后紧跟全角逗号，必须加花括号——UTF-8 locale 下全角字符会被
# bash 当作标识符的一部分，写成 $PASS_N， 会解析成 ${PASS_N，} → unbound variable
echo "M3 冒烟: 通过 ${PASS_N}，失败 ${FAIL_N}"
[ "$FAIL_N" -eq 0 ] || exit 1
