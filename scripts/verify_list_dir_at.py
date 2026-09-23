#!/usr/bin/env python3
"""
`list_dir_at` 的差分验证：**用旧语义的参考实现在真实库上逐条比对新实现**。

为什么需要它：这条路径的重写（候选从"整棵子树"降到"直接子项"）会改变查询结构，
单测只覆盖小规模场景。这里直接拿 24 万文件的压测 fixture：

  ① 从 SQLite 库按**旧算法**（窗口函数枚举整棵子树 + 逐候选 file_at/dir_explicit_alive
     + 祖先 Tombstone 判定 + 归约成直接子项）算出参考结果；
  ② 调服务端 `GET /tree?prefix=&rev=&depth=1`（走新实现）取实际结果；
  ③ 逐条比对路径集合。

用法：
  scripts/verify_list_dir_at.py --db <b-artifact.db> --base http://127.0.0.1:18450 \\
      --user verifyadm --password 'xxx' [--revs 1,10000,19999]
"""

import argparse
import json
import sqlite3
import sys
import urllib.request


def parse_args():
    p = argparse.ArgumentParser()
    p.add_argument("--db", required=True)
    p.add_argument("--base", required=True)
    p.add_argument("--user", required=True)
    p.add_argument("--password", required=True)
    p.add_argument("--revs", default="")
    return p.parse_args()


def bounds(pfx: str):
    """与 Rust `prefix_bounds` 逐字同构：**入参必须是已归一化**（"" 或以 "/" 结尾）的前缀，
    上界 = 末字节 +1（"/" 0x2F → "0" 0x30）。"""
    if not pfx:
        return None
    b = bytearray(pfx.encode())
    b[-1] += 1
    return pfx, b.decode()


# ---------- 旧语义的参考实现 ----------

def last_change(conn, rid, path, max_rev, kind=None):
    sql = "SELECT rev, op, kind FROM changes WHERE repo_id=? AND path=? AND rev<=?"
    args = [rid, path, max_rev]
    if kind:
        sql += " AND kind=?"
        args.append(kind)
    sql += " ORDER BY rev DESC LIMIT 1"
    return conn.execute(sql, args).fetchone()


def covering_tombstone(conn, rid, path, own_rev, max_rev):
    """祖先链上是否存在晚于 own_rev、且 ≤ max_rev 的 delete-dir。"""
    parts = path.split("/")
    for i in range(1, len(parts)):
        anc = "/".join(parts[:i])
        rec = last_change(conn, rid, anc, max_rev, "dir")
        if rec and rec[1] == "delete" and rec[0] > own_rev:
            return anc
    return None


def file_at(conn, rid, path, rev):
    rec = last_change(conn, rid, path, rev, "file")
    if not rec or rec[1] == "delete":
        return None
    if covering_tombstone(conn, rid, path, rec[0], rev):
        return None
    return (rec[0], rec[2])


def dir_explicit_alive(conn, rid, path, rev):
    rec = last_change(conn, rid, path, rev, "dir")
    if not rec or rec[1] == "delete":
        return None
    if covering_tombstone(conn, rid, path, rec[0], rev):
        return None
    return rec[0]


def normalize_prefix(prefix: str) -> str:
    """与 Rust `normalize_prefix` 对齐：非空前缀补尾斜杠（`assets` → `assets/`）。

    这里踩过一次：参考实现漏了这一步，导致 `rel = path[len(prefix):]` 多切出一个 `/`，
    切出的子段变成空串，参考结果全错——差分脚本自己出错会把"实现错"误报成"实现错"，
    所以参考实现必须逐字对齐被参照的语义。
    """
    if not prefix:
        return ""
    return prefix if prefix.endswith("/") else prefix + "/"


def last_changes_under(conn, rid, pfx, max_rev):
    """旧 SQL：窗口函数取每个 path 的最后一次变更，过滤 delete。"""
    prefix = pfx
    b = bounds(pfx)
    if b:
        sql = f"""
        WITH last AS (
            SELECT path, kind, op, ROW_NUMBER() OVER (PARTITION BY path ORDER BY rev DESC) AS rn
              FROM changes WHERE repo_id = ? AND rev <= ? AND path >= ? AND path < ?
        )
        SELECT path, kind FROM last WHERE rn = 1 AND op <> 'delete'"""
        return conn.execute(sql, (rid, max_rev, b[0], b[1])).fetchall()
    sql = """
        WITH last AS (
            SELECT path, kind, op, ROW_NUMBER() OVER (PARTITION BY path ORDER BY rev DESC) AS rn
              FROM changes WHERE repo_id = ? AND rev <= ?
        )
        SELECT path, kind FROM last WHERE rn = 1 AND op <> 'delete'"""
    return conn.execute(sql, (rid, max_rev)).fetchall()


