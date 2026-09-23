#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""b-artifact 造数脚本（M1.5 压测，方案 §12.7 / §14）。

直接生成一个「自洽的大仓库」fixture：既落 SQLite 元数据库，也落真实的 blob 文件，
让服务端启动后不需要任何修复即可对外服务。同一个脚本既服务压测（大参数），
也服务功能测试（小参数跑同一条代码路径）。

数据模型（与 §3.3 Path-History 完全一致）：
  - revisions        每个修订一行
  - changes          只记"变更"增量（导入期 add，维护期 modify/add/delete）
  - head_entries     最终 HEAD 的当前树
  - blobs            blob 索引 + 磁盘文件（blobs/xx/yy/<hash>）
  - repos.head_rev   与 revisions 最大值一致

路径布局（三段式，便于"浅层大目录"与"深层小目录"两种查询对比）：
  assets/{p:04d}/{m:04d}/{l:03d}/f{k:02d}.bin

用法示例：
  # 档位预设
  python3 scripts/gen_fixture.py --scale xs --data-dir /tmp/fx-xs
  python3 scripts/gen_fixture.py --scale full --data-dir /tmp/fx-full

  # 完全自定义
  python3 scripts/gen_fixture.py --data-dir /tmp/fx \\
      --top 10 --mid 10 --leaf 5 --files-per-dir 20 --revs 500

