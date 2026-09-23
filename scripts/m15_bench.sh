#!/usr/bin/env bash
# b-artifact M1.5 压测编排脚本（方案 §12.7 / §14）。
#
# 做四件事：
#   1. 记录压测环境（CPU / 内存 / 工具链 / 数据目录占用）
#   2. 按档位调用 scripts/gen_fixture.py 造数（已存在则复用，--force 重新造）
#   3. 把 fixture 复制一份作为本次运行的副本（APFS clonefile，秒级），保证 fixture 可重复使用
#   4. 用 release(bench profile) 跑 server/benches/m15_bench.rs，原始 JSON 落到 $OUT_DIR
#
# 用法：
#   scripts/m15_bench.sh                            # 跑 xs,s,m 三档
#   scripts/m15_bench.sh --scales xs,s,m,full       # 含 1M 文件 / 100k 修订档
#   scripts/m15_bench.sh --scales xs --iterations 60 --budget 30
#   scripts/m15_bench.sh --scales xs --reuse --only storage,sweep
#
# 产物：$OUT_DIR/{env.json,fixture-<scale>.json,m15-<scale>.json,run.log}
# 汇总：python3 scripts/m15_summary.py $OUT_DIR
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SCALES="xs,s,m"
OUT_DIR="$REPO_ROOT/bench-results"
DATA_ROOT="/tmp/b-artifact-fixtures"
ITERATIONS=""
BUDGET=""
ONLY=""
CONCURRENCY=""
HEAVY="auto"
REUSE=0
FORCE=0
KEEP_RUN=0

usage() {
    sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --scales)      SCALES="$2"; shift 2 ;;
        --out-dir)     OUT_DIR="$2"; shift 2 ;;
        --data-root)   DATA_ROOT="$2"; shift 2 ;;
        --iterations)  ITERATIONS="$2"; shift 2 ;;
        --budget)      BUDGET="$2"; shift 2 ;;
        --only)        ONLY="$2"; shift 2 ;;
        --concurrency) CONCURRENCY="$2"; shift 2 ;;
        --heavy)       HEAVY="$2"; shift 2 ;;
        --reuse)       REUSE=1; shift ;;
        --force)       FORCE=1; shift ;;
        --keep-run)    KEEP_RUN=1; shift ;;
        -h|--help)     usage 0 ;;
        *) echo "未知参数: $1" >&2; usage 1 ;;
    esac
done

# 相对路径会在后续 cd 之后失效（曾导致 --out-dir 静默写不进结果），统一转成绝对路径
case "$OUT_DIR" in /*) ;; *) OUT_DIR="$REPO_ROOT/$OUT_DIR" ;; esac
case "$DATA_ROOT" in /*) ;; *) DATA_ROOT="$REPO_ROOT/$DATA_ROOT" ;; esac

mkdir -p "$OUT_DIR" "$DATA_ROOT"
LOG="$OUT_DIR/run.log"
: > "$LOG"

say() { echo "$*" | tee -a "$LOG"; }

# ---------- 1. 环境记录 ----------
{
    echo "{"
    echo "  \"date\": \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\","
    echo "  \"os\": \"$(uname -srm)\","
    echo "  \"cpu\": \"$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)\","
    echo "  \"n_cpu\": $(sysctl -n hw.ncpu 2>/dev/null || echo 0),"
    echo "  \"mem_bytes\": $(sysctl -n hw.memsize 2>/dev/null || echo 0),"
    echo "  \"rustc\": \"$(rustc --version)\","
    echo "  \"cargo\": \"$(cargo --version)\","
    echo "  \"python\": \"$(python3 --version)\","
    echo "  \"repo_root\": \"$REPO_ROOT\","
    echo "  \"scales\": \"$SCALES\""
    echo "}"
} > "$OUT_DIR/env.json"
say "== 环境 =="
cat "$OUT_DIR/env.json" | tee -a "$LOG"

# ---------- 2. 编译一次（bench profile 才代表真实性能） ----------
say ""
say "== 编译 bench profile =="
( cd "$REPO_ROOT/server" && cargo build --benches --profile bench 2>&1 | tee -a "$LOG" | tail -3 )

# ---------- 3. 逐档位：造数 → 复制 → 压测 ----------
IFS=',' read -r -a SCALE_ARR <<< "$SCALES"
for scale in "${SCALE_ARR[@]}"; do
    fixture_dir="$DATA_ROOT/$scale"
    run_dir="$DATA_ROOT/$scale-run"
    say ""
    say "============================= 档位 $scale =============================

"

    # 3.1 造数
    if [[ $FORCE -eq 1 || ! -f "$fixture_dir/b-artifact.db" ]]; then
        say "-- 造数 → $fixture_dir"
        python3 "$SCRIPT_DIR/gen_fixture.py" --scale "$scale" --data-dir "$fixture_dir" \
            > "$OUT_DIR/fixture-$scale.json" 2> >(tee -a "$LOG" >&2)
        python3 - "$OUT_DIR/fixture-$scale.json" <<'PY' | tee -a "$LOG"
import json, sys
d = json.load(open(sys.argv[1]))
print(f"-- 造数完成 files={d['total_files']} revs={d['revs']} changes={d['rows_changes']} "
      f"db={d['db_bytes'] / 1048576:.1f}MB blobs={d['blob_bytes_total'] / 1048576:.1f}MB "
      f"gen={d['gen_seconds']}s")
PY
    elif [[ $REUSE -eq 1 ]]; then
        say "-- 复用已有 fixture（--reuse）"
    else
        say "-- fixture 已存在，直接复用（--force 可重造）"
    fi

    # 3.2 复制一份本次运行用的副本：压测会真写库（两阶段提交做真实落库），
    #     fixture 本身必须保持干净，否则同档位无法重复压测。
    say "-- 复制运行副本 → $run_dir"
    rm -rf "$run_dir"
    if ! cp -Rc "$fixture_dir" "$run_dir" 2>/dev/null; then
        cp -R "$fixture_dir" "$run_dir"
    fi

    # 3.3 压测
    args=(--data-dir "$run_dir" --out "$OUT_DIR/m15-$scale.json" --label "$scale" --heavy "$HEAVY")
    [[ -n "$ITERATIONS"  ]] && args+=(--iterations "$ITERATIONS")
    [[ -n "$BUDGET"      ]] && args+=(--budget "$BUDGET")
    [[ -n "$ONLY"        ]] && args+=(--only "$ONLY")
    [[ -n "$CONCURRENCY" ]] && args+=(--concurrency "$CONCURRENCY")

    say "-- 压测开始 $(date -u +%H:%M:%SZ)：cargo bench --bench m15_bench -- ${args[*]}"
    start=$(date +%s)
    ( cd "$REPO_ROOT/server" && cargo bench --bench m15_bench -- "${args[@]}" ) \
        2>&1 | tee -a "$LOG"
    say "-- 压测结束，耗时 $(( $(date +%s) - start ))s"

    if [[ $KEEP_RUN -eq 0 ]]; then
        rm -rf "$run_dir"
    fi
done

say ""
say "== 全部完成 =="
say "原始数据: $OUT_DIR"
say "生成汇总: python3 $SCRIPT_DIR/m15_summary.py $OUT_DIR"
