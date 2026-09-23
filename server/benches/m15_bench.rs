//! M1.5 压测 harness（方案 §12.7「造数脚本复用」/ §14「M1.5 性能压测」）。
//!
//! 目的：在大仓库 fixture（`scripts/gen_fixture.py` 生成）上量清楚四件事，
//! 并据此产出瓶颈清单：
//!   1. **环境底噪**：本机文件 open+read 与 SQLite 点查的固定开销——用来把
//!      「环境开销」从被测代码耗时里剥离（受限环境里 open/mkdir 可能有重代理开销）；
//!   2. **存储层核心路径**：Path-History 查询、列目录、提交事务、manifest_hash、
//!      refcount 全量重建——直接调库，排除 HTTP 与序列化干扰；
//!   3. **HTTP 端到端**：进程内起真实 axum（随机端口）+ reqwest 客户端，
//!      量包含鉴权/路由/序列化在内的真实延迟；
//!   4. **并发**：多并发档位下的吞吐与尾延迟（观察 `Mutex<Connection>` 串行化）。
//!
//! 用法（必须在 bench/release profile 下跑，debug 数字没有意义）：
//!   cargo bench --bench m15_bench -- --data-dir /tmp/ba-fx-full --out /tmp/m15-full.json

use b_artifact_server::api;
use b_artifact_server::auth::password;
use b_artifact_server::config::Config;
use b_artifact_server::state::{AppState, SharedState};
use b_artifact_server::storage::blob::BlobStore;
use b_artifact_server::storage::db;
use b_artifact_server::storage::pool::DbPool;
use b_artifact_server::storage::repo::{self, Change};
use rusqlite::params;
use serde::Serialize;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Instant;

// ============ 参数 ============

struct Args {
    data_dir: PathBuf,
    out: Option<PathBuf>,
    iterations: usize,
    warmup: usize,
    concurrency: Vec<usize>,
    only: Vec<String>,
    password: String,
    label: String,
    repo: String,
    /// 单个基准的采样预算（秒）：超预算即停止采样。大档位下防止 O(历史) 的
    /// 基准把整轮压测拖成几小时——**"跑不完"本身就是瓶颈证据**，会在报告里标注。
    budget_secs: f64,
    /// 是否运行不受控的重基准（根目录历史列目录等）。`auto` = 存活文件 > 20 万时自动跳过。
    heavy: String,
    /// changes 前缀扫描的 LIMIT 扫描点（0 表示不限）。
    sweep: Vec<i64>,
}

fn next_val(it: &mut impl Iterator<Item = String>, key: &str) -> String {
    it.next().unwrap_or_else(|| panic!("{key} 缺少取值"))
}

fn parse_args() -> Args {
    let mut a = Args {
        data_dir: PathBuf::new(),
        out: None,
        iterations: 40,
        warmup: 5,
        concurrency: vec![8, 32],
        only: vec!["env".into(), "storage".into(), "sweep".into(), "http".into(), "conc".into()],
        password: "bench-pass-1234".into(),
        label: String::new(),
        repo: "bench".into(),
        budget_secs: 15.0,
        heavy: "auto".into(),
        sweep: vec![1_000, 10_000, 100_000, 1_000_000, 0],
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--data-dir" => a.data_dir = PathBuf::from(next_val(&mut it, &k)),
            "--out" => a.out = Some(PathBuf::from(next_val(&mut it, &k))),
            "--iterations" => a.iterations = next_val(&mut it, &k).parse().expect("iterations"),
            "--warmup" => a.warmup = next_val(&mut it, &k).parse().expect("warmup"),
            "--concurrency" => {
                a.concurrency = next_val(&mut it, &k).split(',').filter_map(|s| s.parse().ok()).collect()
            }
            "--only" => a.only = next_val(&mut it, &k).split(',').map(|s| s.to_string()).collect(),
            "--password" => a.password = next_val(&mut it, &k),
            "--label" => a.label = next_val(&mut it, &k),
            "--repo" => a.repo = next_val(&mut it, &k),
            "--budget" => a.budget_secs = next_val(&mut it, &k).parse().expect("budget"),
            "--heavy" => a.heavy = next_val(&mut it, &k),
            "--sweep" => {
                a.sweep = next_val(&mut it, &k).split(',').filter_map(|s| s.parse().ok()).collect()
            }
            // cargo 在 harness=false 时一般不加这两个，但容忍掉以免整轮跑挂
            "--bench" | "--nocapture" | "--exact" => {}
            other => panic!("未知参数 {other}"),
        }
    }
    if a.data_dir.as_os_str().is_empty() {
        panic!("必须指定 --data-dir");
    }
    if a.label.is_empty() {
        a.label = a
            .data_dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "fixture".into());
    }
    a
}

impl Args {
    fn wants(&self, group: &str) -> bool {
        self.only.iter().any(|g| g == group)
    }
    /// 是否运行不受控的重基准。这批基准的耗时是 O(存活文件 × 深度)，**恰恰是判断
    /// "checkout 是否可用"的核心证据**，所以默认要跑；`--heavy off` 是逃生开关。
    fn heavy_on(&self, _facts: &Facts) -> bool {
        !matches!(self.heavy.as_str(), "off" | "no" | "false")
    }

    /// 重基准的采样数：小档位取 3 次给出 p95/p99；大档位单次调用就是分钟级，
    /// 降到 1 次（此时 p50=p95=p99，报告里已标注），否则一轮压测要跑几小时。
    fn heavy_n(&self, facts: &Facts) -> usize {
        if facts.live_files > 200_000 {
            1
        } else {
            3
        }
    }

    /// 写入路径基准的采样数。`apply_commit_tx` 的 `byte_delta` 计算要对每条变更
    /// 按 `blob_hash` 反查 `changes`，而该列**没有索引**（报告 B5），于是单次提交的
    /// 代价是 O(本次变更数 × 全仓历史行数)。1000 条变更在 1M 文件档位单次要 ~5 分钟，
    /// 所以降到 1 次采样——增长趋势由 xs/s/m 三点给出，full 档只用来确认量级。
    fn commit_n(&self, facts: &Facts, base: usize) -> usize {
        if facts.changes > 1_000_000 {
            1
        } else {
            base
        }
    }

    /// 写入路径基准的预热次数：同上，大档位不预热。
    fn commit_warmup(&self, facts: &Facts, base: usize) -> usize {
        if facts.changes > 1_000_000 {
            0
        } else {
            base
        }
    }
}

// ============ 采样预算 ============

use std::sync::atomic::AtomicU64;

/// 单基准采样预算（毫秒）。超过即停止采样，避免大档位下无界耗时。
static BUDGET_MS: AtomicU64 = AtomicU64::new(15_000);
/// 达到该样本数后才允许因预算提前收工（保证百分位有意义）。
static MIN_SAMPLES: usize = 3;