def old_list_dir_at(conn, rid, prefix, rev):
    """旧 `list_dir_at`：枚举整棵子树 → 逐候选复核 → 归约成直接子项。"""
    prefix = normalize_prefix(prefix)
    entries, dirs = [], {}
    for path, kind in last_changes_under(conn, rid, prefix, rev):
        rel = path[len(prefix):]
        if "/" in rel:
            child, rest = rel.split("/", 1)
            alive = file_at(conn, rid, path, rev) if kind == "file" else dir_explicit_alive(conn, rid, path, rev)
            if alive:
                dirs[child] = max(dirs.get(child, 0), rev)
        else:
            child = rel
            if kind == "file":
                if file_at(conn, rid, path, rev):
                    entries.append(path)
            else:
                add_rev = dir_explicit_alive(conn, rid, path, rev)
                if add_rev is not None:
                    dirs[child] = add_rev
    for name in dirs:
        entries.append(prefix + name)
    return sorted(set(entries))


# ---------- 调服务端取新实现的结果 ----------

def api(base, method, path, token=None, body=None):
    req = urllib.request.Request(base + path, method=method)
    if token:
        req.add_header("Authorization", "Bearer " + token)
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, data) as r:
        return json.loads(r.read().decode())


def new_list_dir_at(base, token, repo, prefix, rev):
    q = f"/api/v1/repos/{repo}/tree?prefix={urllib.parse.quote(prefix)}&depth=1"
    if rev:
        q += f"&rev={rev}"
    items = api(base, "GET", q, token)["items"]
    return sorted(e["path"] for e in items)


def main():
    args = parse_args()
    conn = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)
    conn.execute("PRAGMA case_sensitive_like=ON")
    rid, head, repo = conn.execute("SELECT id, head_rev, name FROM repos LIMIT 1").fetchone()
    print(f"repo={repo} rid={rid} head_rev={head}")

    token = api(args.base, "POST", "/api/v1/auth/login", body={"username": args.user, "password": args.password})["token"]

    revs = [int(x) for x in args.revs.split(",")] if args.revs else [head, head - 1, head // 2, max(1, head // 10), 1]
    prefixes = ["", "assets", "assets/0000", "assets/0000/0000", "assets/0000/0000/000", "nope"]

    bad = 0
    total = 0
    for rev in revs:
        for prefix in prefixes:
            total += 1
            # 旧实现有个瑕疵：当"前缀自身"也是候选时（目录名出现在 changes 里），
            # 它会把目录自己列出来。新实现只列直接子项 —— 这是修正，不是回归，
            # 所以参考结果里剔除它，并单独断言新结果不含前缀自身。
            expect = [p for p in old_list_dir_at(conn, rid, prefix, rev) if p != prefix]
            got = new_list_dir_at(args.base, token, repo, prefix, rev)
            if prefix and prefix in got:
                print(f"  ✗ rev={rev} prefix={prefix!r} 新实现把前缀自身也列出来了")
                bad += 1
            same = expect == got
            if not same:
                bad += 1
            mark = "✓" if same else "✗"
            head_rev_note = " (HEAD)" if rev >= head else ""
            print(f"  {mark} rev={rev:<6}{head_rev_note:8s} prefix={prefix!r:26s} 旧 {len(expect):>6} 条 / 新 {len(got):>6} 条")
            if not same:
                only_old = sorted(set(expect) - set(got))[:5]
                only_new = sorted(set(got) - set(expect))[:5]
                print(f"      只在旧: {only_old}")
                print(f"      只在新: {only_new}")

    print(f"\n---- 差分结果：{total - bad} / {total} 组完全一致 ----")
    return 1 if bad else 0


if __name__ == "__main__":
    import urllib.parse  # noqa: E402

    sys.exit(main())
