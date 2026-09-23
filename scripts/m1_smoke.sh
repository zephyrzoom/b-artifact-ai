#!/bin/bash
# b-artifact M1 + M2 全链路 HTTP 冒烟（curl 版，§12.8 交付门禁）
#
# 覆盖：M1 认证 / 建仓 / 传输 / 两阶段提交 / 历史，M2 ACL 规则 CRUD / 锁（加锁、幂等、
#       覆盖查询、续期、强制解锁留痕、目录不可加锁、锁只作用于精确路径）。
# 只用系统管理员一个账号跑，因此**权限拒绝类**断言（403）放在 tests/api_m2.rs 里用
# 多用户场景覆盖，这里只验正向链路与参数校验。
#
# 用法：
#   1) 准备数据目录并建管理员：
#      server --data-dir /tmp/m1 --create-admin smokeadmin   # 密码自设（≥8 位，需含数字/大写/小写/符号）
#   2) 启动服务端：
#      server --data-dir /tmp/m1 --listen 127.0.0.1:18321
#   3) 运行本脚本（可用环境变量覆盖默认值）：
#      BASE_URL=http://127.0.0.1:18321 SMOKE_USER=smokeadmin SMOKE_PASS=xxx scripts/m1_smoke.sh
#
# 说明：仓库每轮以时间戳命名（可重复执行）；任一断言失败即非零退出。

set -u

BASE="${BASE_URL:-http://127.0.0.1:18321}"
USER_NAME="${SMOKE_USER:-smokeadmin}"
PASS="${SMOKE_PASS:-smoke-PW-123456}"
B="$BASE/api/v1"
PASS_N=0; FAIL_N=0

ok()   { PASS_N=$((PASS_N+1)); echo "  ✓ $1"; }
bad()  { FAIL_N=$((FAIL_N+1)); echo "  ✗ $1"; }
check() { # $1=描述 $2=期望 $3=实际
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (期望 [$2] 实际 [$3])"; fi
}
jq_get() { python3 -c "import json,sys;print(json.load(sys.stdin)$1)"; }

WORK=$(mktemp -d /tmp/b-artifact-smoke.XXXXXX)
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"

# 0. 健康检查
R=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/health")
check "GET /health" "200" "$R"

# 1. 登录
LOGIN=$(curl -s -X POST $B/auth/login -H 'Content-Type: application/json' \
  -d "{\"username\":\"$USER_NAME\",\"password\":\"$PASS\"}")
TOKEN=$(echo "$LOGIN" | jq_get '["token"]')
[ -n "$TOKEN" ] && ok "登录获取 token" || bad "登录失败: $LOGIN"
AUTH="Authorization: Bearer $TOKEN"

# 2. 建仓（时间戳命名，可重复执行）
REPO="smoke-$(date +%s)"
R=$(curl -s -w "\n%{http_code}" -X POST $B/repos -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"name\":\"$REPO\",\"description\":\"冒烟仓库\"}")
check "POST /repos 建仓" "201" "$(echo "$R" | tail -1)"

R=$(curl -s -o /dev/null -w "%{http_code}" -X POST $B/repos -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"name\":\"$REPO\"}")
check "重复建仓 → 409" "409" "$R"

# 3. 生成可压缩测试文件并查 missing（内容每轮唯一：blob 全局去重会让固定内容第二轮起"已存在"）
python3 -c "
import hashlib, time
data = (b'hello b-artifact ' * 1000) + str(time.time_ns()).encode()
open('a.bin','wb').write(data)
print(hashlib.sha256(data).hexdigest())" > hash_a.txt
H_A=$(cat hash_a.txt)

R=$(curl -s -X POST $B/repos/$REPO/blobs/missing -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"hashes\":[\"$H_A\"]}")
check "missing 应报缺失" "1" "$(echo "$R" | jq_get '["missing"].__len__()')"

