#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""M1.5 查询计划证据采集：打印 Path-History 关键查询的 `EXPLAIN QUERY PLAN`。

这是 docs/压测报告-M1.5.md §4 的证据来源。判定口径特意区分两件事：

  - **用没用索引**（`SEARCH` vs `SCAN`）
  - **该用的列有没有把范围界住**（每条查询声明自己的"关键列"）

只要关键列没出现在 SEARCH 条件里，就等价于"顺着索引把该仓库的全部行扫一遍"，
这正是 B1（前缀列 `path` 未下推）与 B5（`blob_hash` 无索引）的共同形态。

M2 开工后建议把本脚本接进 CI，用 `--assert` 做回归断言。

用法：
  python3 scripts/plan_evidence.py /tmp/b-artifact-fixtures/full
  python3 scripts/plan_evidence.py /tmp/b-artifact-fixtures/full --demo-parent
  python3 scripts/plan_evidence.py /tmp/b-artifact-fixtures/full --assert
"""

from __future__ import annotations

import argparse
import sqlite3
import sys
from pathlib import Path

# (表, SQL, 绑定键顺序, 该查询"应该界住范围"的列)
PROD = {
    "changes 前缀（last_changes_under，窗口函数）": (
        "changes",
        """
        WITH last AS (
            SELECT path, kind, op,
                   ROW_NUMBER() OVER (PARTITION BY path ORDER BY rev DESC) AS rn
              FROM changes
             WHERE repo_id = ?1 AND rev <= ?2 AND path LIKE ?3 ESCAPE '\\'
        )
        SELECT path, kind FROM last WHERE rn = 1 AND op <> 'delete'
        """,
        ("rid", "rev", "like"),
        ("path",),
    ),
    "head_entries 前缀（list_dir_head）": (
        "head_entries",
        """
        SELECT path, kind, blob_hash, size, mode, mtime, changed_rev
          FROM head_entries
         WHERE repo_id = ?1 AND path LIKE ?2 ESCAPE '\\'
        """,
        ("rid", "like"),
        ("path",),
    ),
    "byte_delta 反查（apply_commit_tx）": (
        "changes",
        """
        SELECT 1 FROM changes WHERE repo_id = ?1 AND blob_hash = ?2 LIMIT 1
        """,
        ("rid", "blob"),
        ("blob_hash",),
    ),
    "路径自身最近变更（last_change）": (
        "changes",
        """
        SELECT rev, op, kind, blob_hash, size, mode, mtime FROM changes
         WHERE repo_id = ?1 AND path = ?2 AND rev <= ?3
         ORDER BY rev DESC LIMIT 1
        """,
        ("rid", "path", "rev"),
        ("path",),
    ),
    "revision 列表（log 分页）": (
        "revisions",
        """
        SELECT rev, author_id, message, created_at, file_count, byte_delta, manifest_hash
          FROM revisions WHERE repo_id = ?1 AND rev <= ?2 ORDER BY rev DESC LIMIT ?3
        """,
        ("rid", "rev", "limit"),
        ("rev",),
    ),
}

REWRITE = {
    "changes 前缀（last_changes_under，窗口函数）": (
        "changes",
        """
        WITH last AS (
            SELECT path, kind, op,
                   ROW_NUMBER() OVER (PARTITION BY path ORDER BY rev DESC) AS rn
              FROM changes
             WHERE repo_id = ?1 AND rev <= ?2 AND path GLOB ?3
        )
        SELECT path, kind FROM last WHERE rn = 1 AND op <> 'delete'
        """,
        ("rid", "rev", "glob"),
        ("path",),
    ),
    "head_entries 前缀（list_dir_head）": (
        "head_entries",
        """
        SELECT path, kind, blob_hash, size, mode, mtime, changed_rev
          FROM head_entries
         WHERE repo_id = ?1 AND path GLOB ?2
        """,
        ("rid", "glob"),
        ("path",),
    ),
}

PARENT_DEMO_ROWS = 200_000
PARENT_SQL = """
    SELECT path, kind, blob_hash, size, mode, mtime, changed_rev
      FROM head_demo
     WHERE repo_id = ?1 AND parent_path = ?2