fn budget_ms() -> u64 {
    BUDGET_MS.load(Ordering::Relaxed)
}

// ============ 统计 ============

#[derive(Serialize)]
struct Metric {
    group: String,
    name: String,
    n: usize,
    unit: String,
    p50: f64,
    p95: f64,
    p99: f64,
    mean: f64,
    min: f64,
    max: f64,
    /// 并发场景下的吞吐（req/s）；串行为 0
    throughput: f64,
    /// 结果规模（返回条目数、影响行数等），用于解释耗时
    size: i64,
    note: String,
}

struct Series {
    group: String,
    name: String,
    unit: String,
    samples: Vec<f64>,
    size: i64,
    note: String,
    elapsed: Option<f64>,
}

impl Series {
    fn new(group: &str, name: &str, unit: &str) -> Series {
        Series {
            group: group.into(),
            name: name.into(),
            unit: unit.into(),
            samples: vec![],
            size: -1,
            note: String::new(),
            elapsed: None,
        }
    }
    fn push(&mut self, v: f64) {
        self.samples.push(v);
    }
    fn finish(mut self) -> Metric {
        self.samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = self.samples.len().max(1) as f64;
        let q = |p: f64| -> f64 {
            if self.samples.is_empty() {
                return 0.0;
            }
            let idx = ((self.samples.len() - 1) as f64 * p).round() as usize;
            self.samples[idx]
        };
        let sum: f64 = self.samples.iter().sum();
        Metric {
            group: self.group,
            name: self.name,
            n: self.samples.len(),
            unit: self.unit,
            p50: q(0.50),
            p95: q(0.95),
            p99: q(0.99),
            mean: sum / n,
            min: self.samples.first().copied().unwrap_or(0.0),
            max: self.samples.last().copied().unwrap_or(0.0),
            throughput: match self.elapsed {
                Some(sec) if sec > 0.0 => self.samples.len() as f64 / sec,
                _ => 0.0,
            },
            size: self.size,
            note: self.note,
        }
    }
}

fn fmt_ms(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.2}s", v / 1000.0)
    } else if v >= 1.0 {
        format!("{v:.1}ms")
    } else {
        format!("{:.0}us", v * 1000.0)
    }
}

/// 串行同步基准：warmup 不计入，采样 iters 次；闭包返回的 i64 记为该基准的"规模"。
/// 超过 BUDGET_MS 且已有 MIN_SAMPLES 个样本时提前收工（在 note 里标注实际样本数）。
fn sync_bench<F>(group: &str, name: &str, iters: usize, warmup: usize, mut f: F) -> Result<Metric, String>
where
    F: FnMut() -> Result<i64, String>,
{
    for _ in 0..warmup {
        f()?;
    }
    let mut s = Series::new(group, name, "ms");
    let t_all = Instant::now();
    for i in 0..iters {
        let t = Instant::now();
        let size = f()?;
        s.push(t.elapsed().as_secs_f64() * 1000.0);
        if size >= 0 {
            s.size = size;
        }
        if i + 1 >= MIN_SAMPLES && t_all.elapsed().as_millis() as u64 >= budget_ms() {
            s.note = format!("预算 {}ms 用尽，仅 {}/{} 次采样", budget_ms(), i + 1, iters);
            break;
        }
    }
    Ok(s.finish())
}

// ============ HTTP 上下文 ============

struct Ctx {
    client: reqwest::Client,
    base: String,
    token: String,
    db_path: PathBuf,
    repo: String,
    /// 本地跟踪 head_rev，避免每次提交前多打一次 /info
    head: AtomicI64,
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

impl Ctx {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
    fn api(&self, path: &str) -> String {
        self.url(&format!("/api/v1/repos/{}{}", self.repo, path))
    }

    async fn send(&self, rb: reqwest::RequestBuilder) -> Result<(f64, String), String> {
        let t = Instant::now();
        let resp = rb
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .map_err(|e| format!("请求失败: {e}"))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| format!("读取响应失败: {e}"))?;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if !status.is_success() {
            return Err(format!("HTTP {status}: {}", truncate(&body, 300)));
        }
        Ok((ms, body))
    }

    /// 串行 HTTP 基准。`mk` 每次返回一个已构造好的 RequestBuilder 工厂。
    async fn http_bench<F>(&self, name: &str, iters: usize, warmup: usize, mut mk: F) -> Result<Metric, String>
    where
        F: FnMut() -> reqwest::RequestBuilder,
    {
        for _ in 0..warmup {
            self.send(mk()).await?;
        }
        let mut s = Series::new("http", name, "ms");
        let t_all = Instant::now();
        for i in 0..iters {
            let (ms, body) = self.send(mk()).await?;
            s.push(ms);
            match serde_json::from_str::<Value>(&body) {
                Ok(v) => {
                    if let Some(n) = v.get("total").and_then(|t| t.as_i64()) {
                        s.size = n;
                    } else if let Some(arr) = v.get("items").and_then(|t| t.as_array()) {
                        s.size = arr.len() as i64;
                    }
                }
                Err(_) => s.size = body.lines().count() as i64,
            }
            if i + 1 >= MIN_SAMPLES && t_all.elapsed().as_millis() as u64 >= budget_ms() {
                s.note = format!("预算 {}ms 用尽，仅 {}/{} 次采样", budget_ms(), i + 1, iters);
                break;
            }
        }
        Ok(s.finish())
    }

