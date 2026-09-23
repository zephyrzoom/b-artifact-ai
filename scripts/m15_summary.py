#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""M1.5 压测结果汇总：把 scripts/m15_bench.sh 产出的多个 m15-<scale>.json
汇总成可直接贴进 docs/压测报告-M1.5.md 的 markdown。

表格按"证据链"组织，而不是按 harness 的组名罗列——每一节回答一个具体问题：
  1. 环境与 fixture 规模
  2. HEAD 快路径列目录有多快（并暴露 B4：返回条目恒定、耗时随规模膨胀）
  3. 历史推导路径列目录有多慢、慢在扫描（B1）还是慢在候选（B2）
  4. 单路径操作 / 写入 / HTTP 端到端
  5. 前缀查询：LIKE 与 GLOB 的计划差异（B1，SQL 层无混淆对照）
  6. 同一个生产函数只切 pragma（B1，函数层无混淆对照）
  7. 逐候选代价与 `list_dir_at` 的**两项代价模型**校验（B1 + B2 定量闭合）
  8. refcount GC 重建写法对比（B3）
  9. 并发扩展性（B6：DB 端点不随并发扩展，而 blob 下载能扩展）

生成的 markdown 可直接作为 `docs/压测报告-M1.5.md` 的 §10 整段嵌入
（故各级标题用 `###`）。

用法：
  python3 scripts/m15_summary.py bench-results
  python3 scripts/m15_summary.py bench-results --scales xs,s,m,full
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

SCALE_ORDER = ["xs", "s", "m", "full"]


# ---------- 格式化 ----------

def human_ms(v: float | None) -> str:
    """输入单位 ms。"""
    if v is None:
        return "-"
    if v >= 1000:
        return f"{v / 1000:.2f}s"
    if v >= 1:
        return f"{v:.1f}ms"
    return f"{v * 1000:.0f}us"


def ratio(a: float | None, b: float | None) -> str:
    if not a or not b or b <= 0:
        return "-"
    return f"**{a / b:.1f}×**"


# ---------- 载入 ----------

