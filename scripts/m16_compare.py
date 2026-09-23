#!/usr/bin/env python3
"""M1.6 修复前后对比：读两份 m15-<scale>.json，输出 markdown 对比表。

用法：
    python3 scripts/m16_compare.py --before bench-results-m15-baseline \
        --after bench-results --scales xs,s,m [--out /tmp/m16-compare.md]

口径：
  - 取 p50（单位随 metric 变化，如实打印）
  - 提速 = before / after，>1 为变快
  - 只输出两侧都存在的基准项；缺项在表末"仅单侧存在"里列出，避免静默丢数据
"""

import argparse
import json
import os
import sys

# 关注的基准项（顺序即表格顺序）。分组 -> 项名列表
FOCUS = {
    "HEAD 列目录（B4 parent_path）": [
        ("storage", "list_dir_head_root"),
        ("storage", "list_dir_head_l1"),
        ("storage", "list_dir_head_l2"),
        ("storage", "list_dir_head_leaf"),
    ],
    "历史推导列目录（B1 前缀下推 + B2）": [
        ("storage", "list_dir_at_root_head"),
        ("storage", "list_dir_at_l1_mid"),
        ("storage", "list_dir_at_l2_mid"),
        ("storage", "list_dir_at_leaf_head"),
        ("storage", "list_dir_at_leaf_mid"),
    ],
    "单路径操作（B2 语句缓存）": [
        ("storage", "file_at_head"),
        ("storage", "file_at_old_rev"),
        ("storage", "dir_exists_at_head"),
        ("sweep", "file_at_x1"),
        ("sweep", "file_at_x10"),
        ("sweep", "file_at_x100"),
    ],
    "写入（B5 blob_hash 索引）": [
        ("storage", "apply_commit_10"),
        ("storage", "apply_commit_100"),
        ("storage", "apply_commit_1000"),
    ],
    "前缀扫描（B1 证据，SQL 层）": [
        ("sweep", "head_prefix_like_root"),
        ("sweep", "head_prefix_glob_root"),
        ("sweep", "head_prefix_like_l2"),
        ("sweep", "head_prefix_glob_l2"),
        ("sweep", "changes_prefix_like_l2"),
        ("sweep", "changes_prefix_glob_l2"),
    ],
    "HTTP 端到端": [
        ("http", "tree_root_depth1"),
        ("http", "tree_root_depth2"),
        ("http", "tree_root_hist_rev"),
        ("http", "repo_info"),
        ("http", "changes_100rev"),
        ("http", "missing_1000"),
    ],
}

CONC = [
    "tree_root_depth1_c8", "tree_root_depth1_c32",
    "download_blob_c8", "download_blob_c32",
    "tree_hist_leaf_c8", "tree_hist_leaf_c32",
    "tree_head_file_c8", "tree_head_file_c32",
    "repo_info_c8", "repo_info_c32",
    "log_50_c8", "log_50_c32",
]


def fmt(v, unit):
    if v is None:
        return "-"
    if unit == "ms":
        if v >= 1000:
            return f"{v / 1000:.2f} s"
        if v >= 1:
            return f"{v:.2f} ms"
        return f"{v * 1000:.0f} µs"
    return f"{v:.2f} {unit}"


def speedup(before, after):
    if not before or not after:
        return "-"
    r = before / after
    if r >= 1.05:
        return f"**{r:.1f}×**"
    if r <= 0.95:
        return f"{r:.2f}×（变慢）"
    return f"{r:.2f}×"


def load(d, scale):
    p = os.path.join(d, f"m15-{scale}.json")
    if not os.path.exists(p):
        return None
    j = json.load(open(p))
    return {(m["group"], m["name"]): m for m in j["metrics"]}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--before", required=True)
    ap.add_argument("--after", required=True)
    ap.add_argument("--scales", default="xs,s,m")
    ap.add_argument("--out", default=None)
    a = ap.parse_args()

    scales = [s.strip() for s in a.scales.split(",") if s.strip()]
    out = []
    w = out.append

    for scale in scales:
        B, A = load(a.before, scale), load(a.after, scale)
        if not B or not A:
            w(f"> ⚠️ {scale} 档缺结果（before={'有' if B else '无'} after={'有' if A else '无'}），跳过\n")
            continue
        w(f"### {scale} 档\n")
        missing = []
        for title, items in FOCUS.items():
            rows = []
            for g, n in items:
                b, af = B.get((g, n)), A.get((g, n))
                if not b or not af:
                    if b or af:
                        missing.append(f"{g}.{n}")
                    continue
                rows.append(
                    f"| `{n}` | {fmt(b['p50'], b['unit'])} | {fmt(af['p50'], af['unit'])} "
                    f"| {speedup(b['p50'], af['p50'])} | {b['n']}/{af['n']} |"
                )
            if not rows:
                continue
            w(f"**{title}**\n")
            w("| 基准 | 修复前 p50 | 修复后 p50 | 提速 | n(前/后) |")
            w("|---|---|---|---|---|")
            out.extend(rows)
            w("")

        rows = []
        for n in CONC:
            b, af = B.get(("conc", n)), A.get(("conc", n))
            if not b or not af:
                continue
            tb, ta = b.get("throughput") or 0, af.get("throughput") or 0
            rows.append(
                f"| `{n}` | {fmt(b['p50'], b['unit'])} | {fmt(af['p50'], af['unit'])} "
                f"| {tb:.0f}/s | {ta:.0f}/s | {speedup(ta, tb) if tb and ta else '-'} |"
            )
        if rows:
            w("**并发扩展性（B6 连接池）**")
            w("")
            w("| 基准 | 前 p50 | 后 p50 | 前吞吐 | 后吞吐 | 吞吐提升 |")
            w("|---|---|---|---|---|---|")
            out.extend(rows)
            w("")
        if missing:
            w(f"<!-- 仅单侧存在：{', '.join(missing)} -->")
            w("")

    text = "\n".join(out)
    if a.out:
        open(a.out, "w").write(text)
        print(f"已写入 {a.out}", file=sys.stderr)
    print(text)


if __name__ == "__main__":
    main()