产出摘要以 JSON 打到 stdout（供编排脚本消费），进度打到 stderr。
"""

from __future__ import annotations

import argparse
import concurrent.futures as cf
import hashlib
import json
import os
import random
import sqlite3
import sys
import threading
import time
from pathlib import Path

# ---------------- 档位预设 ----------------
# 文件总数 N = top * mid * leaf * files_per_dir
PRESETS = {
    #          top   mid   leaf  files_per_dir  revs       blob_pool  blob_bytes
    "xs":   dict(top=5,   mid=5,   leaf=5,   files_per_dir=8,   revs=200,     blob_pool=800,   blob_bytes=1024),
    "s":    dict(top=20,  mid=20,  leaf=10,  files_per_dir=10,  revs=1_000,   blob_pool=2_000, blob_bytes=1536),
    "m":    dict(top=40,  mid=40,  leaf=15,  files_per_dir=10,  revs=20_000,  blob_pool=3_000, blob_bytes=2048),
    "full": dict(top=100, mid=100, leaf=10,  files_per_dir=10,  revs=100_000, blob_pool=4_000, blob_bytes=2048),
}


def human(n: float) -> str:
    n = float(n)
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if abs(n) < 1024:
            return f"{n:.1f}{unit}"
        n /= 1024
    return f"{n:.1f}PB"


def dir_size(path: Path) -> int:
    total = 0
    for root, _dirs, files in os.walk(path):
        for f in files:
            try:
                total += os.path.getsize(os.path.join(root, f))
            except OSError:
                pass
    return total


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


# ---------------- 建库（复用 server/migrations 作为唯一 schema 真源） ----------------

def open_db(db_path: Path, migrations_dir: Path) -> sqlite3.Connection:
    db_path.parent.mkdir(parents=True, exist_ok=True)
    for suffix in ("", "-wal", "-shm"):
        p = Path(str(db_path) + suffix)
        if p.exists():
            p.unlink()

    conn = sqlite3.connect(str(db_path))
    conn.isolation_level = None  # 手动管理事务（造数期按 batch_revs 提交）
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=OFF")  # 造数期关闭 fsync
    conn.execute("PRAGMA foreign_keys=ON")

    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations ("
        " version TEXT PRIMARY KEY, applied_at TEXT NOT NULL)"
    )
    now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    sql_files = sorted(p for p in migrations_dir.glob("*.sql"))
    if not sql_files:
        raise SystemExit(f"未找到迁移脚本: {migrations_dir}")
    for sql_file in sql_files:
        conn.executescript(sql_file.read_text(encoding="utf-8"))
        conn.execute(
            "INSERT OR REPLACE INTO schema_migrations (version, applied_at) VALUES (?, ?)",
            (sql_file.stem, now),
        )
    return conn


# ---------------- 主体 ----------------

def generate(args: argparse.Namespace) -> dict:
    data_dir = Path(args.data_dir)
    data_dir.mkdir(parents=True, exist_ok=True)
    repo_root = Path(__file__).resolve().parent.parent
    migrations_dir = repo_root / "server" / "migrations"

    db_path = data_dir / "b-artifact.db"
    blobs_dir = data_dir / "blobs"
    for sub in ("tmp", "uploads", "logs"):
        (data_dir / sub).mkdir(exist_ok=True)

    rng = random.Random(args.seed)
    now = time.time()
    iso = lambda ts: time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))

    # ---- 1. blob 池：造内容 + 落盘 ----
    # 注意：某些受限环境对 mkdir/open 有每调用固定的代理开销（本机实测约 9~14ms/次），
    # 所以落盘用线程池并行，且 blob 池规模按"够跑下载基准"设定而非无节制放大。
    t0 = time.time()
    base = rng.randbytes(max(64, int(rng.gauss(args.blob_bytes, args.blob_bytes * 0.2))))
    pool: list[tuple[str, int]] = []      # (hash, size)
    blob_data: list[bytes] = []
    for i in range(args.blob_pool):
        blob = base + i.to_bytes(4, "big")  # 尾部带序号 → 天然互不相同
        pool.append((hashlib.sha256(blob).hexdigest(), len(blob)))
        blob_data.append(blob)
    sizes = dict(pool)
    hash_secs = time.time() - t0

    t0 = time.time()
    made_dirs: set[str] = set()
    dir_lock = threading.Lock()

    def materialize(i: int) -> None:
        h = pool[i][0]
        d = blobs_dir / h[:2] / h[2:4]
        ds = str(d)
        with dir_lock:                     # 同一分片目录只 mkdir 一次（mkdir 是慢调用）
            if ds not in made_dirs:
                d.mkdir(parents=True, exist_ok=True)
                made_dirs.add(ds)
        with open(d / h, "wb") as f:
            f.write(blob_data[i])

    with cf.ThreadPoolExecutor(max_workers=args.blob_threads) as ex:
        list(ex.map(materialize, range(len(pool))))
    blob_secs = time.time() - t0
    log(
        f"[1/5] blob 池 {len(pool)} 个 / {human(sum(s for _, s in pool))} /"
        f" 哈希 {hash_secs:.1f}s + 落盘 {blob_secs:.1f}s（{args.blob_threads} 线程）"
    )

    conn = open_db(db_path, migrations_dir)
    cur = conn.cursor()

    # ---- 2. 用户 / 仓库骨架 ----
    cur.execute(
        "INSERT INTO users (id, username, display_name, email, password_hash, source, is_admin,"
        " disabled, created_at) VALUES (1, 'bench', 'bench', '', NULL, 'local', 1, 0, ?)",
        (iso(now),),
    )
    cur.execute(
        "INSERT INTO repos (id, name, description, head_rev, owner_id, lock_policy, needs_lock,"
        " allow_anon, created_at) VALUES (1, ?, ?, 0, 1, ?, '', 0, ?)",
        (args.repo, "M1.5 压测仓库", "strict", iso(now)),  # 锁策略列已废弃，恒写 strict（v0.4.17）
    )
    cur.execute(  # 建仓基线 ACL（§4.2 要点 4）
        "INSERT INTO acl_rules (repo_id, path_prefix, subject_type, subject_id, level, inherit,"
        " created_at) VALUES (1, '', 'everyone', 0, 'read', 1, ?)",
        (iso(now),),
    )

    # ---- 3. 枚举全部路径 ----
    t0 = time.time()
    paths: list[str] = []
    for p in range(args.top):
        for m in range(args.mid):
            for l in range(args.leaf):
                prefix = f"assets/{p:04d}/{m:04d}/{l:03d}/"
                paths.extend(f"{prefix}f{k:02d}.bin" for k in range(args.files_per_dir))
    total_files = len(paths)
    log(f"[2/5] 路径枚举 {total_files} 条 / {time.time() - t0:.1f}s")

    # ---- 4. 生成历史（revisions + changes） ----
    total_revs = args.revs
    import_revs = max(1, min(total_revs, -(-total_files // max(1, args.import_batch))))
    mods = args.mods_per_rev
    n_add_per = int(mods * args.add_ratio)
    n_del_per = int(mods * args.del_ratio)

    log(
        f"[3/5] 生成历史：{total_revs} 修订（导入期 {import_revs} × 约 {args.import_batch} 文件；"
        f"维护期 {total_revs - import_revs} × {mods} 改 + {n_add_per} 增 + {n_del_per} 删）"
    )

    # 存活集合用「稠密列表 + 索引表 + 内容表」三件套，删改都是 O(1)（swap-remove），
    # 避免每修订复制整表导致大档位不可跑。
    alive_paths: list[str] = []
    alive_pos: dict[str, int] = {}
    alive_hash: dict[str, str] = {}
    referenced: set[str] = set()   # 本仓库已引用过的 blob（byte_delta 口径）
    new_seq = 0                    # 维护期新增文件序号，保证路径不撞

    pop = len(pool)
    if pop == 0:
        raise SystemExit("blob_pool 必须 >= 1")
    pick = lambda: pool[rng.randrange(pop)][0]

    cursor = 0
    rev_rows: list[tuple] = []
    change_buf: list[tuple] = []
    t0 = time.time()
    conn.execute("BEGIN")
    for rev in range(1, total_revs + 1):
        cbuf: list[tuple] = []
        byte_delta = 0

        def note(h: str) -> None:
            nonlocal byte_delta
            if h not in referenced:
                referenced.add(h)
                byte_delta += sizes[h]

        if rev <= import_revs:
            n = min(args.import_batch, total_files - cursor)
            for i in range(cursor, cursor + n):
                path = paths[i]
                h = pick()
                cbuf.append((rev, path, "add", "file", h, sizes[h], 0o644, 0))
                alive_pos[path] = len(alive_paths)
                alive_paths.append(path)
                alive_hash[path] = h
                note(h)
            cursor += n
        else:
            nlive = len(alive_paths)
            # 修改
            if nlive:
                for idx in rng.sample(range(nlive), min(mods, nlive)):
                    path = alive_paths[idx]
                    h = pick()
                    cbuf.append((rev, path, "modify", "file", h, sizes[h], 0o644, 0))
                    alive_hash[path] = h
                    note(h)
            # 新增
            for _ in range(n_add_per):
                path = (
                    f"assets/{rng.randrange(args.top):04d}/{rng.randrange(args.mid):04d}/"
                    f"{rng.randrange(args.leaf):03d}/n{new_seq:05d}.bin"
                )
                new_seq += 1
                h = pick()
                cbuf.append((rev, path, "add", "file", h, sizes[h], 0o644, 0))
                alive_pos[path] = len(alive_paths)
                alive_paths.append(path)
                alive_hash[path] = h
                note(h)
            # 删除（swap-remove 保持稠密）
            if n_del_per and nlive > n_del_per:
                dead = sorted(rng.sample(range(nlive), n_del_per), reverse=True)
                for idx in dead:
                    path = alive_paths[idx]
                    last = alive_paths.pop()
                    if idx < len(alive_paths):
                        alive_paths[idx] = last
                        alive_pos[last] = idx
                    alive_pos.pop(path, None)
                    alive_hash.pop(path, None)
                    cbuf.append((rev, path, "delete", "file", None, 0, 0o644, 0))

        rev_rows.append((1, rev, 1, f"rev {rev}", iso(now - (total_revs - rev) * 60),
                         len(cbuf), byte_delta, ""))
        change_buf.extend(cbuf)

        if rev % args.batch_revs == 0 or rev == total_revs:
            cur.executemany(
                "INSERT INTO revisions (repo_id, rev, author_id, message, created_at,"
                " file_count, byte_delta, manifest_hash) VALUES (?,?,?,?,?,?,?,?)",
                rev_rows,
            )
            cur.executemany(
                "INSERT INTO changes (repo_id, rev, path, op, kind, blob_hash, size, mode, mtime)"
                " VALUES (1,?,?,?,?,?,?,?,?)",
                change_buf,
            )
            conn.commit()
            conn.execute("BEGIN")
            rev_rows.clear()
            change_buf.clear()
            if args.progress_every and rev % args.progress_every == 0:
                log(f"      rev {rev}/{total_revs}（{time.time() - t0:.0f}s）")
    history_secs = time.time() - t0
    log(f"[3/5] 历史生成完成 / {history_secs:.1f}s")

    # ---- 5. head_entries / blobs / repos.head_rev ----
    t0 = time.time()
    # 与服务端 apply_commit 的不变量保持一致：
    #   - 每个文件行带 parent_path
    #   - 全部目录（含文件隐式产生的父目录）物化为 kind='dir' 行（is_explicit=0）
    # 否则列目录无法走 parent_path 等值查询（M1.5 B4）。
    file_rows = []
    dir_parents: dict[str, str] = {}
    for p in alive_paths:
        parent = p.rsplit("/", 1)[0] if "/" in p else ""
        file_rows.append(
            (p, alive_hash[p], sizes[alive_hash[p]], 0o644, total_revs, parent)
        )
        parts = p.split("/")[:-1]
        for i in range(1, len(parts) + 1):
            d = "/".join(parts[:i])
            dir_parents.setdefault(d, d.rsplit("/", 1)[0] if "/" in d else "")
    cur.executemany(
        "INSERT INTO head_entries"
        " (repo_id, path, kind, blob_hash, size, mode, mtime, changed_rev, parent_path, is_explicit)"
        " VALUES (1, ?, 'file', ?, ?, ?, 0, ?, ?, 0)",
        file_rows,
    )
    cur.executemany(
        "INSERT OR IGNORE INTO head_entries"
        " (repo_id, path, kind, blob_hash, size, mode, mtime, changed_rev, parent_path, is_explicit)"
        " VALUES (1, ?, 'dir', NULL, 0, 0, 0, ?, ?, 0)",
        [(d, total_revs, par) for d, par in dir_parents.items()],
    )
    conn.commit()

    cur.executemany(
        "INSERT OR IGNORE INTO blobs (hash, size, stored_size, codec, refcount, created_at)"
        " VALUES (?, ?, ?, 'raw', 0, ?)",
        [(h, s, s, iso(now)) for h, s in pool],
    )
    # refcount 口径：引用该 blob 的 file-change 行数（= commit.rs 的 increment 语义）。
    # 注意必须用 GROUP BY 聚合，不要写成 `SET refcount = (SELECT COUNT(*) ... WHERE
    # c.blob_hash = blobs.hash)` 这种逐行相关子查询——它的代价是 O(blobs × changes)，
    # 在 full 档位（4000 blob × 200 万 change）下等于永远跑不完。
    # 实测证据见 docs/压测报告-M1.5.md 的「瓶颈 B3」。
    cur.execute(
        "UPDATE blobs SET refcount = t.c FROM ("
        "  SELECT blob_hash AS h, COUNT(*) AS c FROM changes"
        "   WHERE blob_hash IS NOT NULL GROUP BY blob_hash) t"
        " WHERE blobs.hash = t.h"
    )
    cur.execute("UPDATE repos SET head_rev = ? WHERE id = 1", (total_revs,))
    conn.commit()

    if args.analyze:
        cur.execute("ANALYZE")
        conn.commit()
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    conn.commit()

    stats = {
        "rows_changes": cur.execute("SELECT COUNT(*) FROM changes").fetchone()[0],
        "rows_head": cur.execute("SELECT COUNT(*) FROM head_entries").fetchone()[0],
        "rows_revs": cur.execute("SELECT COUNT(*) FROM revisions").fetchone()[0],
        "rows_blobs": cur.execute("SELECT COUNT(*) FROM blobs").fetchone()[0],
        "live_files": len(alive_paths),
    }
    conn.close()
    finish_secs = time.time() - t0

    db_size = db_path.stat().st_size
    blob_size = dir_size(blobs_dir)
    log(
        f"[4/5] 收尾 / {finish_secs:.1f}s；[5/5] changes={stats['rows_changes']}"
        f" head_entries={stats['rows_head']} revisions={stats['rows_revs']}"
        f" blobs={stats['rows_blobs']}"
    )
    log(f"      DB {human(db_size)} + blobs {human(blob_size)}")

    return {
        "data_dir": str(data_dir),
        "repo": args.repo,
        "scale": args.scale,
        "seed": args.seed,
        "top": args.top,
        "mid": args.mid,
        "leaf": args.leaf,
        "files_per_dir": args.files_per_dir,
        "total_files": total_files,
        "revs": total_revs,
        "import_revs": import_revs,
        "import_batch": args.import_batch,
        "mods_per_rev": mods,
        "blob_pool": len(pool),
        "blob_bytes": args.blob_bytes,
        "analyze": args.analyze,
        "db_bytes": db_size,
        "blob_bytes_total": blob_size,
        "gen_seconds": round(blob_secs + history_secs + finish_secs, 1),
        **stats,
    }


def main() -> None:
    ap = argparse.ArgumentParser(description="b-artifact 压测造数脚本")
    ap.add_argument("--scale", choices=sorted(PRESETS), default=None,
                    help="档位预设；同时给出显式参数时以显式参数为准")
    ap.add_argument("--data-dir", required=True, help="数据目录（会被重建）")
    ap.add_argument("--repo", default="bench", help="仓库名（默认 bench）")
    # v0.4.17：锁策略唯一（先锁后提交），这个参数只为兼容旧调用而保留
    ap.add_argument("--lock-policy", default="strict", choices=["advisory", "strict"])

    ap.add_argument("--top", type=int, default=None, help="第一级目录数")
    ap.add_argument("--mid", type=int, default=None, help="第二级目录数")
    ap.add_argument("--leaf", type=int, default=None, help="第三级目录数")
    ap.add_argument("--files-per-dir", type=int, default=None, help="每个末级目录的文件数")
    ap.add_argument("--revs", type=int, default=None, help="总修订数")
    ap.add_argument("--blob-pool", type=int, default=None, help="互不相同的 blob 个数（去重池）")
    ap.add_argument("--blob-bytes", type=int, default=None, help="blob 平均原始字节数")
    ap.add_argument("--blob-threads", type=int, default=8, help="blob 落盘线程数")

    ap.add_argument("--import-batch", type=int, default=2000,
                    help="导入期每个修订铺开的文件数（默认 2000）")
    ap.add_argument("--mods-per-rev", type=int, default=10, help="维护期每修订修改文件数")
    ap.add_argument("--add-ratio", type=float, default=0.02, help="维护期新增数 = mods*该比例")
    ap.add_argument("--del-ratio", type=float, default=0.01, help="维护期删除数 = mods*该比例")
    ap.add_argument("--batch-revs", type=int, default=500, help="每多少个修订提交一次事务")
    ap.add_argument("--progress-every", type=int, default=5000, help="每多少修订打印一次进度")
    ap.add_argument("--analyze", action="store_true",
                    help="生成后执行 ANALYZE（默认关闭，以贴近 M1 服务端真实行为）")
    ap.add_argument("--seed", type=int, default=20260914)
    args = ap.parse_args()

    if args.scale:
        for k, v in PRESETS[args.scale].items():
            if getattr(args, k) is None:
                setattr(args, k, v)
    for k, v in PRESETS["xs"].items():   # 未给档位也未给显式参数时的兜底
        if getattr(args, k) is None:
            setattr(args, k, v)

    summary = generate(args)
    print(json.dumps(summary, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