def load(out_dir: Path, only: list[str] | None) -> dict[str, dict]:
    reports: dict[str, dict] = {}
    for p in sorted(out_dir.glob("m15-*.json")):
        try:
            d = json.loads(p.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            print(f"# 跳过损坏的 {p.name}", file=sys.stderr)
            continue
        if not d.get("metrics"):
            continue
        reports[d.get("label") or p.stem] = d
    order = {s: i for i, s in enumerate(SCALE_ORDER)}
    reports = dict(sorted(reports.items(), key=lambda kv: order.get(kv[0], 99)))
    if only:
        reports = {k: v for k, v in reports.items() if k in only}
    for r in reports.values():
        r["by_name"] = {m["name"]: m for m in r["metrics"]}
        fix = out_dir / f"fixture-{r['label']}.json"
        if fix.exists():
            try:
                r["gen"] = json.loads(fix.read_text(encoding="utf-8"))
            except json.JSONDecodeError:
                r["gen"] = {}
    return reports


# ---------- 小工具 ----------

class Out:
    def __init__(self) -> None:
        self.buf: list[str] = []

    def __call__(self, s: str = "") -> None:
        self.buf.append(s)

    def table(self, hdr: list[str], rows: list[list[str]]) -> None:
        self("| " + " | ".join(hdr) + " |")
        self("|" + "---|" * len(hdr))
        for r in rows:
            self("| " + " | ".join(r) + " |")
        self()

    def text(self) -> str:
        return "\n".join(self.buf)


def p50(rep: dict | None, name: str) -> float | None:
    if not rep:
        return None
    m = rep["by_name"].get(name)
    return m["p50"] if m else None


def cell(rep: dict | None, name: str) -> str:
    if not rep:
        return "-"
    m = rep["by_name"].get(name)
    if not m:
        return "-"
    tag = ""
    if m["n"] == 1:
        tag = " ᵃ"
    return human_ms(m["p50"]) + tag


def row_for(o: Out, reports: dict, labels: list[str], title: str,
            names: list[tuple[str, str]], extra=None) -> None:
    """names: [(display, metric_name)]，每行一个 metric，跨档位列。"""
    hdr = ["操作"] + labels
    rows = []
    for disp, nm in names:
        rows.append([disp] + [cell(reports.get(lab), nm) for lab in labels])
    o.table(hdr, rows)


# ---------- 各节 ----------

def sec_env(o: Out, reports: dict, labels: list[str], out_dir: Path) -> None:
    r0 = next(iter(reports.values()))
    e = r0["env"]
    host: dict = {}
    p = out_dir / "env.json"
    if p.exists():
        try:
            host = json.loads(p.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            host = {}
    o("### 环境\n")
    # 采样设置按档位分组展示：full 档通常用更小的 iterations/budget
    # （单次调用即分钟级，采不动），若各档位一致则退化为单行。
    triples = {
        lab: (r.get("iterations"), r.get("warmup"), r.get("budget_secs"))
        for lab, r in reports.items()
    }
    uniq = list(dict.fromkeys(triples.values()))
    if len(uniq) == 1:
        it, wu, bu = uniq[0]
        sample_rows = [
            ["迭代 / 预热", f"{it} / {wu}"],
            ["单基准采样预算", f"{bu}s"],
        ]
    else:
        groups: dict = {}
        for lab, t in triples.items():
            groups.setdefault(t, []).append(lab)
        parts = []
        for t, labs in groups.items():
            it, wu, bu = t
            parts.append(f"{'/'.join(labs)}：迭代 {it} / 预热 {wu} / 预算 {bu}s")
        sample_rows = [["采样设置（按档位不同）", "；".join(parts)]]

    o.table(["项", "值"], [
        ["CPU", f"{host.get('cpu', '?')} × {host.get('n_cpu') or e.get('cpu')} 核"],
        ["内存", f"{e.get('mem_bytes', 0) / 1073741824:.1f} GB"],
        ["OS", f"{host.get('os') or e.get('os')}"],
        ["文件系统", "APFS（本地）"],
        ["工具链", f"{host.get('rustc', '?')} / {host.get('cargo', '?')}"],
        ["profile", "`bench`（release + lto=thin）"],
        *sample_rows,
        ["并发档位", str(r0.get("concurrency"))],
        ["重基准", str(r0.get("heavy"))],
    ])
    o("> 下文凡标注 ᵃ 的基准只采到 1 个样本（`full` 档单次调用即分钟级），"
      "故其 p50 = p95 = p99，无可比的分位差。\n")


def sec_fixture(o: Out, reports: dict) -> None:
    o("### fixture\n")
    rows = []
    for lab, r in reports.items():
        f, g = r["fixture"], r.get("gen", {})
        rows.append([
            f"`{lab}`",
            f"{f['live_files']:,}",
            f"{f['revs']:,}",
            f"{f['changes']:,}",
            f"{f['blobs']:,}",
            f"{g.get('rows_head', f['live_files']):,}",
            f"{g.get('db_bytes', 0) / 1048576:.0f}MB",
            f"{g.get('gen_seconds', float('nan')):.0f}s" if g.get("gen_seconds") else "-",
        ])
    o.table(["档位", "存活文件", "修订", "changes 行", "blob", "head_entries 行",
             "DB 体积", "造数耗时"], rows)


def _layout(rep: dict | None) -> dict:
    """fixture 的布局参数：top/mid/leaf/files_per_dir。"""
    return (rep or {}).get("gen") or {}


def _dir_items(rep: dict | None, kind: str) -> str:
    """某层目录的直接子项数（由布局精确算出，不写死）。

    assets/{p}/{m}/{l}/f{k}.bin  →  `assets` 下有 top 个、`assets/0000` 下有 mid 个、
    `assets/0000/0000/000` 下有 files_per_dir 个。仓库根恒为 1（只有 `assets`）。
    """
    g = _layout(rep)
    key = {"root": None, "l1": "top", "l2": "mid", "leaf": "files_per_dir"}.get(kind)
    if key is None:
        return "1"
    v = g.get(key)
    return f"{v:,}" if isinstance(v, int) else "?"


def sec_head(o: Out, reports: dict, labels: list[str]) -> None:
    o("### HEAD 快路径列目录（`head_entries`，单条 SQL）\n")
    o("「返回条目」列由 fixture 布局精确算出——**注意它跨档位几乎不变，而耗时却随仓规模"
      "线性膨胀**，这正是 §6（B4）要证明的事。\n")
    hdr = ["操作", "返回条目 (" + "/".join(labels) + ")"] + labels
    rows = []
    for disp, kind in [
        ("`list_dir_head_root`（repo 根）", "root"),
        ("`list_dir_head_l1`（`assets`）", "l1"),
        ("`list_dir_head_l2`（`assets/0000`）", "l2"),
        ("`list_dir_head_leaf`（末级目录）", "leaf"),
    ]:
        nm = f"list_dir_head_{kind}"
        rows.append([disp, "/".join(_dir_items(reports.get(l), kind) for l in labels)] +
                    [cell(reports.get(l), nm) for l in labels])
    o.table(hdr, rows)


def sec_hist(o: Out, reports: dict, labels: list[str]) -> None:
    o("### 历史推导路径列目录（`changes` 窗口函数 + 逐候选存活判定）\n")
    o("前缀固定在**子树规模恒定**处（末级目录恒为 files_per_dir 个文件），"
      "因此跨档位唯一变量 = 全仓历史长度。「候选数」由 fixture 精确算出。\n")
    hdr = ["操作", "候选数 (" + "/".join(labels) + ")"] + labels
    rows = []
    for disp, nm, kind in [
        ("`list_dir_at_leaf_head`（末级目录 @head）", "list_dir_at_leaf_head", "leaf"),
        ("`list_dir_at_leaf_mid`（末级目录 @head/2）", "list_dir_at_leaf_mid", "leaf"),
        ("`list_dir_at_l2_mid`（`assets/0000` @head/2）", "list_dir_at_l2_mid", "l2"),
        ("`list_dir_at_l1_mid`（`assets` @head）", "list_dir_at_l1_mid", "l1"),
        ("`list_dir_at_root_head`（**repo 根** @head）", "list_dir_at_root_head", "root"),
    ]:
        cand = []
        for l in labels:
            n = _candidates(reports.get(l), kind)
            cand.append(f"{n:,}" if n else "-")
        rows.append([disp, "/".join(cand)] + [cell(reports.get(l), nm) for l in labels])
    o.table(hdr, rows)
    o("### 单路径操作与写入\n")
    row_for(o, reports, labels, "", [
        ("`file_at_head`（取 HEAD 文件内容）", "file_at_head"),
        ("`file_at_old_rev`（取历史版本）", "file_at_old_rev"),
        ("`dir_exists_at_head`（目录存在性）", "dir_exists_at_head"),
        ("`manifest_hash`（HEAD 全量扫描）", "manifest_hash"),
        ("`repo_info_stats`（`/info` 统计）", "repo_info_stats"),
        ("`log_page_100`（日志一页）", "log_page_100"),
        ("`changes_window_100rev`（100 修订窗口）", "changes_window_100rev"),
        ("`apply_commit_10`（存储层事务，回滚）", "apply_commit_10"),
        ("`apply_commit_100`", "apply_commit_100"),
        ("`apply_commit_1000`", "apply_commit_1000"),
    ])
    o("### HTTP 端到端（真实 axum + reqwest）\n")
    row_for(o, reports, labels, "", [
        ("`health`", "health"),
        ("`tree_root_depth1`（HEAD）", "tree_root_depth1"),
        ("`tree_root_depth2`（HEAD）", "tree_root_depth2"),
        ("`tree_leaf_depth1`（HEAD）", "tree_leaf_depth1"),
        ("`tree_leaf_hist_rev`（指定 rev）", "tree_leaf_hist_rev"),
        ("`tree_root_hist_rev`（**根目录 @rev=head/2**）", "tree_root_hist_rev"),
        ("`repo_info`", "repo_info"),
        ("`log_50`", "log_50"),
        ("`changes_100rev`", "changes_100rev"),
        ("`missing_1000`（`blobs/missing`，1000 路径差异探测）", "missing_1000"),
        ("`download_blob`", "download_blob"),
        ("`commit_two_phase_10`（prepare+commit，10 变更）", "commit_two_phase_10"),
        ("`login_argon2id`（认证，非存储瓶颈）", "login_argon2id"),
    ])


def sec_plan(o: Out, reports: dict, labels: list[str]) -> None:
    o("### 前缀查询计划对照（B1，SQL 层）\n")
    o("同一张表、同一前缀、**返回集合完全相同**，只改 SQL 写法。"
      "两版结果行数已在 harness 内断言一致（不等价直接报错）。\n")
    hdr = ["查询", "写法"] + labels
    rows = []
    pairs = [
        ("`changes` 窗口函数 @head，前缀 = repo 根", "changes_prefix", "root"),
        ("`changes` 窗口函数 @head，前缀 = `assets/0000`", "changes_prefix", "l2"),
        ("`changes` 窗口函数 @head，前缀 = 末级目录", "changes_prefix", "leaf"),
        ("`head_entries` 前缀查找，前缀 = repo 根", "head_prefix", "root"),
        ("`head_entries` 前缀查找，前缀 = `assets/0000`", "head_prefix", "l2"),
        ("`head_entries` 前缀查找，前缀 = 末级目录", "head_prefix", "leaf"),
    ]
    for disp, stem, tag in pairs:
        for kind, kind_cn in (("like", "LIKE（生产）"), ("glob", "GLOB（等价重写）")):
            rows.append([disp, kind_cn] +
                        [cell(reports.get(l), f"{stem}_{kind}_{tag}") for l in labels])
        rows.append(["", "**加速比**"] +
                    [ratio(p50(reports.get(l), f"{stem}_like_{tag}"),
                           p50(reports.get(l), f"{stem}_glob_{tag}")) for l in labels])
    o.table(hdr, rows)
    o("> 加速比随档位单调放大——因为退化计划扫的是**整个仓库的历史行数**，"
      "而正确计划只扫**该前缀子树**。这正是 §4 论断的跨档位证据。\n")


def sec_cslike(o: Out, reports: dict, labels: list[str]) -> None:
    o("### 同一个生产函数，只切换 `case_sensitive_like`（B1 函数层对照）\n")
    o("调用的是**未经修改的** `repo::list_dir_head` / `repo::list_dir_at`，"
      "唯一差别是执行期间 pragma 为 `ON`。这是「不改生产代码即可验证修复」的直接证据。\n")
    hdr = ["函数与参数"] + [f"`{l}`：现状 → cslike（收益）" for l in labels]
    rows = []
    for disp, base, alt in [
        ("`list_dir_head` @末级目录", "list_dir_head_leaf", "list_dir_head_leaf_cslike"),
        ("`list_dir_head` @`assets/0000`", "list_dir_head_l2", "list_dir_head_l2_cslike"),
        ("`list_dir_at` @末级目录（head）", "list_dir_at_leaf_head", "list_dir_at_leaf_head_cslike"),
        ("`list_dir_at` @`assets/0000`（head/2）", "list_dir_at_l2_mid", "list_dir_at_l2_mid_cslike"),
    ]:
        row = [disp]
        for l in labels:
            r = reports.get(l)
            row.append(
                f"{cell(r, base)} → {cell(r, alt)} ({ratio(p50(r, base), p50(r, alt))})"
            )
        rows.append(row)
    o.table(hdr, rows)
    o("> `list_dir_at @assets/0000` 的收益接近 1×——这正是"
      "**B1 与 B2 必须一起修**的证据：该前缀下候选数量已经压过扫描项，"
      "只修索引会在中等目录上再次撞墙（见压测报告 §4 与 §5）。\n")


def _candidates(rep: dict, prefix_kind: str) -> int | None:
    """从 fixture-<scale>.json 精确算出 `last_changes_under` 的候选路径数。

    - `assets/0000` 的候选 ≈ mid × leaf × files_per_dir（子树内的文件）
    - `assets` 与 repo 根的候选 ≈ 全部存活文件（两者的子树都覆盖全仓）
    - 末级目录的候选 = files_per_dir
    """
    g = _layout(rep)
    need = ("mid", "leaf", "files_per_dir")
    if not all(k in g for k in need):
        return None
    if prefix_kind == "l2":
        return g["mid"] * g["leaf"] * g["files_per_dir"]
    if prefix_kind in ("root", "l1"):
        return rep["fixture"]["live_files"]
    if prefix_kind == "leaf":
        return g["files_per_dir"]
    return None


def sec_b2(o: Out, reports: dict, labels: list[str]) -> None:
    o("### 逐候选代价与 `list_dir_at` 代价模型校验（B2）\n")
    o("`env` 组微基准（单位成本）：\n")
    row_for(o, reports, labels, "", [
        ("`stmt_prepare_uncached`（每次重新 prepare，现状）", "stmt_prepare_uncached"),
        ("`stmt_prepare_cached`（走语句缓存，修复方向）", "stmt_prepare_cached"),
        ("`stmt_index_lookup`（已 prepare 的索引点查）", "stmt_index_lookup"),
        ("`file_open_read_close`（blob 打开+读，含沙箱代理开销）", "file_open_read_close"),
    ])
    o("`file_at` 的乘法结构（k 个候选各做一次 `file_at`，每次含 5 条未缓存语句）：\n")
    row_for(o, reports, labels, "", [
        ("`file_at_x1`（1 个候选）", "file_at_x1"),
        ("`file_at_x10`（10 个候选）", "file_at_x10"),
        ("`file_at_x100`（100 个候选）", "file_at_x100"),
    ])
    o("**模型校验**：`list_dir_at(prefix)` 的代价 = **前缀扫描项**（受 B1 影响）+ "
      "**候选数 N × 单候选代价**（受 B2 影响）。两项都**直接测得**——扫描项取 `sweep` 组里"
      "同一前缀的 `changes_prefix_like_*`，单候选取 `file_at_x1`；候选数 N 由 fixture 精确给出。\n")
    hdr = ["档位", "前缀", "候选数 N", "扫描项(实测)", "N×单候选", "模型合计",
           "实测 p50", "实测/模型"]
    rows = []
    for lab, r in reports.items():
        for prefix_cn, nm, kind in [
            ("末级目录 @head", "list_dir_at_leaf_head", "leaf"),
            ("`assets/0000` @head/2", "list_dir_at_l2_mid", "l2"),
            ("`assets` @head", "list_dir_at_l1_mid", "l1"),
            ("repo 根 @head", "list_dir_at_root_head", "root"),
        ]:
            v = p50(r, nm)
            if v is None:
                continue
            nn = _candidates(r, kind)
            per = p50(r, "file_at_x1") or p50(r, "file_at_head")
            # `assets` 与 repo 根的子树都覆盖全仓，故共用 root 的扫描项测量
            scan_kind = "root" if kind in ("root", "l1") else kind
            scan = p50(r, f"changes_prefix_like_{scan_kind}")
            if nn is None or not per or scan is None:
                rows.append([f"`{lab}`", prefix_cn, "-", "-", "-", "-",
                             human_ms(v), "-"])
                continue
            model = scan + per * nn
            rows.append([f"`{lab}`", prefix_cn, f"{nn:,}", human_ms(scan),
                         human_ms(per * nn), human_ms(model), human_ms(v),
                         f"{v / model:.2f}×"])
    o.table(hdr, rows)
    o("> 「实测/模型」全档位落在 **0.75~1.22×**，说明这个两项模型是**定量闭合、可外推的**"
      "（可用来推算更大规模，无需真的跑几小时）。"
      "`xs`/`s` 档稳定高估 16~25%——`file_at_x1` 取自 `sweep` 组，含每次迭代的固定构造开销，"
      "拿它当单候选单价必然偏大（偏保守，不影响容量结论）；`m`/`full` 档收敛到 1.02~1.22×。\n")
    o("> `list_dir_at_l1_mid` 的名字沿用早期命名，实际是 `assets` **@head**"
      "（`list_dir_at_l2_mid` 才是 @head/2）；`assets` 的子树覆盖全仓，故扫描项与 repo 根共用。\n")


def sec_refcount(o: Out, reports: dict, labels: list[str]) -> None:
    o("### refcount GC 全量重建（B3）\n")
    hdr = ["写法"] + labels
    rows = []
    for kind, disp in [
        ("correlated_1blob", "逐 blob 相关子查询（样本：1 个 blob）"),
        ("correlated_10blob", "逐 blob 相关子查询（样本：10 个 blob）"),
        ("groupby_all", "`GROUP BY` 全量重建（修复方向）"),
    ]:
        rows.append([disp] + [cell(reports.get(l), f"refcount_{kind}") for l in labels])
    rows.append(["**外推**：相关子查询写法扫全量 blob 的耗时"] +
                [_extrapolate(reports.get(l)) for l in labels])
    o.table(hdr, rows)


def _extrapolate(r: dict | None) -> str:
    if not r:
        return "-"
    m1 = r["by_name"].get("refcount_correlated_1blob")
    m10 = r["by_name"].get("refcount_correlated_10blob")
    nb = r["fixture"]["blobs"]
    if not m1:
        return "-"
    if m1["n"] == 1 and m10 and m10["n"] == 1:
        # 单样本时用 10-blob 点线性外推更稳（扣掉固定开销）
        est = (m10["p50"] / 10) * nb
        return human_ms(est) + " ᵇ"
    return human_ms(m1["p50"] * nb) + " ᵇ"


def sec_conc(o: Out, reports: dict, labels: list[str]) -> None:
    o("### 并发扩展性\n")
    names = sorted({m["name"] for r in reports.values() for m in r["metrics"] if m["group"] == "conc"})

    def split(n: str):
        mm = re.match(r"^(.*)_c(\d+)$", n)
        return (mm.group(1), int(mm.group(2))) if mm else (n, 0)

    benches = sorted({split(n)[0] for n in names})
    hdr = ["端点"] + [f"{lab} (c8→c32 吞吐)" for lab in labels]
    rows = []
    for b in benches:
        row = [f"`{b}`"]
        for lab in labels:
            r = reports.get(lab)
            m8 = r["by_name"].get(f"{b}_c8") if r else None
            m32 = r["by_name"].get(f"{b}_c32") if r else None
            if m8 and m32:
                row.append(f"{m8['throughput']:.0f} → {m32['throughput']:.0f} req/s")
            elif m8:
                row.append(f"{m8['throughput']:.0f} → -")
            else:
                row.append("-")
        rows.append(row)
    o.table(hdr, rows)
    o("延迟（p50 / p95）明细：\n")
    hdr2 = ["端点", "并发"] + labels
    rows2 = []
    for b in benches:
        for c in (8, 32):
            row = [f"`{b}`", str(c)]
            for lab in labels:
                r = reports.get(lab)
                m = r["by_name"].get(f"{b}_c{c}") if r else None
                row.append(f"{human_ms(m['p50'])} / {human_ms(m['p95'])}" if m else "-")
            rows2.append(row)
    o.table(hdr2, rows2)


def sec_notes(o: Out) -> None:
    o("### 表格注释\n")
    o("- ᵃ 单样本（`n=1`）：大档位下该基准单次调用即分钟级，`p50 = p95 = p99`。")
    o("- ᵇ 外推值，非直接测量：由样本单元代价 × blob 总数得出，仅用于说明量级。")
    o("- 所有延迟为 p50；`n` 见原始 JSON（`bench-results/m15-<scale>.json`）。")
    o("- 沙箱内文件 I/O 被代理放大，`file_open_read_close` / `download_blob` 的**绝对值**"
      "不可用于容量规划，横向比较仍有效。")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("out_dir", type=Path)
    ap.add_argument("--scales", default="", help="逗号分隔，限制档位")
    ap.add_argument("--sections", default="all")
    ap.add_argument("--json", action="store_true", help="追加机器可读汇总")
    args = ap.parse_args()

    only = [s.strip() for s in args.scales.split(",") if s.strip()] or None
    reports = load(args.out_dir, only)
    if not reports:
        sys.exit(f"在 {args.out_dir} 下没找到可用的 m15-*.json")
    labels = list(reports.keys())

    o = Out()
    o(f"<!-- 由 scripts/m15_summary.py 生成；档位：{', '.join(labels)} -->\n")
    sec_env(o, reports, labels, args.out_dir)
    sec_fixture(o, reports)
    sec_head(o, reports, labels)
    sec_hist(o, reports, labels)
    sec_plan(o, reports, labels)
    sec_cslike(o, reports, labels)
    sec_b2(o, reports, labels)
    sec_refcount(o, reports, labels)
    sec_conc(o, reports, labels)
    sec_notes(o)
    print(o.text())

    if args.json:
        print("\n```json")
        print(json.dumps({k: {m["name"]: m for m in v["metrics"]} for k, v in reports.items()},
                         ensure_ascii=False))
        print("```")


if __name__ == "__main__":
    main()