# 4. 整块上传
R=$(curl -s -w "\n%{http_code}" -X PUT "$B/repos/$REPO/blobs/$H_A" -H "$AUTH" \
  -H 'Content-Type: application/octet-stream' -H 'Expect:' --data-binary @a.bin)
check "PUT 整块上传" "200" "$(echo "$R" | tail -1)"

R=$(curl -s -X POST $B/repos/$REPO/blobs/missing -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"hashes\":[\"$H_A\"]}")
check "missing 应为空" "0" "$(echo "$R" | jq_get '["missing"].__len__()')"

# 错误 hash 校验（64 个 0 是合法 hex，但与内容不符 → 412）
BADHASH=$(printf '0%.0s' $(seq 1 64))
R=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$B/repos/$REPO/blobs/$BADHASH" -H "$AUTH" \
  -H 'Content-Type: application/octet-stream' --data-binary @a.bin)
check "sha256 不符 → 412" "412" "$R"

# 4b. 未持锁提交 → 412 NEEDS_LOCK（v0.4.17 的唯一策略，§5.2）
CIDN="$(uuidgen | tr 'A-Z' 'a-z')"
R=$(curl -s -X POST $B/repos/$REPO/commit/prepare -H "$AUTH" -H 'Content-Type: application/json' -d "{
  \"commit_id\":\"$CIDN\", \"base_rev\":0, \"message\":\"未持锁\",
  \"changes\":[{\"path\":\"dir1/a.bin\",\"op\":\"add\",\"kind\":\"file\",\"blob_hash\":\"$H_A\",\"size\":$(stat -f%z a.bin)}]
}")
check "未持锁提交 → NEEDS_LOCK" "NEEDS_LOCK" "$(echo "$R" | jq_get '["error"]["code"]')"

# 5. 先加锁再 prepare（§5.2 v0.4.17：每个变更路径都必须由本人持锁）
curl -s -o /dev/null -X POST $B/repos/$REPO/locks -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"path":"dir1/a.bin","comment":"smoke"}'

# 5b. prepare（blob 已就位 → need_blobs 空）
CID="$(uuidgen | tr 'A-Z' 'a-z')"
PREP=$(curl -s -X POST $B/repos/$REPO/commit/prepare -H "$AUTH" -H 'Content-Type: application/json' -d "{
  \"commit_id\":\"$CID\", \"base_rev\":0, \"message\":\"首次提交\",
  \"changes\":[{\"path\":\"dir1/a.bin\",\"op\":\"add\",\"kind\":\"file\",\"blob_hash\":\"$H_A\",\"size\":$(stat -f%z a.bin)}]
}")
CT=$(echo "$PREP" | jq_get '["commit_token"]')
check "prepare need_blobs 为空" "0" "$(echo "$PREP" | jq_get '["need_blobs"].__len__()')"

# 6. commit → rev 1
R=$(curl -s -X POST $B/repos/$REPO/commit -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"commit_id\":\"$CID\",\"commit_token\":\"$CT\"}")
check "commit → rev 1" "1" "$(echo "$R" | jq_get '["rev"]')"

# 7. 幂等重放
R=$(curl -s -X POST $B/repos/$REPO/commit -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"commit_id\":\"$CID\",\"commit_token\":\"$CT\"}")
check "幂等重放 → rev 1 / replayed" "1 True" "$(echo "$R" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d["rev"], d["replayed"])')"

# 8. 旧 base_rev → OUT_OF_DATE
CID2="$(uuidgen | tr 'A-Z' 'a-z')"
PREP2=$(curl -s -X POST $B/repos/$REPO/commit/prepare -H "$AUTH" -H 'Content-Type: application/json' -d "{
  \"commit_id\":\"$CID2\", \"base_rev\":0, \"message\":\"stale\",
  \"changes\":[{\"path\":\"x.bin\",\"op\":\"add\",\"kind\":\"file\",\"blob_hash\":\"$H_A\",\"size\":1}]
}")
check "过期 base_rev → OUT_OF_DATE" "OUT_OF_DATE" "$(echo "$PREP2" | jq_get '["error"]["code"]')"