"""


def like_prefix(pfx: str) -> str:
    """与 repo.rs::like_prefix 同语义。"""
    esc = pfx.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")
    return esc + "%"


def glob_prefix(pfx: str) -> str:
    """GLOB 语义：转义 * ? [ ]，再补一个 `*`。"""
    esc = pfx.replace("[", "[[]").replace("*", "[*]").replace("?", "[?]")
    return esc + "*"


def plan(conn: sqlite3.Connection, sql: str, args: tuple) -> list[str]:
    return [str(r[-1]) for r in conn.execute("EXPLAIN QUERY PLAN " + sql, args).fetchall()]


def verdict(lines: list[str], table: str, key_cols: tuple[str, ...]) -> tuple[str, str]:
    """返回 (标记, 说明)。key_cols = 该查询本应界住范围的列。"""
    hits = [l for l in lines if f" {table}" in l]
    if not hits:
        return "—", "未触及该表"
    if any(l.strip() == f"SCAN {table}" for l in hits):
        return "❌", f"**全表扫描** {table}"
    searches = [l for l in hits if "SEARCH" in l]
    if not searches:
        return "❓", "计划形态未识别"
    bounded = any(any(c in l for c in key_cols) for l in searches)
    if bounded:
        tmp = "，但仍有临时排序" if any("TEMP B-TREE" in l for l in lines) else "，无临时排序"
        return "✅", f"索引范围扫描，关键列 {'/'.join(key_cols)} 有界{tmp}"
    return "⚠️", f"**用了索引但关键列 {'/'.join(key_cols)} 无界**（= 整仓扫描）"


def show(title: str, lines: list[str]) -> None:
    print(f"\n**{title}**\n")
    print("```")
    for l in lines or ["(无输出)"]:
        print("  " + l)
    print("```")


def build_parent_demo(conn: sqlite3.Connection, rid: int, n_rows: int) -> int:
    """用临时表真实复现 parent_path 方案（只取前 n_rows 行，足够体现计划形态）。"""
    conn.execute("CREATE TEMP TABLE head_demo AS "
                 "SELECT repo_id, path, kind, blob_hash, size, mode, mtime, changed_rev, '' AS parent_path "
                 "  FROM main.head_entries WHERE repo_id = ? LIMIT ?", (rid, n_rows))
    conn.execute("CREATE INDEX idx_head_demo_parent ON head_demo(repo_id, parent_path)")
    rows = conn.execute("SELECT rowid, path FROM head_demo").fetchall()
    conn.executemany(
        "UPDATE head_demo SET parent_path = ? WHERE rowid = ?",
        ((p.rsplit("/", 1)[0] if "/" in p else "", r) for r, p in rows),
    )
    conn.execute("ANALYZE head_demo")
    return len(rows)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("fixture", nargs="?", help="fixture 目录（含 b-artifact.db）")
    ap.add_argument("--db", help="直接给出 db 路径")
    ap.add_argument("--repo", type=int, default=1)
    ap.add_argument("--demo-parent", action="store_true",
                    help="用临时表在真实数据上验证 parent_path 提案的计划形态")
    ap.add_argument("--assert", dest="do_assert", action="store_true", help="回归断言模式（不达标则退出码 1）")
    args = ap.parse_args()

    db = Path(args.db) if args.db else Path(args.fixture or "") / "b-artifact.db"
    if not db.exists():
        sys.exit(f"找不到 {db}")

    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    conn.execute("PRAGMA case_sensitive_like=OFF")  # 现状：db::open() 未设置该 pragma

    rid = args.repo
    head_rev = conn.execute("SELECT head_rev FROM repos WHERE id=?", (rid,)).fetchone()[0]
    n_ch = conn.execute("SELECT COUNT(*) FROM changes WHERE repo_id=?", (rid,)).fetchone()[0]
    n_he = conn.execute("SELECT COUNT(*) FROM head_entries WHERE repo_id=?", (rid,)).fetchone()[0]
    n_rev = conn.execute("SELECT COUNT(*) FROM revisions WHERE repo_id=?", (rid,)).fetchone()[0]
    blob = conn.execute(
        "SELECT blob_hash FROM changes WHERE repo_id=? AND blob_hash IS NOT NULL LIMIT 1", (rid,)
    ).fetchone()[0]
    path = conn.execute(
        "SELECT path FROM head_entries WHERE repo_id=? AND path LIKE 'assets/0000/0000/000/%' LIMIT 1",
        (rid,),
    ).fetchone()[0]
    leaf = path.rsplit("/", 1)[0]
    sample_dir = leaf.rsplit("/", 1)[0]

    binds = {
        "rid": rid,
        "rev": head_rev,
        "like": like_prefix(leaf + "/"),
        "glob": glob_prefix(leaf + "/"),
        "path": path,
        "limit": 50,
        "blob": blob,
    }
    scale = {"changes": n_ch, "head_entries": n_he, "revisions": n_rev}

    print("# Path-History 查询计划证据\n")
    print(f"- fixture：`{db}`")
    print(f"- 规模：`changes` **{n_ch:,}** 行 / `head_entries` **{n_he:,}** 行 / "
          f"`revisions` **{n_rev:,}** 行 / `head_rev` = {head_rev}")
    print("- `PRAGMA case_sensitive_like` = **OFF**（与 `db::open()` 现状一致）")
    print(f"- 样本前缀：末级目录 `{leaf}/`")

    # ---------- 1. 生产写法 ----------
    print("\n## 1. 生产写法（现状）\n")
    print("| 查询 | 数据规模 | 判定 |")
    print("|---|---|---|")
    results: dict[str, tuple[str, list[str], tuple[str, ...]]] = {}
    for name, (tbl, sql, keys, keycols) in PROD.items():
        pl = plan(conn, sql, tuple(binds[k] for k in keys))
        results[name] = (tbl, pl, keycols)
        mark, desc = verdict(pl, tbl, keycols)
        print(f"| {name} | `{tbl}` {scale[tbl]:,} 行 | {mark} {desc} |")
    for name, (tbl, pl, _) in results.items():
        show(f"{name} —— 生产写法", pl)
    print("\n> `SEARCH changes USING INDEX idx_changes_rev (repo_id=?)` 读作："
          "**只按 `repo_id` 界了范围**，等于顺着索引把该仓库的**全部**历史行读一遍；"
          "`USE TEMP B-TREE FOR ORDER BY` 说明窗口函数还要额外排一次序。\n")

    # ---------- 2. GLOB 重写 ----------
    print("\n## 2. 等价重写：前缀 `LIKE` → `GLOB`\n")
    print("| 查询 | 生产（LIKE） | 重写（GLOB） |")
    print("|---|---|---|")
    for name, (tbl, sql, keys, keycols) in REWRITE.items():
        pl = plan(conn, sql, tuple(binds[k] for k in keys))
        prod_pl = results[name][1]
        print(f"| {name} | {verdict(prod_pl, tbl, keycols)[1]} "
              f"| {verdict(pl, tbl, keycols)[1]} |")
        show(f"{name} —— GLOB 重写", pl)
    print("\n> GLOB 版本走 `idx_changes_path (repo_id=? AND path>? AND path<?)`，"
          "**前缀被下推成索引范围扫描**；又因为索引序本身就是 `path` 序，"
          "`USE TEMP B-TREE FOR ORDER BY` 一并消失。\n")

    # ---------- 3. parent_path 提案 ----------
    print("\n## 3. B4 提案：`head_entries` 增加 `parent_path`\n")
    print("现状下 `list_dir_head` 的语义是「返回整棵子树的全部条目」，"
          "再由 API 层在 Rust 里按 `depth` 过滤 —— 于是**列一个目录的代价 = 该子树的行数**，"
          "列仓库根目录就要把 100 万行读出来才能返回 100 个子项。\n")
    print("```sql")
    print("ALTER TABLE head_entries ADD COLUMN parent_path TEXT NOT NULL DEFAULT '';")
    print("CREATE INDEX idx_head_parent ON head_entries(repo_id, parent_path);")
    print("```")
    if args.demo_parent:
        n = build_parent_demo(conn, rid, PARENT_DEMO_ROWS)
        pl = plan(conn, PARENT_SQL, (rid, sample_dir))
        mark, desc = verdict(pl, "head_demo", ("parent_path",))
        print(f"\n实测：临时表 `head_demo`（{n:,} 行）+ `idx_head_demo_parent` → {mark} {desc}\n")
        show("按 `parent_path` 点查（列 `" + sample_dir + "`）", pl)
        print("\n> 代价变成 `O(直接子项数)`，与子树规模、与全仓规模都无关。"
              "`changes` 侧同理（`last_changes_under` 也可按 `parent_path` 收窄）。\n")
    else:
        print("\n> 加 `--demo-parent` 会用临时表在真实数据上验证这条计划。")

    # ---------- 4. 汇总 ----------
    bad = [n for n, (tbl, pl, kc) in results.items() if verdict(pl, tbl, kc)[0] in ("❌", "⚠️")]
    print("\n## 4. 判定汇总\n")
    if bad:
        print(f"**{len(bad)} / {len(PROD)}** 条关键查询没有把关键列下推到索引：\n")
        for n in bad:
            tbl, pl, kc = results[n]
            print(f"- `{n}` —— {verdict(pl, tbl, kc)[1]}")
    else:
        print("全部关键查询的关键列均已下推到索引。")
    conn.close()

    if args.do_assert and bad:
        print(f"\n**断言失败**：{len(bad)} 条查询范围无界（即报告 B1 / B5）。")
        sys.exit(1)
    if args.do_assert:
        print("\n**断言通过**。")


if __name__ == "__main__":
    main()
