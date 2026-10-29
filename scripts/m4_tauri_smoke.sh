#!/bin/bash
# b-artifact 客户端冒烟（v0.5.0：Tauri 2 + Rust 引擎）
#
# **Electron 时代的 143 项 CDP 断言随框架一起退役**：Tauri 下没有 CDP，
# 而 `tauri-driver` 只支持 Linux / Windows（macOS 没有 WebDriver 实现）。
# 覆盖改成分层，每层都真跑：
#
#   ① 引擎 / 命令层：`cargo test`（163 项，含 checkout/commit/update/冲突/锁）
#   ② 渲染层：`npm test`（201 项）
#   ③ 端到端生命周期：`scripts/m4_smoke.sh`（69 组断言，Rust CLI 打真服务端）
#   ④ **真实应用**：本脚本 —— release 二进制 + `--self-check`
#      （注入 JS 断言"首屏已挂载 + 一次 IPC 往返成功"，这两件事正好覆盖
#       白屏与 IPC 不通这两类打包事故）
#
# 用法：
#   scripts/m4_tauri_smoke.sh
#   SKIP_TESTS=1 scripts/m4_tauri_smoke.sh    # 跳过 ①②（只验应用能不能起来）
#
# 环境变量：CARGO（本沙箱不在 PATH 上，传 ~/.cargo/bin/cargo）/ SKIP_TESTS

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
CLIENT="$ROOT/client"
BIN="$CLIENT/src-tauri/target/release/b-artifact-app"

WORK=$(mktemp -d /tmp/b-artifact-tauri-smoke.XXXXXX)
trap 'rm -rf "$WORK"' EXIT

PASS_N=0
FAIL_N=0
ok()  { PASS_N=$((PASS_N + 1)); echo "  ✓ $1"; }
bad() { FAIL_N=$((FAIL_N + 1)); echo "  ✗ $1"; }

echo "== 1. 构建（渲染层 + Rust） =="
(cd "$CLIENT" && npm run build >/tmp/m4_tauri_build.log 2>&1) \
  && ok "渲染层构建" || { bad "渲染层构建失败（见 /tmp/m4_tauri_build.log）"; exit 1; }
# **必须走 `tauri build`，不能用 `cargo build --release`**：
# 前端产物是 `tauri::generate_context!()` 这个**过程宏**在读 tauri.conf.json 时内嵌的，
# 只改 dist/ 不会让 cargo 重新展开宏 —— 于是二进制里还是上一次（可能是空的）资源，
# 启动就是白屏，而且没有任何报错。真实踩过一次：release 二进制自检超时，
# 换成 `tauri build --no-bundle` 立刻正常。
(cd "$CLIENT" && npx tauri build --no-bundle > "$WORK/tauri-build.log" 2>&1) \
  && ok "tauri build（release）" || { bad "打包失败（见 $WORK/tauri-build.log）"; exit 1; }
[ -x "$BIN" ] && ok "二进制就绪（$(basename "$BIN")）" || { bad "缺少 $BIN"; exit 1; }

if [ "${SKIP_TESTS:-0}" != "1" ]; then
  echo "== 2. 单测（引擎 + 命令层） =="
  if (cd "$CLIENT/src-tauri" && "$CARGO" test --lib --quiet >/tmp/m4_tauri_cargo_test.log 2>&1); then
    N=$(grep -oE '[0-9]+ passed' /tmp/m4_tauri_cargo_test.log | tail -1 | grep -oE '[0-9]+')
    ok "cargo test 通过（${N} 项）"
  else
    bad "cargo test 失败（见 /tmp/m4_tauri_cargo_test.log）"
  fi

  echo "== 3. 单测（渲染层） =="
  if (cd "$CLIENT" && npm test >/tmp/m4_tauri_npm_test.log 2>&1); then
    N=$(grep -oE 'Tests[[:space:]]+[0-9]+ passed' /tmp/m4_tauri_npm_test.log | tail -1 | grep -oE '[0-9]+')
    ok "vitest 通过（${N} 项）"
  else
    bad "vitest 失败（见 /tmp/m4_tauri_npm_test.log）"
  fi
fi

echo "== 4. 真实应用自检（首屏挂载 + IPC 往返） =="
# 用独立的 HOME，避免碰到开发者真实的配置与钥匙串
export B_ARTIFACT_HOME="$(mktemp -d /tmp/b-artifact-app-home.XXXXXX)"
set +e
SELF_CHECK_OUT=$("$BIN" --self-check 2>&1)
SELF_CHECK_RC=$?
set -e
echo "$SELF_CHECK_OUT" | sed 's/^/    /'
case "$SELF_CHECK_RC" in
  0) ok "self-check 通过（前端已挂载且 IPC 通）" ;;
  3) bad "self-check：前端没挂载或 IPC 不通（白屏/桥断了）" ;;
  4) bad "self-check：注入的 JS 抛异常" ;;
  5) bad "self-check 超时：前端完全没跑起来（多半白屏）" ;;
  *) bad "self-check 异常退出（rc=$SELF_CHECK_RC）" ;;
esac
rm -rf "$B_ARTIFACT_HOME"

echo "---- 结果：${PASS_N} 通过 / ${FAIL_N} 失败 ----"
[ "$FAIL_N" -eq 0 ] || exit 1