# 9. tree / log / changes / info
R=$(curl -s "$B/repos/$REPO/tree?prefix=dir1" -H "$AUTH")
check "tree prefix=dir1 含 a.bin" "dir1/a.bin" "$(echo "$R" | jq_get '["items"][0]["path"]')"

R=$(curl -s "$B/repos/$REPO/log" -H "$AUTH")
check "log rev1" "1" "$(echo "$R" | jq_get '["items"][0]["rev"]')"

R=$(curl -s "$B/repos/$REPO/changes?from=1&to=1" -H "$AUTH")
check "changes JSONL" "add dir1/a.bin" "$(echo "$R" | head -1 | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d["op"],d["path"])')"

R=$(curl -s "$B/repos/$REPO/info" -H "$AUTH")
check "info head_rev=1" "1" "$(echo "$R" | jq_get '["head_rev"]')"

# 10. 下载 + Range + HEAD
curl -s "$B/repos/$REPO/blobs/$H_A" -H "$AUTH" -o dl.bin
check "下载内容一致" "$H_A" "$(python3 -c 'import hashlib;print(hashlib.sha256(open("dl.bin","rb").read()).hexdigest())')"

# a.bin 为可压缩内容 → zstd 存储 → 按设计忽略 Range 返回全量 200
R=$(curl -s -o range.bin -w "%{http_code}" -H "Range: bytes=0-15" "$B/repos/$REPO/blobs/$H_A" -H "$AUTH")
check "zstd blob Range 忽略 → 200" "200" "$R"
check "zstd blob 返回全量" "$H_A" "$(python3 -c 'import hashlib;print(hashlib.sha256(open("range.bin","rb").read()).hexdigest())')"

R=$(curl -s -I -o /dev/null -w "%{http_code}:%{size_download}" -X HEAD "$B/repos/$REPO/blobs/$H_A" -H "$AUTH")
check "HEAD → 200" "200:0" "$R"

# 11. 分块上传（200KB 随机数据，64KB 一块 → 4 块，最后一块 8KB）
python3 -c "
import hashlib, os
data = os.urandom(200*1024)
open('big.bin','wb').write(data)
print(hashlib.sha256(data).hexdigest())" > hash_big.txt
H_BIG=$(cat hash_big.txt)
SIZE_BIG=$(stat -f%z big.bin)

UP=$(curl -s -X POST $B/repos/$REPO/blobs/uploads -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"hash\":\"$H_BIG\",\"size\":$SIZE_BIG,\"chunk_size\":65536}")
UPID=$(echo "$UP" | jq_get '["upload_id"]')
[ -n "$UPID" ] && ok "创建分块上传会话" || bad "创建上传会话失败: $UP"

python3 - <<EOF
data = open('big.bin','rb').read()
for n in range(4):
    open(f'chunk_{n}','wb').write(data[n*65536:(n+1)*65536])
EOF
# 乱序上传：2,0,3,1
for n in 2 0 3 1; do
  curl -s -o /dev/null -X PUT "$B/repos/$REPO/blobs/uploads/$UPID/$n" -H "$AUTH" \
    -H 'Content-Type: application/octet-stream' --data-binary @chunk_$n
done
R=$(curl -s "$B/repos/$REPO/blobs/uploads/$UPID" -H "$AUTH")
check "断点查询收到 4 块" "4" "$(echo "$R" | jq_get '["received_chunks"].__len__()')"

R=$(curl -s -X POST "$B/repos/$REPO/blobs/uploads/$UPID/complete" -H "$AUTH")
check "分块结算 hash 一致" "$H_BIG" "$(echo "$R" | jq_get '["hash"]')"