    /// 并发基准：固定并发度打满 total 个请求，报告尾延迟与吞吐。
    /// `mk` 必须 'static（内部只用 owned 克隆），以便 future 在 join_all 中并发推进。
    async fn http_conc<F>(&self, name: &str, total: usize, concurrency: usize, mk: F) -> Result<Metric, String>
    where
        F: Fn() -> reqwest::RequestBuilder + Send + Sync + 'static,
    {
        use futures_util::future::join_all;
        let whole = Instant::now();
        let mut s = Series::new("conc", &format!("{name}_c{concurrency}"), "ms");
        let mut done = 0usize;
        while done < total {
            let batch = (total - done).min(concurrency);
            let futs: Vec<_> = (0..batch).map(|_| self.send(mk())).collect();
            let mut err = None;
            for r in join_all(futs).await {
                match r {
                    Ok((ms, _)) => s.push(ms),
                    Err(e) => {
                        err = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = err {
                return Err(e);
            }
            done += batch;
            if done >= MIN_SAMPLES && whole.elapsed().as_millis() as u64 >= budget_ms() {
                s.note = format!(
                    "预算 {}ms 用尽，仅 {done}/{total} 请求；并发 {concurrency}",
                    budget_ms()
                );
                break;
            }
        }
        let secs = whole.elapsed().as_secs_f64();
        if s.note.is_empty() {
            s.note = format!("并发 {concurrency}，{total} 请求，吞吐 {:.0} req/s", total as f64 / secs);
        } else {
            s.note = format!("{}，实测吞吐 {:.0} req/s", s.note, done as f64 / secs);
        }
        s.elapsed = Some(secs);
        Ok(s.finish())
    }

    /// 只读连接：bench 进程与 router 共享同一 DB 文件（WAL 允许并发读）。
    fn sample_read(&self, sql: &str) -> Result<String, String> {
        let c = rusqlite::Connection::open(&self.db_path).map_err(|e| e.to_string())?;
        c.query_row(sql, [], |r| r.get(0)).map_err(|e| e.to_string())
    }
}

// ============ 主流程 ============

fn main() {
    let args = parse_args();
    BUDGET_MS.store((args.budget_secs.max(0.5) * 1000.0) as u64, Ordering::Relaxed);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .expect("tokio runtime");
    if let Err(e) = rt.block_on(run(args)) {
        eprintln!("\n[压测失败] {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), String> {
    let data_dir = args.data_dir.clone();
    let db_path = data_dir.join("b-artifact.db");
    if !db_path.exists() {
        return Err(format!(
            "找不到 {}；请先用 scripts/gen_fixture.py 造数",
            db_path.display()
        ));
    }

    // ---- 装配与线上完全一致的 AppState ----
    let mut cfg = Config::load(None).map_err(|e| format!("加载默认配置: {e}"))?;
    cfg.server.data_dir = data_dir.to_string_lossy().to_string();
    let mut conn = db::open(&db_path).map_err(|e| format!("打开 DB: {e}"))?;
    db::migrate(&mut conn).map_err(|e| format!("迁移: {e}"))?;

    // fixture 里的 bench 用户没有口令（造数脚本不引 argon2），这里补上
    let phc = password::hash_password(&args.password).map_err(|e| format!("口令哈希: {e}"))?;
    conn.execute(
        "UPDATE users SET password_hash = ?1, is_admin = 1 WHERE username = 'bench'",
        params![phc],
    )
    .map_err(|e| format!("写入测试用户口令: {e}"))?;

    drop(conn);
    let blob = BlobStore::new(&data_dir).map_err(|e| format!("BlobStore: {e}"))?;
    let db = DbPool::new(&db_path, cfg.db.pool_size).map_err(|e| format!("连接池: {e}"))?;
    let state = SharedState(Arc::new(AppState::new(
        db,
        blob,
        cfg.clone(),
        data_dir.join("uploads"),
    )));
    let app = api::router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("绑定端口: {e}"))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base = format!("http://{addr}");

    let facts = fixture_facts(&state, &args.repo)?;
    eprintln!(
        "[fixture] repo={} head_rev={} 文件 {} / changes {} / 修订 {} / blob {}",
        args.repo, facts.head_rev, facts.live_files, facts.changes, facts.revs, facts.blobs
    );

    // ---- 登录拿 token ----
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .build()
        .map_err(|e| format!("reqwest client: {e}"))?;
    let resp = client
        .post(format!("{base}/api/v1/auth/login"))
        .json(&json!({ "username": "bench", "password": args.password }))
        .send()
        .await
        .map_err(|e| format!("登录请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("登录失败 HTTP {status}: {}", truncate(&text, 300)));
    }
    let token = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("token").and_then(|t| t.as_str().map(|s| s.to_string())))
        .ok_or_else(|| "登录响应里没有 token".to_string())?;

    let ctx = Ctx {
        client,
        base,
        token,
        db_path: db_path.clone(),
        repo: args.repo.clone(),
        head: AtomicI64::new(facts.head_rev),
    };

    let mut metrics: Vec<Metric> = vec![];

    if args.wants("env") {
        eprintln!("[env] 采集环境底噪…");
        metrics.push(env_file_open(&state, &data_dir)?);
        metrics.push(sync_bench(
            "env",
            "sqlite_point_query",
            args.iterations,
            args.warmup,
            || {
                let conn = state.lock_db().map_err(|e| e.to_string())?;
                let v: i64 = conn.query_row("SELECT 1", [], |r| r.get(0)).map_err(|e| e.to_string())?;
                Ok(v)
            },
        )?);
        metrics.extend(env_stmt_cost(&state)?);
    }

    if args.wants("storage") {
        eprintln!("[storage] Path-History 核心路径…");
        metrics.extend(storage_bench(&state, &args, &facts)?);
    }

    if args.wants("sweep") {
        eprintln!("[sweep] 查询计划对照（LIKE vs GLOB）+ 候选代价…");
        metrics.extend(sweep_bench(&state, &args, &facts)?);
    }

    if args.wants("http") {
        eprintln!("[http] 端到端（真实 axum + reqwest）…");
        metrics.extend(http_bench(&ctx, &args, &facts).await?);
    }

    if args.wants("conc") {
        eprintln!("[conc] 并发…");
        metrics.extend(conc_bench(&ctx, &args, &facts).await?);
    }

    let report = json!({
        "label": args.label,
        "data_dir": data_dir.to_string_lossy(),
        "repo": args.repo,
        "iterations": args.iterations,
        "warmup": args.warmup,
        "budget_secs": args.budget_secs,
        "concurrency": args.concurrency,
        "heavy": if args.heavy_on(&facts) {
            format!("on(n={})", args.heavy_n(&facts))
        } else {
            "skipped".into()
        },
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "env": env_meta(&data_dir, &facts),
        "fixture": facts.to_json(),
        "metrics": metrics,
    });

    if let Some(out) = &args.out {
        let s = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        std::fs::write(out, s).map_err(|e| format!("写报告失败: {e}"))?;
        eprintln!("[out] JSON 报告已写入 {}", out.display());
    }

    print_markdown(&args.label, &metrics, &facts);
    Ok(())
}

// ============ fixture 事实 ============

struct Facts {
    repo_id: i64,
    head_rev: i64,
    live_files: i64,
    live_bytes: i64,
    changes: i64,
    revs: i64,
    blobs: i64,
    sample_deep: String,
}

impl Facts {
    fn to_json(&self) -> Value {
        json!({
            "head_rev": self.head_rev,
            "live_files": self.live_files,
            "live_bytes": self.live_bytes,
            "changes": self.changes,
            "revs": self.revs,
            "blobs": self.blobs,
            "sample_deep": self.sample_deep,
        })
    }
}

fn fixture_facts(state: &SharedState, repo_name: &str) -> Result<Facts, String> {
    let conn = state.lock_db().map_err(|e| e.to_string())?;
    let row = repo::repo_by_name(&conn, repo_name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("仓库 `{repo_name}` 不存在"))?;
    let rid = row.id;
    let one = |sql: &str| -> i64 { conn.query_row(sql, params![rid], |r| r.get(0)).unwrap_or(0) };
    let (live_files, live_bytes) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(size),0) FROM head_entries WHERE repo_id=?1 AND kind='file'",
            params![rid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| e.to_string())?;
    let sample_deep: String = conn
        .query_row(
            "SELECT path FROM head_entries WHERE repo_id=?1 AND kind='file' ORDER BY path LIMIT 1",
            params![rid],
            |r| r.get(0),
        )
        .map_err(|e| format!("fixture 里没有文件: {e}"))?;
    Ok(Facts {
        repo_id: rid,
        head_rev: row.head_rev,
        live_files,
        live_bytes,
        changes: one("SELECT COUNT(*) FROM changes WHERE repo_id=?1"),
        revs: one("SELECT COUNT(*) FROM revisions WHERE repo_id=?1"),
        blobs: conn.query_row("SELECT COUNT(*) FROM blobs", [], |r| r.get(0)).unwrap_or(0),
        sample_deep,
    })
}

/// 环境底噪：现有 blob 文件的 open+read+close 单次成本。
fn env_file_open(state: &SharedState, data_dir: &Path) -> Result<Metric, String> {
    let blob_path = {
        let conn = state.lock_db().map_err(|e| e.to_string())?;
        let h: String = conn
            .query_row("SELECT hash FROM blobs LIMIT 1", [], |r| r.get(0))
            .map_err(|e| format!("blobs 表为空: {e}"))?;
        data_dir.join("blobs").join(&h[..2]).join(&h[2..4]).join(&h)
    };
    let mut s = Series::new("env", "file_open_read_close", "ms");
    s.note = "打开+读一个已有 blob 文件的固定开销".into();
    for _ in 0..20 {
        let t = Instant::now();
        let data = std::fs::read(&blob_path).map_err(|e| format!("读 blob: {e}"))?;
        s.push(t.elapsed().as_secs_f64() * 1000.0);
        s.size = data.len() as i64;
    }
    Ok(s.finish())
}

/// 「每次调用都重新 prepare」的代价：`repo::last_change` 等热点函数用的是
/// `conn.query_row(SQL_字面量, ..)`，不走 rusqlite 的 prepared-statement 缓存，
/// 于是每次调用都要让 SQLite 重新解析一遍 SQL。这里把三件事分开量：
///   - `stmt_prepare_uncached`：同一段 ~150 字符 SQL 反复 prepare+drop（模拟现状）
///   - `stmt_prepare_cached`  ：同一段 SQL 走 `prepare_cached`（走的修复方向）
///   - `stmt_index_lookup`    ：已 prepare 的点查，纯索引查找成本（对照组）
fn env_stmt_cost(state: &SharedState) -> Result<Vec<Metric>, String> {
    // 与 repo.rs::last_change 的 SQL 长度、形态一致
    const SQL: &str = "SELECT rev, op, kind, blob_hash, size, mode, mtime FROM changes
                         WHERE repo_id = ?1 AND path = ?2 AND rev <= ?3 AND kind = ?4
                         ORDER BY rev DESC LIMIT 1";
    let mut out = vec![];

    let mut m = sync_bench("env", "stmt_prepare_uncached", 2000, 100, || {
        let conn = state.lock_db().map_err(|e| e.to_string())?;
        let stmt = conn.prepare(SQL).map_err(|e| e.to_string())?;
        let len = stmt.column_count() as i64;
        drop(stmt);
        Ok(len)
    })?;
    m.note = "每次调用重新 prepare（现状：query_row 传 SQL 字面量）".into();
    out.push(m);

    let mut m = sync_bench("env", "stmt_prepare_cached", 2000, 100, || {
        let conn = state.lock_db().map_err(|e| e.to_string())?;
        let stmt = conn.prepare_cached(SQL).map_err(|e| e.to_string())?;
        let len = stmt.column_count() as i64;
        drop(stmt);
        Ok(len)
    })?;
    m.note = "走语句缓存（修复方向：prepare_cached）".into();
    out.push(m);

    // 对照：已 prepare 的索引点查（真实执行成本）
    {
        let (path, rid): (String, i64) = {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            let rid: i64 = conn.query_row("SELECT id FROM repos LIMIT 1", [], |r| r.get(0)).map_err(|e| e.to_string())?;
            let p: String = conn
                .query_row(
                    "SELECT path FROM head_entries WHERE repo_id=?1 AND kind='file' ORDER BY path LIMIT 1",
                    params![rid],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            (p, rid)
        };
        let mut m = sync_bench("env", "stmt_index_lookup", 2000, 100, || {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            let mut stmt = conn.prepare_cached(SQL).map_err(|e| e.to_string())?;
            let n = stmt
                .query_map(params![rid, &path, 1_000_000i64, "file"], |_| Ok(1))
                .map_err(|e| e.to_string())?
                .count();
            Ok(n as i64)
        })?;
        m.note = "已 prepare 的点查（含索引查找 + 无匹配）".into();
        out.push(m);
    }
    Ok(out)
}

fn env_meta(data_dir: &Path, facts: &Facts) -> Value {
    let db_bytes = std::fs::metadata(data_dir.join("b-artifact.db")).map(|m| m.len()).unwrap_or(0);
    let wal_bytes = std::fs::metadata(data_dir.join("b-artifact.db-wal")).map(|m| m.len()).unwrap_or(0);
    json!({
        "cpu": sysctl("hw.ncpu"),
        "mem_bytes": sysctl("hw.memsize"),
        "db_bytes": db_bytes,
        "wal_bytes": wal_bytes,
        "live_bytes": facts.live_bytes,
        "os": std::env::consts::OS,
    })
}

fn sysctl(key: &str) -> i64 {
    std::process::Command::new("sysctl")
        .arg("-n")
        .arg(key)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

// ============ B. 存储层 ============

fn storage_bench(state: &SharedState, args: &Args, facts: &Facts) -> Result<Vec<Metric>, String> {
    let rid = facts.repo_id;
    let uid = 1i64;
    let mid_rev = (facts.head_rev / 2).max(1);
    let it = args.iterations;
    let w = args.warmup;
    let mut out = vec![];

    // 列目录：HEAD 快路径（head_entries 前缀 LIKE）
    for (name, prefix) in [
        ("list_dir_head_root", ""),
        ("list_dir_head_l1", "assets"),
        ("list_dir_head_l2", "assets/0000"),
        ("list_dir_head_leaf", "assets/0000/0000/000"),
    ] {
        let p = prefix.to_string();
        out.push(sync_bench("storage", name, it, w, move || {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            let v = repo::list_dir_head(&conn, rid, &p).map_err(|e| e.to_string())?;
            Ok(v.len() as i64)
        })?);
    }

    // 列目录：历史推导路径（changes 窗口函数）。
    // 前缀固定在子树规模恒定的位置（每个末级目录固定 10 个文件），这样跨档位比较时
    // 唯一变化的是 **全仓历史长度**，把"扫描量随历史增长"这一项单独隔离出来。
    for (name, prefix, rev) in [
        ("list_dir_at_leaf_head", "assets/0000/0000/000", facts.head_rev),
        ("list_dir_at_leaf_mid", "assets/0000/0000/000", mid_rev),
        ("list_dir_at_l2_mid", "assets/0000", mid_rev),
    ] {
        let p = prefix.to_string();
        out.push(sync_bench("storage", name, it.min(20), w.min(2), move || {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            let v = repo::list_dir_at(&conn, rid, &p, rev).map_err(|e| e.to_string())?;
            Ok(v.len() as i64)
        })?);
    }

    // 不受控的重基准：整仓根目录的历史列目录，代价 O(存活文件 × 路径深度)。
    // 大档位默认跳过（用 --heavy on 强制）。
    if args.heavy_on(facts) {
        for (name, prefix) in [("list_dir_at_root_head", ""), ("list_dir_at_l1_mid", "assets")] {
            let p = prefix.to_string();
            out.push(sync_bench("storage", name, args.heavy_n(facts), 0, move || {
                let conn = state.lock_db().map_err(|e| e.to_string())?;
                let v = repo::list_dir_at(&conn, rid, &p, facts.head_rev).map_err(|e| e.to_string())?;
                Ok(v.len() as i64)
            })?);
        }
    }

    // ---- 关键 A/B：PRAGMA case_sensitive_like=ON 之后，同一生产函数走不走 path 索引 ----
    // 方案 §3.3 的 Path-History 查询用 `path LIKE 'pfx%' ESCAPE '\'`，而 SQLite 只有在
    // case_sensitive_like=ON（或改用 GLOB / 范围条件）时才能把前缀 LIKE 下推成
    // idx_changes_path 的范围扫描（见 docs/压测报告-M1.5.md 的 EXPLAIN QUERY PLAN 证据）。
    // 这里用**同一个 repo::list_dir_at**，只切换 pragma，量化"一行的修复"能带来多少收益。
    for (name, prefix, rev) in [
        ("list_dir_at_leaf_head_cslike", "assets/0000/0000/000", facts.head_rev),
        ("list_dir_at_l2_mid_cslike", "assets/0000", mid_rev),
    ] {
        let p = prefix.to_string();
        {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            conn.execute_batch("PRAGMA case_sensitive_like=ON").map_err(|e| e.to_string())?;
        }
        let m = sync_bench("storage", name, it.min(20), w.min(2), || {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            let v = repo::list_dir_at(&conn, rid, &p, rev).map_err(|e| e.to_string())?;
            Ok(v.len() as i64)
        });
        {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            conn.execute_batch("PRAGMA case_sensitive_like=OFF").map_err(|e| e.to_string())?;
        }
        out.push(m?);
    }
    // HEAD 快路径同一个坑：head_entries 的前缀 LIKE 同样不能下推索引。
    for (name, prefix) in
        [("list_dir_head_leaf_cslike", "assets/0000/0000/000"), ("list_dir_head_l2_cslike", "assets/0000")]
    {
        let p = prefix.to_string();
        {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            conn.execute_batch("PRAGMA case_sensitive_like=ON").map_err(|e| e.to_string())?;
        }
        let m = sync_bench("storage", name, it, w, || {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            let v = repo::list_dir_head(&conn, rid, &p).map_err(|e| e.to_string())?;
            Ok(v.len() as i64)
        });
        {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            conn.execute_batch("PRAGMA case_sensitive_like=OFF").map_err(|e| e.to_string())?;
        }
        out.push(m?);
    }

    // 单文件取内容
    let deep = facts.sample_deep.clone();
    out.push(sync_bench("storage", "file_at_head", it * 5, w, move || {
        let conn = state.lock_db().map_err(|e| e.to_string())?;
        let e = repo::file_at(&conn, rid, &deep, facts.head_rev).map_err(|x| x.to_string())?;
        Ok(if e.is_some() { 1 } else { 0 })
    })?);
    let deep = facts.sample_deep.clone();
    out.push(sync_bench("storage", "file_at_old_rev", it * 5, w, move || {
        let conn = state.lock_db().map_err(|e| e.to_string())?;
        let e = repo::file_at(&conn, rid, &deep, mid_rev).map_err(|x| x.to_string())?;
        Ok(if e.is_some() { 1 } else { 0 })
    })?);
    let deep = facts.sample_deep.clone();
    out.push(sync_bench("storage", "dir_exists_at_head", it, w, move || {
        let conn = state.lock_db().map_err(|e| e.to_string())?;
        let e = repo::dir_exists_at(&conn, rid, &deep, facts.head_rev).map_err(|x| x.to_string())?;
        Ok(if e { 1 } else { 0 })
    })?);

    // manifest_hash（仅 HEAD，异步补算；全量扫描 head_entries）
    out.push(sync_bench("storage", "manifest_hash", it.min(5).max(1), 0, || {
        let conn = state.lock_db().map_err(|e| e.to_string())?;
        let h = repo::compute_manifest_hash(&conn, rid).map_err(|e| e.to_string())?;
        Ok(h.len() as i64)
    })?);

    // repo_info 统计
    out.push(sync_bench("storage", "repo_info_stats", it, w, || {
        let conn = state.lock_db().map_err(|e| e.to_string())?;
        let (c, _s): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(size),0) FROM head_entries WHERE repo_id=?1 AND kind='file'",
                params![rid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        Ok(c)
    })?);

    // log 分页 / changes 增量窗口
    out.push(sync_bench("storage", "log_page_100", it, w, || {
        let conn = state.lock_db().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT r.rev, r.author_id, u.username, r.message, r.created_at, r.file_count,
                        r.byte_delta, r.manifest_hash
                   FROM revisions r JOIN users u ON u.id = r.author_id
                  WHERE r.repo_id = ?1 ORDER BY r.rev DESC LIMIT 100 OFFSET 0",
            )
            .map_err(|e| e.to_string())?;
        let n = stmt
            .query_map(params![rid], |_| Ok(1))
            .map_err(|e| e.to_string())?
            .count();
        Ok(n as i64)
    })?);
    out.push(sync_bench("storage", "changes_window_100rev", it.min(20), 2, || {
        let conn = state.lock_db().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT rev, path, op, kind, blob_hash, size, mode, mtime FROM changes
                  WHERE repo_id = ?1 AND rev >= ?2 AND rev <= ?3 ORDER BY rev, path",
            )
            .map_err(|e| e.to_string())?;
        let n = stmt
            .query_map(params![rid, mid_rev, mid_rev + 100], |_| Ok(1))
            .map_err(|e| e.to_string())?
            .count();
        Ok(n as i64)
    })?);

    // 提交事务：apply_commit_tx（与线上 commit 同一函数）→ 事务内回滚，可重复测
    for k in [10usize, 100, 1000] {
        let mut counter = 0usize;
        let base = if k >= 1000 { 5 } else if k >= 100 { 15 } else { 40 };
        out.push(sync_bench(
            "storage",
            &format!("apply_commit_{k}"),
            args.commit_n(facts, base),
            args.commit_warmup(facts, 2),
            move || {
                counter += 1;
                let changes: Vec<Change> = (0..k)
                    .map(|i| {
                        // 固定用一个 fixture 里**不存在**的 blob_hash，于是 `byte_delta`
                        // 每条变更都走"本仓库未引用过"分支 —— 这正是 `WHERE blob_hash = ?`
                        // 找不到匹配、必须扫完整个仓库历史的最坏情形（也是真实新增内容的情形）。
                        Change::add_file(
                            format!("assets/0000/0000/000/bench{counter:04}_{i:04}.bin"),
                            "0000000000000000000000000000000000000000000000000000000000000001",
                            1024,
                        )
                    })
                    .collect();
                let mut conn = state.lock_db().map_err(|e| e.to_string())?;
                let head = repo::repo_head_rev(&conn, rid).map_err(|e| e.to_string())?;
                let tx = conn.transaction().map_err(|e| e.to_string())?;
                let r = repo::apply_commit_tx(&tx, rid, uid, "bench", &changes, head)
                    .map_err(|e| e.to_string())?;
                tx.rollback().map_err(|e| e.to_string())?;
                Ok(r.file_count)
            },
        )?);
    }

    // refcount 全量重建：两种写法对比（§3.4 refcount 是可重建缓存）。
    // ① 逐 blob 相关子查询 —— 语义直观，但每个 blob 都要全表扫一遍 changes，
    //    总代价 O(blobs × changes)。大档位下跑一次要几十分钟，所以只采"单个 blob 的
    //    单元代价"，再乘以 blob 数外推（报告里会明确标注是外推值）。
    // ② 单次 GROUP BY 聚合（推荐写法）：代价 O(changes)。
    for k in [1i64, 10] {
        out.push(sync_bench(
            "storage",
            &format!("refcount_correlated_{k}blob"),
            if k == 1 { 5 } else { 3 },
            0,
            move || {
                let mut conn = state.lock_db().map_err(|e| e.to_string())?;
                let tx = conn.transaction().map_err(|e| e.to_string())?;
                let n = tx
                    .execute(
                        "UPDATE blobs SET refcount =
                           (SELECT COUNT(*) FROM changes c WHERE c.blob_hash = blobs.hash)
                         WHERE hash IN (SELECT hash FROM blobs ORDER BY hash LIMIT ?1)",
                        params![k],
                    )
                    .map_err(|e| e.to_string())?;
                tx.rollback().map_err(|e| e.to_string())?;
                Ok(n as i64)
            },
        )?);
    }
    out.push(sync_bench("storage", "refcount_groupby_all", 3, 0, move || {
        let mut conn = state.lock_db().map_err(|e| e.to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute("UPDATE blobs SET refcount = 0", []).map_err(|e| e.to_string())?;
        let n = tx
            .execute(
                "UPDATE blobs SET refcount = t.c FROM (
                     SELECT blob_hash AS h, COUNT(*) AS c FROM changes
                      WHERE blob_hash IS NOT NULL GROUP BY blob_hash) t
                 WHERE blobs.hash = t.h",
                [],
            )
            .map_err(|e| e.to_string())?;
        tx.rollback().map_err(|e| e.to_string())?;
        Ok(n as i64)
    })?);
    Ok(out)
}

// ============ B2. 查询计划对照（LIKE vs GLOB） ============

/// Path-History 的两条核心 SQL 都写作 `path LIKE 'pfx%' ESCAPE '\'`。
/// SQLite 只在 **大小写敏感 LIKE**（`PRAGMA case_sensitive_like=ON`）或改用 `GLOB` /
/// 范围条件时，才能把前缀条件下推成 `idx_changes_path` 的范围扫描；否则退化成
/// 「扫描该仓库 revision ≤ N 的全部 change 行 + 临时 B-Tree 排序」——
/// 代价与 **整个仓库的历史长度** 成正比，而不是与目录大小成正比。
///
/// 本组把生产 SQL 与等价重写并排量，并**校验两者返回行数一致**，用来证明：
///   1）重写语义等价；2）收益来自查询计划而非别处。
fn sweep_bench(state: &SharedState, _args: &Args, facts: &Facts) -> Result<Vec<Metric>, String> {
    let rid = facts.repo_id;
    let rev = facts.head_rev;
    let mut out = vec![];

    // 生产写法：changes 的窗口函数 + 前缀 LIKE
    const SQL_LIKE: &str = "
        WITH last AS (
            SELECT path, kind, op,
                   ROW_NUMBER() OVER (PARTITION BY path ORDER BY rev DESC) AS rn
              FROM changes
             WHERE repo_id = ?1 AND rev <= ?2 AND path LIKE ?3 ESCAPE '\\'
        )
        SELECT path, kind FROM last WHERE rn = 1 AND op <> 'delete'";
    // 等价重写：GLOB 前缀（SQLite 可下推 idx_changes_path）
    const SQL_GLOB: &str = "
        WITH last AS (
            SELECT path, kind, op,
                   ROW_NUMBER() OVER (PARTITION BY path ORDER BY rev DESC) AS rn
              FROM changes
             WHERE repo_id = ?1 AND rev <= ?2 AND path GLOB ?3
        )
        SELECT path, kind FROM last WHERE rn = 1 AND op <> 'delete'";

    for (tag, pfx) in [
        ("root", "assets/"),
        ("l2", "assets/0000/"),
        ("leaf", "assets/0000/0000/000/"),
    ] {
        let like_pat = format!("{pfx}%");
        let glob_pat = format!("{pfx}*");

        let count = |sql: &str, pat: &str| -> Result<i64, String> {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
            let n = stmt
                .query_map(params![rid, rev, pat], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?
                .count();
            Ok(n as i64)
        };

        // 先校验等价性：不一致说明重写有语义偏差，直接失败而不是给出误导数字
        let n_like = count(SQL_LIKE, &like_pat)?;
        let n_glob = count(SQL_GLOB, &glob_pat)?;
        if n_like != n_glob {
            return Err(format!(
                "前缀 `{pfx}` 下 LIKE 与 GLOB 结果不一致（{n_like} vs {n_glob}），重写不等价"
            ));
        }

        let (lp, gp) = (like_pat.clone(), glob_pat.clone());
        let mut m = sync_bench("sweep", &format!("changes_prefix_like_{tag}"), 5, 1, move || {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            let mut stmt = conn.prepare(SQL_LIKE).map_err(|e| e.to_string())?;
            let n = stmt
                .query_map(params![rid, rev, &lp], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?
                .count();
            Ok(n as i64)
        })?;
        m.note = "生产写法：前缀 LIKE + 窗口函数（计划退化为全历史扫描）".into();
        out.push(m);

        let gp2 = gp.clone();
        let mut m = sync_bench("sweep", &format!("changes_prefix_glob_{tag}"), 5, 1, move || {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            let mut stmt = conn.prepare(SQL_GLOB).map_err(|e| e.to_string())?;
            let n = stmt
                .query_map(params![rid, rev, &gp2], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?
                .count();
            Ok(n as i64)
        })?;
        m.note = format!("等价重写：GLOB 前缀（走 idx_changes_path 范围扫描）；与 LIKE 同结果 {n_like} 行");
        out.push(m);

        // HEAD 快路径同一个坑
        let lp3 = like_pat.clone();
        let gp3 = glob_pat.clone();
        out.push(sync_bench("sweep", &format!("head_prefix_like_{tag}"), 5, 1, move || {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            let mut stmt = conn
                .prepare(
                    "SELECT path FROM head_entries
                      WHERE repo_id = ?1 AND path LIKE ?2 ESCAPE '\\'",
                )
                .map_err(|e| e.to_string())?;
            let n = stmt
                .query_map(params![rid, &lp3], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?
                .count();
            Ok(n as i64)
        })?);
        out.push(sync_bench("sweep", &format!("head_prefix_glob_{tag}"), 5, 1, move || {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            let mut stmt = conn
                .prepare("SELECT path FROM head_entries WHERE repo_id = ?1 AND path GLOB ?2")
                .map_err(|e| e.to_string())?;
            let n = stmt
                .query_map(params![rid, &gp3], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?
                .count();
            Ok(n as i64)
        })?);
    }

    // 单候选代价：file_at 一次 = 1 次 last_change + 每个祖先 1 次 last_change。
    // list_dir_at 的历史路径要**对每个候选**都做一次，所以这个数是乘数项。
    let mut cands: Vec<String> = vec![];
    {
        let conn = state.lock_db().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT path FROM head_entries WHERE repo_id = ?1 AND kind = 'file'
                  ORDER BY path LIMIT 2000",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![rid], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for r in rows {
            cands.push(r.map_err(|e| e.to_string())?);
        }
    }
    for k in [1usize, 10, 100] {
        if cands.len() < k {
            continue;
        }
        let set: Vec<String> = cands[..k].to_vec();
        let mut m = sync_bench("sweep", &format!("file_at_x{k}"), 5, 1, move || {
            let conn = state.lock_db().map_err(|e| e.to_string())?;
            let mut hit = 0i64;
            for p in &set {
                if repo::file_at(&conn, rid, p, rev).map_err(|e| e.to_string())?.is_some() {
                    hit += 1;
                }
            }
            Ok(hit)
        })?;
        m.note = format!("{} 个候选路径各做一次 file_at（含祖先 Tombstone 链查询）", k);
        out.push(m);
    }

    Ok(out)
}

// ============ C. HTTP 端到端 ============

async fn http_bench(ctx: &Ctx, args: &Args, facts: &Facts) -> Result<Vec<Metric>, String> {
    let it = args.iterations;
    let w = args.warmup;
    let cl = ctx.client.clone();
    let mut out = vec![];

    let u = ctx.url("/health");
    let c = cl.clone();
    out.push(ctx.http_bench("health", it * 3, w, move || c.get(u.clone())).await?);

    for (name, q) in [
        ("tree_root_depth1", "?depth=1"),
        ("tree_l1_depth1", "?prefix=assets&depth=1"),
        ("tree_l2_depth1", "?prefix=assets/0000&depth=1"),
        ("tree_leaf_depth1", "?prefix=assets/0000/0000/000&depth=1"),
    ] {
        let u = ctx.api(&format!("/tree{q}"));
        let c = cl.clone();
        out.push(ctx.http_bench(name, it, w, move || c.get(u.clone())).await?);
    }

    // 历史修订下的列目录：前缀固定在末级目录，保证子树规模不随档位变化，
    // 唯一变量还是"全仓历史长度"。
    for (name, q) in [
        ("tree_root_depth2", "?depth=2".to_string()),
        (
            "tree_leaf_hist_rev",
            format!("?prefix=assets/0000/0000/000&depth=1&rev={}", (facts.head_rev / 2).max(1)),
        ),
    ] {
        let u = ctx.api(&format!("/tree{q}"));
        let c = cl.clone();
        out.push(ctx.http_bench(name, it.min(20), 2, move || c.get(u.clone())).await?);
    }
    // 根目录历史列目录：代价 O(存活文件 × 深度)，大档位默认跳过
    if args.heavy_on(facts) {
        let q = format!("?rev={}&depth=1", (facts.head_rev / 2).max(1));
        let u = ctx.api(&format!("/tree{q}"));
        let c = cl.clone();
        out.push(
            ctx.http_bench("tree_root_hist_rev", args.heavy_n(facts), 0, move || c.get(u.clone()))
                .await?,
        );
    }

    let u = ctx.api("/info");
    let c = cl.clone();
    out.push(ctx.http_bench("repo_info", it, w, move || c.get(u.clone())).await?);

    let u = ctx.api("/log?limit=50");
    let c = cl.clone();
    out.push(ctx.http_bench("log_50", it, w, move || c.get(u.clone())).await?);

    let mid_rev = (facts.head_rev / 2).max(1);
    let u = ctx.api(&format!("/changes?from={mid_rev}&to={}", mid_rev + 100));
    let c = cl.clone();
    out.push(ctx.http_bench("changes_100rev", it.min(20), 2, move || c.get(u.clone())).await?);

    // blobs/missing：一次 1000 个 hash（接口上限）
    let hashes: Vec<String> = (0..1000).map(|i| format!("{i:064x}")).collect();
    let u = ctx.api("/blobs/missing");
    let c = cl.clone();
    out.push(
        ctx.http_bench("missing_1000", it.min(20), 2, move || {
            c.post(u.clone()).json(&json!({ "hashes": hashes }))
        })
        .await?,
    );

    // 下载 blob（含 open 系统调用与流式解压路径）
    let sample_hash = ctx.sample_read("SELECT hash FROM blobs LIMIT 1")?;
    let u = ctx.api(&format!("/blobs/{sample_hash}"));
    let c = cl.clone();
    out.push(ctx.http_bench("download_blob", it * 2, w, move || c.get(u.clone())).await?);

    // 登录（argon2id m=64MiB t=3 p=4 —— 预期的"贵操作"基准）
    {
        let pw = args.password.clone();
        let u = ctx.url("/api/v1/auth/login");
        let c = cl.clone();
        let mut s = Series::new("http", "login_argon2id", "ms");
        for _ in 0..5 {
            let t = Instant::now();
            let r = c
                .post(u.clone())
                .json(&json!({ "username": "bench", "password": pw }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !r.status().is_success() {
                return Err(format!("登录基准失败 HTTP {}", r.status()));
            }
            let _ = r.text().await;
            s.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        out.push(s.finish());
    }

    // 两阶段提交（真实落库，串行递增 head_rev）
    {
        let k = 10usize;
        let mut s = Series::new("http", &format!("commit_two_phase_{k}"), "ms");
        s.note = "prepare + commit 合计，含 10 条变更".into();
        for i in 0..10 {
            let t = Instant::now();
            let base_rev = ctx.head.load(Ordering::SeqCst);
            let cid = uuid::Uuid::new_v4().to_string();
            let changes: Vec<Value> = (0..k)
                .map(|j| {
                    json!({
                        "path": format!("assets/0000/0000/000/bench-http-{i:02}-{j:02}.bin"),
                        "op": "add",
                        "kind": "file",
                        "blob_hash": sample_hash,
                        "size": 1024,
                        "mode": 420,
                        "mtime": 0,
                    })
                })
                .collect();
            let (_, body) = ctx
                .send(ctx.client.post(ctx.api("/commit/prepare")).json(&json!({
                    "commit_id": cid,
                    "base_rev": base_rev,
                    "message": "bench",
                    "changes": changes,
                })))
                .await?;
            let v: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            let tok = v
                .get("commit_token")
                .and_then(|t| t.as_str())
                .ok_or_else(|| format!("prepare 未返回 token: {}", truncate(&body, 200)))?
                .to_string();
            ctx.send(ctx.client.post(ctx.api("/commit")).json(&json!({
                "commit_id": cid,
                "commit_token": tok,
                "message": "bench",
            })))
            .await?;
            ctx.head.fetch_add(1, Ordering::SeqCst);
            s.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        s.size = k as i64;
        out.push(s.finish());
    }

    Ok(out)
}

// ============ D. 并发 ============

async fn conc_bench(ctx: &Ctx, args: &Args, facts: &Facts) -> Result<Vec<Metric>, String> {
    let total = (args.iterations * 2).max(64);
    let cl = ctx.client.clone();
    let mut out = vec![];

    let u = ctx.api("/tree?depth=1");
    for &c in &args.concurrency {
        let (c2, u2) = (cl.clone(), u.clone());
        out.push(ctx.http_conc("tree_root_depth1", total, c, move || c2.get(u2.clone())).await?);
    }

    let sample_hash = ctx.sample_read("SELECT hash FROM blobs LIMIT 1")?;
    let u = ctx.api(&format!("/blobs/{sample_hash}"));
    for &c in &args.concurrency {
        let (c2, u2) = (cl.clone(), u.clone());
        out.push(ctx.http_conc("download_blob", total, c, move || c2.get(u2.clone())).await?);
    }

    // 历史修订路径（会走 list_dir_at）+ HEAD 内容路径（走 file_at）：都是"读"，
    // 目的就是看单写者 Mutex<Connection> 在纯读并发下会不会互相排队。
    let mid_rev = (facts.head_rev / 2).max(1);
    let u = ctx.api(&format!("/tree?prefix=assets/0000/0000/000&depth=1&rev={mid_rev}"));
    for &c in &args.concurrency {
        let (c2, u2) = (cl.clone(), u.clone());
        out.push(ctx.http_conc("tree_hist_leaf", total, c, move || c2.get(u2.clone())).await?);
    }

    // 单文件（没有独立 /file 端点，用 tree 的精确前缀实现，depth=1 只回 1 条）
    let deep = facts.sample_deep.clone();
    let u = ctx.api(&format!("/tree?prefix={deep}&depth=1"));
    for &c in &args.concurrency {
        let (c2, u2) = (cl.clone(), u.clone());
        out.push(ctx.http_conc("tree_head_file", total, c, move || c2.get(u2.clone())).await?);
    }

    // 单文件的历史修订版本（走 list_dir_at）
    let deep = facts.sample_deep.clone();
    let u = ctx.api(&format!("/tree?prefix={deep}&depth=1&rev={mid_rev}"));
    for &c in &args.concurrency {
        let (c2, u2) = (cl.clone(), u.clone());
        out.push(ctx.http_conc("tree_hist_file", total, c, move || c2.get(u2.clone())).await?);
    }

    let u = ctx.api("/info");
    for &c in &args.concurrency {
        let (c2, u2) = (cl.clone(), u.clone());
        out.push(ctx.http_conc("repo_info", total, c, move || c2.get(u2.clone())).await?);
    }

    let u = ctx.api("/log?limit=50");
    for &c in &args.concurrency {
        let (c2, u2) = (cl.clone(), u.clone());
        out.push(ctx.http_conc("log_50", total, c, move || c2.get(u2.clone())).await?);
    }

    Ok(out)
}

// ============ 输出 ============

fn print_markdown(label: &str, metrics: &[Metric], facts: &Facts) {
    println!("\n## {label}\n");
    println!(
        "fixture：{} 个存活文件 / {} 修订 / changes {} 行 / blob {} 个\n",
        facts.live_files, facts.revs, facts.changes, facts.blobs
    );
    for group in ["env", "storage", "sweep", "http", "conc"] {
        let rows: Vec<&Metric> = metrics.iter().filter(|m| m.group == group).collect();
        if rows.is_empty() {
            continue;
        }
        let title = match group {
            "env" => "环境底噪",
            "storage" => "存储层（直连）",
            "sweep" => "前缀查询计划对照（LIKE vs GLOB）",
            "http" => "HTTP 端到端",
            "conc" => "并发",
            _ => group,
        };
        println!("### {title}\n");
        println!("| 基准 | n | p50 | p95 | p99 | 规模 | 吞吐 | 备注 |");
        println!("|---|---|---|---|---|---|---|---|");
        for m in rows {
            println!(
                "| `{}` | {} | {} | {} | {} | {} | {} | {} |",
                m.name,
                m.n,
                fmt_ms(m.p50),
                fmt_ms(m.p95),
                fmt_ms(m.p99),
                if m.size >= 0 { m.size.to_string() } else { "-".into() },
                if m.throughput > 0.0 { format!("{:.0}/s", m.throughput) } else { "-".into() },
                m.note
            );
        }
        println!();
    }
    let _ = std::io::stdout().flush();
}