# 12. 第二次提交引用分块 blob → rev 2（同样先加锁）
curl -s -o /dev/null -X POST $B/repos/$REPO/locks -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"path":"dir1/big.bin","comment":"smoke"}'
CID3="$(uuidgen | tr 'A-Z' 'a-z')"
PREP3=$(curl -s -X POST $B/repos/$REPO/commit/prepare -H "$AUTH" -H 'Content-Type: application/json' -d "{
  \"commit_id\":\"$CID3\", \"base_rev\":1, \"message\":\"大文件分块\",
  \"changes\":[{\"path\":\"dir1/big.bin\",\"op\":\"add\",\"kind\":\"file\",\"blob_hash\":\"$H_BIG\",\"size\":$SIZE_BIG}]
}")
CT3=$(echo "$PREP3" | jq_get '["commit_token"]')
R=$(curl -s -X POST $B/repos/$REPO/commit -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"commit_id\":\"$CID3\",\"commit_token\":\"$CT3\"}")
check "第二次提交 → rev 2" "2" "$(echo "$R" | jq_get '["rev"]')"

# 13. 分块 blob 下载校验（随机数据 → raw 存储）+ Range
curl -s "$B/repos/$REPO/blobs/$H_BIG" -H "$AUTH" -o dl_big.bin
check "分块 blob 下载一致" "$H_BIG" "$(python3 -c 'import hashlib;print(hashlib.sha256(open("dl_big.bin","rb").read()).hexdigest())')"

R=$(curl -s -o range2.bin -w "%{http_code}" -H "Range: bytes=0-15" "$B/repos/$REPO/blobs/$H_BIG" -H "$AUTH")
check "raw blob Range → 206" "206" "$R"
check "Range 内容前 16 字节一致" "$(head -c 16 big.bin | xxd -p)" "$(head -c 16 range2.bin | xxd -p)"

# 14. 目录提交 + 删除（rev 3）：删 dir1/a.bin + 加目录
CID4="$(uuidgen | tr 'A-Z' 'a-z')"
PREP4=$(curl -s -X POST $B/repos/$REPO/commit/prepare -H "$AUTH" -H 'Content-Type: application/json' -d "{
  \"commit_id\":\"$CID4\", \"base_rev\":2, \"message\":\"删除与目录\",
  \"changes\":[{\"path\":\"dir1/a.bin\",\"op\":\"delete\",\"kind\":\"file\"},
                {\"path\":\"dir2\",\"op\":\"add\",\"kind\":\"dir\"}]
}")
CT4=$(echo "$PREP4" | jq_get '["commit_token"]')
R=$(curl -s -X POST $B/repos/$REPO/commit -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"commit_id\":\"$CID4\",\"commit_token\":\"$CT4\"}")
check "第三次提交 → rev 3" "3" "$(echo "$R" | jq_get '["rev"]')"

R=$(curl -s "$B/repos/$REPO/tree?prefix=dir1" -H "$AUTH")
N=$(echo "$R" | python3 -c 'import json,sys;items=json.load(sys.stdin)["items"];print([i["path"] for i in items])')
check "HEAD 不含已删文件" "['dir1/big.bin']" "$N"

R=$(curl -s "$B/repos/$REPO/tree?depth=2" -H "$AUTH")
N=$(echo "$R" | python3 -c 'import json,sys;items=json.load(sys.stdin)["items"];print([i["path"] for i in items])')
check "depth=2 递归树" "['dir1', 'dir1/big.bin', 'dir2']" "$N"

R=$(curl -s "$B/repos/$REPO/tree?rev=1&prefix=dir1" -H "$AUTH")
N=$(echo "$R" | python3 -c 'import json,sys;items=json.load(sys.stdin)["items"];print([i["path"] for i in items])')
check "rev1 历史树可查" "['dir1/a.bin']" "$N"

# 15. 空提交拒绝
CID5="$(uuidgen | tr 'A-Z' 'a-z')"
R=$(curl -s -X POST $B/repos/$REPO/commit/prepare -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"commit_id\":\"$CID5\",\"base_rev\":3,\"message\":\"empty\",\"changes\":[]}")
check "空提交 → 400" "INVALID_ARGUMENT" "$(echo "$R" | jq_get '["error"]["code"]')"

# 16. 非法路径
CID6="$(uuidgen | tr 'A-Z' 'a-z')"
R=$(curl -s -X POST $B/repos/$REPO/commit/prepare -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"commit_id\":\"$CID6\",\"base_rev\":3,\"message\":\"bad path\",\"changes\":[{\"path\":\"../escape.bin\",\"op\":\"add\",\"kind\":\"dir\"}]}")
check "路径穿越 → INVALID_PATH" "INVALID_PATH" "$(echo "$R" | jq_get '["error"]["code"]')"

# 17. 坏 token（新 commit_id + 不存在的 token → 409 COMMIT_TOKEN_EXPIRED）
CID7="$(uuidgen | tr 'A-Z' 'a-z')"
R=$(curl -s -o /dev/null -w "%{http_code}" -X POST $B/repos/$REPO/commit -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"commit_id\":\"$CID7\",\"commit_token\":\"deadbeef\"}")
check "坏 commit_token → 409" "409" "$R"

# ===== 18~24：M2 ACL（§4）=====
echo "--- M2: ACL ---"

# 18. 建规则：dir1 上 everyone=write
R=$(curl -s -X POST $B/admin/repos/$REPO/acl -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"path_prefix":"dir1","subject_type":"everyone","subject_id":0,"level":"write","inherit":true}')
ACL_ID=$(echo "$R" | jq_get '["id"]')
[ -n "$ACL_ID" ] && ok "POST /admin/.../acl 建规则 (id=$ACL_ID)" || bad "建 ACL 规则失败: $R"

# 19. 规则列表可见（建仓基线 + 本条 ≥ 2）
TOTAL=$(curl -s "$B/admin/repos/$REPO/acl" -H "$AUTH" | jq_get '["total"]')
[ "$TOTAL" -ge 2 ] 2>/dev/null && ok "GET /admin/.../acl 共 $TOTAL 条" || bad "ACL 列表异常: total=$TOTAL"

# 20. PUT 按 id 改级别
R=$(curl -s -X PUT $B/admin/repos/$REPO/acl -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"id\":$ACL_ID,\"level\":\"read\",\"inherit\":true}")
check "PUT 改规则级别 → read" "read" "$(echo "$R" | jq_get '["level"]')"

# 21. 删除规则
R=$(curl -s -X DELETE "$B/admin/repos/$REPO/acl?id=$ACL_ID" -H "$AUTH")
check "DELETE 规则" "True" "$(echo "$R" | jq_get '["deleted"]')"

# 22. 非法 level → 400
R=$(curl -s -o /dev/null -w "%{http_code}" -X POST $B/admin/repos/$REPO/acl -H "$AUTH" \
  -H 'Content-Type: application/json' -d '{"path_prefix":"dir1","level":"superuser"}')
check "非法 level → 400" "400" "$R"

# 23. 非法 subject_type → 400
R=$(curl -s -o /dev/null -w "%{http_code}" -X POST $B/admin/repos/$REPO/acl -H "$AUTH" \
  -H 'Content-Type: application/json' -d '{"path_prefix":"dir1","subject_type":"alien","subject_id":1}')
check "非法 subject_type → 400" "400" "$R"

# ===== 25~33：M2 锁（§5）=====
echo "--- M2: 锁 ---"

# 24. 加锁
LK=$(curl -s -X POST $B/repos/$REPO/locks -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"path":"dir1/big.bin","comment":"smoke"}')
LID=$(echo "$LK" | jq_get '["id"]')
[ -n "$LID" ] && ok "POST /locks 加锁 (id=$LID)" || bad "加锁失败: $LK"

# 25. 本人重复加锁幂等
R=$(curl -s -X POST $B/repos/$REPO/locks -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"path":"dir1/big.bin","comment":"again"}')
check "重复加锁幂等（同 id）" "$LID" "$(echo "$R" | jq_get '["id"]')"

# 26. 锁列表
R=$(curl -s "$B/repos/$REPO/locks" -H "$AUTH")
# 仓库里可能同时有多把锁（"先锁后提交"会留下先前提交时加的锁），所以查"含"而不是取第 0 条
HAS=$(echo "$R" | python3 -c 'import json,sys;print(any(i["path"]=="dir1/big.bin" for i in json.load(sys.stdin)["items"]))')
check "锁列表含该路径" "True" "$HAS"

# 27. 覆盖查询
R=$(curl -s "$B/repos/$REPO/locks?path=dir1/big.bin" -H "$AUTH")
check "?path= 覆盖查询命中" "dir1/big.bin" "$(echo "$R" | jq_get '["items"][0]["path"]')"

# 28. 心跳续期
N=$(curl -s -X POST $B/repos/$REPO/locks/refresh -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"ttl_secs":3600}' | jq_get '["refreshed"]')
[ "$N" -ge 1 ] 2>/dev/null && ok "refresh 续期 $N 把锁" || bad "refresh 失败: $N"

# 29. 强制解锁缺 reason → 400（§5.5 审计必填）
R=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$B/repos/$REPO/locks/dir1/big.bin?break=true" -H "$AUTH")
check "强制解锁缺 reason → 400" "400" "$R"

# 30. 强制解锁 → 留痕
R=$(curl -s -X DELETE "$B/repos/$REPO/locks/dir1/big.bin?break=true&reason=smoke" -H "$AUTH")
check "强制解锁 broken=1" "1" "$(echo "$R" | jq_get '["broken"]')"

R=$(curl -s "$B/repos/$REPO/locks?include_broken=true" -H "$AUTH" \
  | python3 -c 'import json,sys;print([i.get("break_reason") for i in json.load(sys.stdin)["items"] if i["path"]=="dir1/big.bin"][0])')
check "强制解锁留痕可查（include_broken）" "smoke" "$R"

# 31. 强制解锁后可重新加锁（旧行被惰性回收，UNIQUE 可重用）
R=$(curl -s -o /dev/null -w "%{http_code}" -X POST $B/repos/$REPO/locks -H "$AUTH" \
  -H 'Content-Type: application/json' -d '{"path":"dir1/big.bin"}')
check "强制解锁后可重新加锁" "200" "$R"

# 32. 释放自己的锁
R=$(curl -s -X DELETE "$B/repos/$REPO/locks/dir1/big.bin" -H "$AUTH")
check "释放自己的锁" "True" "$(echo "$R" | jq_get '["released"]')"

# 33. v0.4.17：目录锁整体移除 —— 对**已存在的目录**加锁 → 400（§5.2）
R=$(curl -s -X POST $B/repos/$REPO/locks -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"path":"dir1","comment":"whole subtree"}')
check "对目录加锁 → 400 INVALID_ARGUMENT" "INVALID_ARGUMENT" "$(echo "$R" | jq_get '["error"]["code"]')"

# 33b. 锁只作用于精确路径：锁住 dir1/big.bin 后，同目录的别的文件不受影响
curl -s -o /dev/null -X POST $B/repos/$REPO/locks -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"path":"dir1/big.bin","comment":"path scoped"}'
R=$(curl -s "$B/repos/$REPO/locks?path=dir1/other.bin" -H "$AUTH")
check "相邻路径不受锁影响" "0" "$(echo "$R" | jq_get '["items"].__len__()')"
R=$(curl -s -X DELETE "$B/repos/$REPO/locks/dir1/big.bin" -H "$AUTH")
check "释放文件锁" "True" "$(echo "$R" | jq_get '["released"]')"

echo
echo "===== 冒烟结果: PASS=$PASS_N FAIL=$FAIL_N (repo=$REPO) ====="
[ "$FAIL_N" = "0" ]
