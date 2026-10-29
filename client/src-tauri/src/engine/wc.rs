/**
 * 工作副本引擎（§6.3 关键流程）：检出 / 状态 / 提交 / 更新 / 锁 / 冲突。
 *
 * 与 `db.rs` 的分工：这里只负责"把服务端状态与磁盘状态对齐"的流程编排，
 * 所有持久化都走 `Wc`，所有网络都走 `ApiClient`，便于单测用桩替换。
 *
 * 与 TS 版的差异只在**并发原语**：TS 靠事件循环 + `mapLimit`，这里靠
 * `buffer_unordered` + `spawn_blocking`（哈希/读写不能占住 async worker）。
 */
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use serde::Serialize;

use crate::engine::api::{ApiClient, ChangeSpec, LockInfo, PrepareResult};
use crate::engine::db::{
    acquire_process_lock, ConflictReason, ConflictRow, EntryKind, EntryRow, EntryStatus, PendingOp,
    ProcessLock, Wc, WC_DIR, WC_DB,
};
use crate::engine::errors::{ClientError, ClientResult};
use crate::engine::hash::{hash_file, needs_chunked_upload, DEFAULT_CHUNK_SIZE};
use crate::engine::ignore::IGNORE_FILE;
use crate::engine::io_retry::{with_retry, with_retry_async, RetryOptions};
use crate::engine::paths::{normalize_nfc, parent_of, validate_path};
use crate::engine::pristine::{
    cache_path_for, default_cache_dir, ensure_pristine, import_working_file, install_working_file,
    pristine_path_for, store_to_cache,
};
use crate::engine::scan::{compute_status, is_local_change, scan_disk, StatusItem, StatusOptions, MINE_SUFFIX};
use crate::engine::text::{read_text_capped, TextReadResult};

/// 部分检出时一次拉满的目录深度（服务端 clamp 到 16）。
const TREE_DEPTH: i64 = 16;
/// 传输并发上限（§6.3：信号量限 4~8 并发）。
pub const DEFAULT_CONCURRENCY: usize = 4;

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ProgressEvent {
    pub phase: &'static str,
    pub done: usize,
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
}

pub type ProgressFn = Arc<dyn Fn(ProgressEvent) + Send + Sync>;

#[derive(Clone)]
pub struct WorkingCopyOptions {
    pub root: PathBuf,
    pub api: Arc<ApiClient>,
    /// 省略时取 wc.db 里记录的仓库名（CLI 场景）。
    pub repo: Option<String>,
    /// 检出者用户名，记进 meta 便于 `status` 展示。
    pub user: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub concurrency: Option<usize>,
}

pub struct CheckoutOptions {
    pub root: PathBuf,
    pub api: Arc<ApiClient>,
    /// 检出必须明确指定仓库（没有 meta 可回退）。
    pub repo: String,
    pub user: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub concurrency: Option<usize>,
    /// 部分检出的前缀列表；空数组 = 全量。
    pub sparse: Vec<String>,
}

#[derive(Default, Clone)]
pub struct CommitOptions {
    pub message: Option<String>,
    /// 只提交这些路径（默认提交全部本地改动）。
    pub paths: Option<Vec<String>>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct CommitOutcome {
    pub rev: i64,
    /// 实际进入这次提交的路径。
    pub committed: Vec<String>,
    pub replayed: bool,
}

/// 三方对比里某一侧的可用形态。
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SideKind {
    Text,
    Binary,
    TooLarge,
    Missing,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct SideContent {
    pub kind: SideKind,
    pub size: i64,
    /// 只有 `kind == Text` 时非空。
    pub text: Option<String>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ConflictSidesSummary {
    pub base: SideKind,
    pub mine: SideKind,
    pub theirs: SideKind,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ConflictInfo {
    pub path: String,
    pub kind: EntryKind,
    pub reason: ConflictReason,
    /// 本地版本（`<path>.mine`）是否还在。
    pub has_mine: bool,
    /// 服务端版本是否可取得（服务端已删除时为 false）。
    pub has_theirs: bool,
    /// 三方都是文本才允许做合并式解决。
    pub mergeable: bool,
    /// 服务端版本对应修订。
    pub theirs_rev: i64,
    /// 各侧判定（界面据此给不同提示，不传内容）。
    pub sides: ConflictSidesSummary,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ConflictSides {
    pub path: String,
    pub mergeable: bool,
    pub base: SideContent,
    pub mine: SideContent,
    pub theirs: SideContent,
}

/// 冲突解决方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    Mine,
    Theirs,
    Merged,
}

impl ConflictResolution {
    pub fn parse(s: &str) -> ClientResult<Self> {
        match s {
            "mine" => Ok(Self::Mine),
            "theirs" => Ok(Self::Theirs),
            "merged" => Ok(Self::Merged),
            other => Err(ClientError::new(
                "BAD_REQUEST",
                format!("choice 取值非法：{other}"),
            )),
        }
    }
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct UpdateOutcome {
    pub rev: i64,
    pub updated: Vec<String>,
    pub deleted: Vec<String>,
    pub conflicts: Vec<String>,
    pub skipped: Vec<String>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_secs() -> i64 {
    now_ms() / 1000
}

pub struct WorkingCopy {
    pub wc: Wc,
    pub api: Arc<ApiClient>,
    pub repo: String,
    pub cache_dir: PathBuf,
    pub concurrency: usize,
    process_lock: Option<ProcessLock>,
}

impl WorkingCopy {
    fn assemble(
        wc: Wc,
        api: Arc<ApiClient>,
        repo: String,
        cache_dir: Option<PathBuf>,
        concurrency: Option<usize>,
    ) -> Self {
        Self {
            wc,
            api,
            repo,
            cache_dir: cache_dir.unwrap_or_else(default_cache_dir),
            concurrency: concurrency.unwrap_or(DEFAULT_CONCURRENCY).max(1),
            process_lock: None,
        }
    }

    /// 检出（支持部分检出）。
    pub async fn checkout(
        opts: CheckoutOptions,
        on_progress: Option<ProgressFn>,
    ) -> ClientResult<Self> {
        let root = opts.root.clone();
        if root.join(WC_DIR).join(WC_DB).exists() {
            return Err(ClientError::wc(
                "NOT_A_WORKING_COPY",
                format!("目录已是工作副本，不能重复检出：{}", root.display()),
            ));
        }
        let info = opts.api.repo_info(&opts.repo).await?;
        let head_rev = info.get("head_rev").and_then(|v| v.as_i64()).unwrap_or(0);
        let sparse: Vec<String> = opts.sparse.iter().map(|s| normalize_nfc(s)).collect();
        let cache_dir = opts.cache_dir.clone().unwrap_or_else(default_cache_dir);

        std::fs::create_dir_all(&root)?;
        let mut wc = Wc::init(
            &root,
            &crate::engine::db::WcMeta {
                server: opts.api.base_url.clone(),
                repo: opts.repo.clone(),
                revision: head_rev,
                sparse_paths: sparse.clone(),
                user: opts.user.clone().unwrap_or_default(),
                cache_dir: cache_dir.to_string_lossy().to_string(),
            },
        )?;
        // 进程锁尽早拿：后面几百个文件的下载期间，另一个实例不该能插手同一副本
        let process_lock = acquire_process_lock(&root)?;
        let concurrency = opts.concurrency.unwrap_or(DEFAULT_CONCURRENCY).max(1);

        // ① 拉目录树（每个部分检出前缀一次；空数组表示全量根）
        let prefixes: Vec<String> = if sparse.is_empty() {
            vec![String::new()]
        } else {
            sparse.clone()
        };
        let mut all = Vec::new();
        for p in &prefixes {
            let t = opts
                .api
                .tree(&opts.repo, None, if p.is_empty() { None } else { Some(p) }, Some(TREE_DEPTH))
                .await?;
            all.extend(t.items);
        }
        if let Some(cb) = &on_progress {
            cb(ProgressEvent {
                phase: "tree",
                done: all.len(),
                total: all.len(),
                current: None,
            });
        }

        // ② 目录先建，避免文件安装时父目录不存在
        //
        // 除树里显式返回的目录外，还要补上**部分检出前缀本身与其祖先**：服务端
        // `tree?prefix=X` 返回的是 X 的**子项**（不含 X），不补的话前缀目录
        // 会出现在磁盘上却没有基线条目，状态机把它报成"未纳管"。
        let mut dir_set: HashSet<String> = all
            .iter()
            .filter(|e| e.kind == "dir")
            .map(|e| e.path.clone())
            .collect();
        for p in &prefixes {
            let mut a = p.as_str();
            while !a.is_empty() {
                dir_set.insert(a.to_string());
                a = parent_of(a).unwrap_or("");
            }
        }
        for f in all.iter().filter(|e| e.kind == "file") {
            let mut a = parent_of(&f.path).unwrap_or("");
            while !a.is_empty() {
                dir_set.insert(a.to_string());
                a = parent_of(a).unwrap_or("");
            }
        }
        let mut dirs: Vec<String> = dir_set.into_iter().collect();
        dirs.sort_by_key(|d| d.split('/').count());
        for d in &dirs {
            std::fs::create_dir_all(join_rel(&root, d))?;
        }

        // ③ 文件：下载缺失 blob → 入缓存 → pristine 硬链接 → 安装到工作副本
        let files: Vec<(String, String)> = all
            .iter()
            .filter(|e| e.kind == "file")
            .filter_map(|e| e.blob_hash.clone().map(|h| (e.path.clone(), h)))
            .collect();
        let installer = Installer {
            api: opts.api.clone(),
            repo: opts.repo.clone(),
            root: root.clone(),
            cache_dir: cache_dir.clone(),
        };
        let total = files.len();
        let counter = std::sync::atomic::AtomicUsize::new(0);
        let progress_cb = on_progress.clone();
        let mut stream = futures_util::stream::iter(files.iter().cloned())
            .map(|(rel, hash)| {
                let installer = &installer;
                let counter = &counter;
                let cb = progress_cb.clone();
                async move {
                    installer.install_file(&rel, &hash).await?;
                    let done = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    if let Some(cb) = cb {
                        cb(ProgressEvent {
                            phase: "download",
                            done,
                            total,
                            current: Some(rel),
                        });
                    }
                    Ok::<(), ClientError>(())
                }
            })
            .buffer_unordered(concurrency);
        while let Some(r) = stream.next().await {
            r?;
        }

        // ④ 落基线：stat 必须在事务外做完（事务回调里不做 I/O）
        let mut file_stats: Vec<(String, Option<String>, i64, i64, i64)> = Vec::new();
        for (rel, hash) in &files {
            let abs = join_rel(&root, rel);
            let meta = std::fs::metadata(&abs)?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            file_stats.push((
                rel.clone(),
                Some(hash.clone()),
                meta.len() as i64,
                mtime,
                mode_bits(&meta),
            ));
        }
        wc.tx(|w| {
            for d in &dirs {
                w.upsert_entry(&EntryRow {
                    path: d.clone(),
                    kind: EntryKind::Dir,
                    base_rev: head_rev,
                    base_hash: None,
                    size: 0,
                    mtime_ms: 0,
                    mode: 0o755,
                    status: EntryStatus::Normal,
                    case_conflict: false,
                })?;
            }
            for (rel, hash, size, mtime, mode) in &file_stats {
                w.upsert_entry(&EntryRow {
                    path: rel.clone(),
                    kind: EntryKind::File,
                    base_rev: head_rev,
                    base_hash: hash.clone(),
                    size: *size,
                    mtime_ms: *mtime,
                    mode: *mode,
                    status: EntryStatus::Normal,
                    case_conflict: false,
                })?;
            }
            Ok(())
        })?;

        let mut self_ = Self::assemble(
            wc,
            opts.api.clone(),
            opts.repo.clone(),
            opts.cache_dir.clone(),
            Some(concurrency),
        );
        self_.process_lock = Some(process_lock);
        Ok(self_)
    }

    /// 打开既有工作副本。
    ///
    /// `repo` 省略时以 wc.db 里记录的为准；若 meta 里的服务端地址与传入 client 不同，
    /// 按 meta 重建 client（工作副本可能是在另一台服务器上检出的）。
    pub fn open(opts: WorkingCopyOptions) -> ClientResult<Self> {
        let wc = Wc::open(&opts.root)?;
        let meta = wc.get_meta()?;
        let repo = opts.repo.clone().filter(|r| !r.is_empty()).unwrap_or(meta.repo);
        let api = if !meta.server.is_empty() && meta.server != opts.api.base_url {
            Arc::new(ApiClient::new(
                &meta.server,
                opts.api.token_value().map(|s| s.to_string()),
                opts.api.transport(),
            ))
        } else {
            opts.api.clone()
        };
        let mut self_ = Self::assemble(wc, api, repo, opts.cache_dir.clone(), opts.concurrency);
        self_.process_lock = Some(acquire_process_lock(&opts.root)?);
        Ok(self_)
    }

    pub fn close(&mut self) {
        if let Some(l) = self.take_lock() {
            l.release();
        }
    }

    fn take_lock(&mut self) -> Option<ProcessLock> {
        self.process_lock.take()
    }

    pub fn revision(&self) -> i64 {
        self.wc.revision()
    }

    pub fn sparse_paths(&self) -> Vec<String> {
        self.wc.sparse_paths()
    }

    pub fn abs_of(&self, rel: &str) -> PathBuf {
        let mut p = self.root().to_path_buf();
        for seg in rel.split('/') {
            if !seg.is_empty() {
                p.push(seg);
            }
        }
        p
    }

    fn root(&self) -> &Path {
        &self.wc.root
    }

    // ---------- 内部：blob 与文件安装 ----------

    /// 确保 blob 在缓存与 pristine 中，然后把内容安装到工作文件。
    async fn install_blob(&mut self, hash: &str) -> ClientResult<()> {
        if !cache_path_for(hash, &self.cache_dir).exists() {
            let tmp = self
                .cache_dir
                .join("tmp")
                .join(format!("{hash}.dl-{}", std::process::id()));
            std::fs::create_dir_all(tmp.parent().unwrap_or(&self.cache_dir))?;
            self.api.download_blob_to(&self.repo, hash, &tmp).await?;
            store_to_cache(&tmp, hash, &self.cache_dir)?;
        }
        ensure_pristine(self.root(), hash, &self.cache_dir)?;
        Ok(())
    }

    /// 安装服务端某个修订的文件内容到工作副本。
    async fn install_file(&mut self, rel: &str, hash: &str) -> ClientResult<()> {
        self.install_blob(hash).await?;
        install_working_file(self.root(), hash, &self.abs_of(rel))?;
        Ok(())
    }

    fn write_entry_from_disk(&mut self, rel: &str, hash: &str, rev: i64) -> ClientResult<()> {
        let abs = self.abs_of(rel);
        let meta = std::fs::metadata(&abs)?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.wc.upsert_entry(&EntryRow {
            path: rel.to_string(),
            kind: EntryKind::File,
            base_rev: rev,
            base_hash: Some(hash.to_string()),
            size: meta.len() as i64,
            mtime_ms: mtime,
            mode: mode_bits(&meta),
            status: EntryStatus::Normal,
            case_conflict: false,
        })
    }

    /// 记录一个"内容与基线**不同**"的本地改动条目。
    ///
    /// `mtime_ms` 必须置 0：状态判定的快速路径是"mtime + size 都没变 ⇒ 内容没变"（§6.1），
    /// 而我们刚把本地内容装回工作文件、磁盘 mtime 是新的；如果照实记进条目，
    /// 下一次 status 会直接报 `normal`——**冲突解决完了却提交不了**。
    fn write_local_change_entry(
        &mut self,
        rel: &str,
        kind: EntryKind,
        size: i64,
        mode: i64,
        base_rev: i64,
        base_hash: Option<String>,
    ) -> ClientResult<()> {
        self.wc.upsert_entry(&EntryRow {
            path: rel.to_string(),
            kind,
            base_rev,
            base_hash,
            size,
            mtime_ms: 0,
            mode,
            status: EntryStatus::Modified,
            case_conflict: false,
        })
    }

    // ---------- 状态 ----------

    pub fn status(&self, force_hash: bool) -> ClientResult<Vec<StatusItem>> {
        compute_status(
            &self.wc,
            &StatusOptions {
                force_hash,
                server_changed: None,
            },
        )
    }

    /// 加入待提交集（新文件必须先 add 才会出现在提交候选里）。
    pub fn add(&mut self, paths: &[String]) -> ClientResult<usize> {
        let mut n = 0;
        self.wc.tx(|w| {
            for raw in paths {
                let p = normalize_nfc(raw);
                if let Some(bad) = validate_path(&p) {
                    return Err(ClientError::wc("PATH_INVALID", bad).with_details(
                        serde_json::json!({ "path": p }),
                    ));
                }
                // 隐式祖先目录随子路径一起纳入版本控制（svn 语义，与 update 端的
                // ensureAncestorEntries 对称）：否则 `add a/b.psd` 之后 `a` 仍是 unversioned。
                // 目录本身不进 changes——服务端 ensure_dirs 自补。
                let mut ancs: Vec<String> = Vec::new();
                let mut a = parent_of(&p).map(|s| s.to_string());
                while let Some(cur) = a {
                    if w.has_entry(&cur)? {
                        break;
                    }
                    ancs.push(cur.clone());
                    a = parent_of(&cur).map(|s| s.to_string());
                }
                ancs.reverse();
                for a in ancs {
                    w.stage(&a, PendingOp::Add)?;
                }
                let op = if w.has_entry(&p)? {
                    PendingOp::Modify
                } else {
                    PendingOp::Add
                };
                w.stage(&p, op)?;
                n += 1;
            }
            Ok(())
        })?;
        Ok(n)
    }

    /// 在工作副本里新建目录（§6.5：树上直接增删）。
    ///
    /// 目录不参与提交（不进 `changes`，服务端 `ensure_dirs` 隐式补建），所以**不 stage**。
    pub async fn mkdir(&mut self, path: &str) -> ClientResult<()> {
        let p = self.require_safe_path(path)?;
        let abs = self.abs_of(&p);
        tokio::task::spawn_blocking(move || {
            with_retry(RetryOptions::default(), || std::fs::create_dir_all(&abs))
        })
        .await
        .map_err(|e| ClientError::new("IO", e.to_string()))??;
        Ok(())
    }

    /// 路径校验 + NFC 归一化（新建类操作共用的入口）。
    fn require_safe_path(&self, raw: &str) -> ClientResult<String> {
        let p = normalize_nfc(raw);
        if let Some(bad) = validate_path(&p) {
            return Err(
                ClientError::wc("PATH_INVALID", bad).with_details(serde_json::json!({ "path": p }))
            );
        }
        if p.is_empty() {
            return Err(ClientError::wc("PATH_INVALID", "路径不能为空"));
        }
        Ok(p)
    }

    /// 删除：纳入待提交集（已纳管），或干脆当作本地文件删掉（未纳管）。
    ///
    /// 三种情况必须分开（"记一条待提交的删除"只对第一种成立）：
    /// 1. **有基线条目**（已纳管）→ 记一条待删除，文件立即移除；
    /// 2. **只在待提交集里**（`add` 过、还没提交）→ 撤销那条 `add`；
    /// 3. **完全陌生**（未纳管）→ 什么都不记，删掉文件即可。
    pub async fn remove(&mut self, paths: &[String]) -> ClientResult<()> {
        self.wc.tx(|w| {
            for raw in paths {
                let p = normalize_nfc(raw);
                if w.has_entry(&p)? {
                    w.stage(&p, PendingOp::Delete)?;
                } else {
                    w.unstage(&p)?;
                }
            }
            Ok(())
        })?;
        for raw in paths {
            let abs = self.abs_of(&normalize_nfc(raw));
            tokio::task::spawn_blocking(move || remove_path(&abs))
                .await
                .map_err(|e| ClientError::new("IO", e.to_string()))??;
        }
        Ok(())
    }

    /// 读本副本的忽略规则文件（`.b-artifactignore`，不存在等价于空规则）。
    pub fn read_ignore_file(&self) -> String {
        std::fs::read_to_string(self.root().join(IGNORE_FILE)).unwrap_or_default()
    }

    /// 写本副本的忽略规则文件；**内容清空 = 删除该文件**（留个空文件只会在树上多一条噪音）。
    pub async fn write_ignore_file(&mut self, content: &str) -> ClientResult<()> {
        let file = self.root().join(IGNORE_FILE);
        if content.trim().is_empty() {
            tokio::task::spawn_blocking(move || {
                with_retry(RetryOptions::default(), || match std::fs::remove_file(&file) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e),
                })
            })
            .await
            .map_err(|e| ClientError::new("IO", e.to_string()))??;
            return Ok(());
        }
        let content = content.to_string();
        tokio::task::spawn_blocking(move || {
            with_retry(RetryOptions::default(), || std::fs::write(&file, &content))
        })
        .await
        .map_err(|e| ClientError::new("IO", e.to_string()))??;
        Ok(())
    }

    /// 还原：把工作文件恢复成基线内容（离线可用，§6.1）。
    pub async fn revert(&mut self, paths: &[String]) -> ClientResult<()> {
        for raw in paths {
            let p = normalize_nfc(raw);
            let e = self.wc.get_entry(&p)?.ok_or_else(|| {
                ClientError::wc("NOT_FOUND", format!("路径不在版本控制下：{p}"))
            })?;
            if e.kind == EntryKind::File {
                if let Some(hash) = e.base_hash.clone() {
                    ensure_pristine(self.root(), &hash, &self.cache_dir)?;
                    install_working_file(self.root(), &hash, &self.abs_of(&p))?;
                    self.write_entry_from_disk(&p, &hash, e.base_rev)?;
                }
            }
            // 还原 = 放弃本地改动，冲突随之解除（`.mine` 与冲突登记一起清掉）
            let mine = PathBuf::from(format!("{}{}", self.abs_of(&p).to_string_lossy(), MINE_SUFFIX));
            tokio::task::spawn_blocking(move || match std::fs::remove_file(&mine) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            })
            .await
            .map_err(|e| ClientError::new("IO", e.to_string()))??;
            self.wc.tx(|w| {
                w.unstage(&p)?;
                w.remove_conflict(&p)
            })?;
        }
        Ok(())
    }

    // ---------- 提交（§6.3 / §7.3） ----------

    pub async fn commit(
        &mut self,
        opts: CommitOptions,
        on_progress: Option<ProgressFn>,
    ) -> ClientResult<CommitOutcome> {
        let base_rev = self.wc.revision();
        let status = self.status(false)?;
        let want: HashSet<String> = opts
            .paths
            .clone()
            .unwrap_or_default()
            .iter()
            .map(|p| normalize_nfc(p))
            .collect();
        let matches = |p: &str| -> bool {
            want.is_empty() || want.contains(p) || want.iter().any(|w| p.starts_with(&format!("{w}/")))
        };
        let mut local: Vec<StatusItem> = status
            .iter()
            .filter(|s| is_local_change(s.status) && matches(&s.path))
            .cloned()
            .collect();
        // v0.4.17（§6.5）：未纳管文件**自动算新增**，但只有被显式勾选才进提交集。
        if !want.is_empty() {
            for s in status.iter() {
                // 目录不进来：它在服务端由 `ensure_dirs` 随文件隐式补建。
                if s.status == crate::engine::scan::StatusCode::Unversioned
                    && s.kind == EntryKind::File
                    && matches(&s.path)
                {
                    let mut added = s.clone();
                    added.status = crate::engine::scan::StatusCode::Added;
                    local.push(added);
                }
            }
        }
        // 显式 stage 的删除即使文件已不在磁盘也要带上
        for p in self.wc.all_pending()? {
            if p.op == PendingOp::Delete && !local.iter().any(|s| s.path == p.path) {
                let e = self.wc.get_entry(&p.path)?;
                local.push(StatusItem {
                    path: p.path.clone(),
                    kind: e.as_ref().map(|x| x.kind).unwrap_or(EntryKind::File),
                    status: crate::engine::scan::StatusCode::Deleted,
                    base_rev: e.as_ref().map(|x| x.base_rev).unwrap_or(base_rev),
                    base_hash: e.as_ref().and_then(|x| x.base_hash.clone()),
                    size: e.as_ref().map(|x| x.size).unwrap_or(0),
                    needs_update: false,
                });
            }
        }
        // `missing`（磁盘上被直接删掉、没走 `remove`）按 svn 1.7+ 的语义在提交时记为删除。
        for s in &mut local {
            if s.status == crate::engine::scan::StatusCode::Missing {
                s.status = crate::engine::scan::StatusCode::Deleted;
            }
        }

        if local.is_empty() {
            return Err(ClientError::wc("NO_CHANGES", "没有需要提交的变更"));
        }
        // 只有目录变更（例如 add 了一个空目录）时 changes 会是空的：空目录在服务端没有
        // 独立实体，提交上去只会换来一句看不懂的服务端报错，这里直接说清楚。
        if local
            .iter()
            .all(|s| s.kind == EntryKind::Dir && s.status != crate::engine::scan::StatusCode::Deleted)
        {
            return Err(ClientError::wc(
                "NO_CHANGES",
                "只有目录变更：空目录不会单独入库，请先放入文件",
            ));
        }
        if let Some(c) = local
            .iter()
            .find(|s| s.status == crate::engine::scan::StatusCode::Conflicted)
        {
            let _ = c;
            return Err(ClientError::wc("CONFLICT", "存在冲突，请先解决冲突再提交").with_details(
                serde_json::json!({
                    "paths": local.iter()
                        .filter(|s| s.status == crate::engine::scan::StatusCode::Conflicted)
                        .map(|s| s.path.clone())
                        .collect::<Vec<_>>()
                }),
            ));
        }

        // ② 计算待上传文件的内容哈希（有界并发）
        let to_hash: Vec<String> = local
            .iter()
            .filter(|s| {
                (s.status == crate::engine::scan::StatusCode::Modified
                    || s.status == crate::engine::scan::StatusCode::Added)
                    && s.kind == EntryKind::File
            })
            .map(|s| s.path.clone())
            .collect();
        let total_for_hash = local.len();
        let counter = std::sync::atomic::AtomicUsize::new(0);
        let mut uploads: Vec<(String, PathBuf, String, i64, i64)> = Vec::new();
        {
            let root = self.root().to_path_buf();
            let cb = on_progress.clone();
            let mut results = futures_util::stream::iter(to_hash.clone())
                    .map(|rel| {
                        let root = root.clone();
                        let counter = &counter;
                        let cb = cb.clone();
                        async move {
                            let abs = join_rel(&root, &rel);
                            let hash = tokio::task::spawn_blocking({
                                let abs = abs.clone();
                                move || hash_file(&abs)
                            })
                            .await
                            .map_err(|e| ClientError::new("IO", e.to_string()))??;
                            let meta = std::fs::metadata(&abs)?;
                            let done =
                                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                            if let Some(cb) = cb {
                                cb(ProgressEvent {
                                    phase: "hash",
                                    done,
                                    total: total_for_hash,
                                    current: Some(rel.clone()),
                                });
                            }
                            Ok::<_, ClientError>((rel, abs, hash, meta.len() as i64, mode_bits(&meta)))
                        }
                    })
                    .buffer_unordered(self.concurrency)
                    .collect::<Vec<_>>()
                    .await;
            for r in results.drain(..) {
                uploads.push(r?);
            }
        }

        // ③ 变更清单
        let mut changes: Vec<ChangeSpec> = Vec::new();
        for s in &local {
            if s.status == crate::engine::scan::StatusCode::Deleted {
                changes.push(ChangeSpec {
                    path: s.path.clone(),
                    op: "delete".into(),
                    kind: s.kind.as_str().into(),
                    blob_hash: None,
                    size: None,
                    mode: None,
                    mtime: None,
                });
                continue;
            }
            if s.kind == EntryKind::Dir {
                continue; // 目录由服务端隐式补建，不进 changes
            }
            let u = uploads
                .iter()
                .find(|u| u.0 == s.path)
                .ok_or_else(|| ClientError::wc("IO", format!("缺少上传条目：{}", s.path)))?;
            changes.push(ChangeSpec {
                path: s.path.clone(),
                op: if s.status == crate::engine::scan::StatusCode::Added {
                    "add".into()
                } else {
                    "modify".into()
                },
                kind: "file".into(),
                blob_hash: Some(u.2.clone()),
                size: Some(u.3),
                mode: Some(u.4),
                mtime: Some(now_secs()),
            });
        }

        // 提交涉及的文件路径（目录不参与锁模型）：提交成功后按这批路径解锁
        let committed_files: Vec<String> = local
            .iter()
            .filter(|s| s.kind == EntryKind::File)
            .map(|s| s.path.clone())
            .collect();

        let commit_id = uuid::Uuid::new_v4().to_string();
        let (prep, _auto) = self
            .prepare_with_auto_lock(&commit_id, base_rev, &changes, &opts, on_progress.clone())
            .await?;

        if prep.replayed {
            if let Some(rev) = prep.rev {
                self.apply_committed_revision(rev, &local, &uploads).await?;
                self.release_committed_locks(&committed_files).await;
                return Ok(CommitOutcome {
                    rev,
                    committed: local.iter().map(|s| s.path.clone()).collect(),
                    replayed: true,
                });
            }
        }
        let Some(token) = prep.commit_token.clone() else {
            return Err(ClientError::wc("IO", "服务端未签发 commit_token"));
        };

        // ④ 上传缺失 blob
        let need: HashSet<String> = prep.need_blobs.clone().unwrap_or_default().into_iter().collect();
        let todo: Vec<(String, PathBuf, String, i64, i64)> = uploads
            .iter()
            .filter(|u| need.contains(&u.2))
            .cloned()
            .collect();
        let total_up = todo.len();
        let up_counter = std::sync::atomic::AtomicUsize::new(0);
        let uploaded = {
            let api = self.api.clone();
            let repo = self.repo.clone();
            let cb = on_progress.clone();
            let mut stream = futures_util::stream::iter(todo.clone())
                .map(|(rel, abs, hash, size, _mode)| {
                    let api = api.clone();
                    let repo = repo.clone();
                    let cb = cb.clone();
                    let counter = &up_counter;
                    async move {
                        upload_blob(&api, &repo, &abs, &hash, size).await?;
                        let done = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        if let Some(cb) = cb {
                            cb(ProgressEvent {
                                phase: "upload",
                                done,
                                total: total_up,
                                current: Some(rel),
                            });
                        }
                        Ok::<(), ClientError>(())
                    }
                })
                .buffer_unordered(self.concurrency);
            while let Some(r) = stream.next().await {
                r?;
            }
        };
        let _ = uploaded;

        // ⑤ 落库（commit_id 幂等，网络重试安全）
        let message = opts.message.clone().unwrap_or_default();
        let res = match self.api.commit(&self.repo, &commit_id, &token, &message).await {
            Ok(r) => r,
            Err(e) => {
                // 响应可能在服务端**已提交之后**丢失。此时若换个 commit_id 重来就会产出
                // 一笔重复提交；用同一个 commit_id 再问一次 prepare：已应用则直接拿到
                // replayed + rev，未应用则重新签发 token 后继续。
                if e.code != "NETWORK" {
                    return Err(e);
                }
                let again = self
                    .api
                    .prepare_commit(&self.repo, &commit_id, base_rev, &message, &changes)
                    .await?;
                if again.replayed {
                    if let Some(rev) = again.rev {
                        self.apply_committed_revision(rev, &local, &uploads).await?;
                        self.release_committed_locks(&committed_files).await;
                        return Ok(CommitOutcome {
                            rev,
                            committed: local.iter().map(|s| s.path.clone()).collect(),
                            replayed: true,
                        });
                    }
                }
                let Some(token2) = again.commit_token.clone() else {
                    return Err(e);
                };
                self.api
                    .commit(&self.repo, &commit_id, &token2, &message)
                    .await?
            }
        };

        self.apply_committed_revision(res.rev, &local, &uploads).await?;
        self.release_committed_locks(&committed_files).await;
        Ok(CommitOutcome {
            rev: res.rev,
            committed: local.iter().map(|s| s.path.clone()).collect(),
            replayed: res.replayed,
        })
    }

    /// prepare，并在服务端要求"先锁后提交"时**自动补锁**（§5.2 + §6.5）。
    ///
    /// 为什么不是提交前先查一遍锁：那要为每个变更路径发一次 `GET /locks`（N 次往返），
    /// 而服务端在 412 里已经把**未持锁的路径清单**给了我们。
    ///
    /// 加锁**串行**且第一个失败即抛：失败几乎总是"被别人持锁"，此时用户需要知道
    /// 是谁占着（`details` 带 owner）。
    async fn prepare_with_auto_lock(
        &mut self,
        commit_id: &str,
        base_rev: i64,
        changes: &[ChangeSpec],
        opts: &CommitOptions,
        on_progress: Option<ProgressFn>,
    ) -> ClientResult<(PrepareResult, Vec<String>)> {
        let message = opts.message.clone().unwrap_or_default();
        match self
            .api
            .prepare_commit(&self.repo, commit_id, base_rev, &message, changes)
            .await
        {
            Ok(p) => Ok((p, Vec::new())),
            Err(e) => {
                if e.code != "NEEDS_LOCK" {
                    return Err(e);
                }
                let missing: Vec<String> = e
                    .details
                    .as_ref()
                    .and_then(|d| d.get("paths"))
                    .and_then(|p| p.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                if missing.is_empty() {
                    return Err(e);
                }
                let total = missing.len();
                for (i, p) in missing.iter().enumerate() {
                    self.lock(p, None).await?;
                    if let Some(cb) = &on_progress {
                        cb(ProgressEvent {
                            phase: "lock",
                            done: i + 1,
                            total,
                            current: Some(p.clone()),
                        });
                    }
                }
                let prep = self
                    .api
                    .prepare_commit(&self.repo, commit_id, base_rev, &message, changes)
                    .await?;
                Ok((prep, missing))
            }
        }
    }

    /// 提交成功后自动解锁：**本次提交涉及的文件上、由本人持有的锁，全部释放**（§6.5）。
    ///
    /// 只动"本次提交涉及的路径"；释放失败不影响提交结果。
    async fn release_committed_locks(&mut self, paths: &[String]) {
        for p in paths {
            // 只解"我确实持有"的那些（本地锁表是权威：没锁就别白跑一趟网络）
            if self.wc.get_lock(p).ok().flatten().is_none() {
                continue;
            }
            let _ = self.unlock(p, false, None).await;
        }
    }

    /// 提交成功后：刷新基线、把新内容纳入缓存、清理待提交集。
    async fn apply_committed_revision(
        &mut self,
        rev: i64,
        local: &[StatusItem],
        uploads: &[(String, PathBuf, String, i64, i64)],
    ) -> ClientResult<()> {
        for (_, abs, hash, _, _) in uploads {
            import_working_file(abs, &self.cache_dir, hash)?;
            ensure_pristine(self.root(), hash, &self.cache_dir)?;
        }
        let root = self.root().to_path_buf();
        let cache = self.cache_dir.clone();
        // 落库前的 stat 在事务外算好（事务回调里不做 I/O）
        let mut stats: HashMap<String, (i64, i64, i64)> = HashMap::new();
        for s in local {
            if s.status == crate::engine::scan::StatusCode::Deleted || s.kind == EntryKind::Dir {
                continue;
            }
            if let Ok(meta) = std::fs::metadata(join_rel(&root, &s.path)) {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(now_ms());
                stats.insert(s.path.clone(), (meta.len() as i64, mtime, mode_bits(&meta)));
            }
        }
        let _ = cache;
        self.wc.tx(|w| {
            for s in local {
                if s.status == crate::engine::scan::StatusCode::Deleted {
                    w.delete_entry(&s.path)?;
                    w.unstage(&s.path)?;
                    continue;
                }
                // 补登记祖先目录（与 TS 版一致）
                let mut a = parent_of(&s.path).map(|x| x.to_string());
                while let Some(p) = a {
                    if w.has_entry(&p)? {
                        break;
                    }
                    w.upsert_entry(&EntryRow {
                        path: p.clone(),
                        kind: EntryKind::Dir,
                        base_rev: rev,
                        base_hash: None,
                        size: 0,
                        mtime_ms: 0,
                        mode: 0o755,
                        status: EntryStatus::Normal,
                        case_conflict: false,
                    })?;
                    a = parent_of(&p).map(|x| x.to_string());
                }
                if s.kind == EntryKind::Dir {
                    w.upsert_entry(&EntryRow {
                        path: s.path.clone(),
                        kind: EntryKind::Dir,
                        base_rev: rev,
                        base_hash: None,
                        size: 0,
                        mtime_ms: now_ms(),
                        mode: 0o755,
                        status: EntryStatus::Normal,
                        case_conflict: false,
                    })?;
                    w.unstage(&s.path)?;
                    continue;
                }
                let u = uploads
                    .iter()
                    .find(|u| u.0 == s.path)
                    .ok_or_else(|| ClientError::wc("IO", format!("缺少上传条目：{}", s.path)))?;
                w.upsert_entry(&EntryRow {
                    path: s.path.clone(),
                    kind: EntryKind::File,
                    base_rev: rev,
                    base_hash: Some(u.2.clone()),
                    size: u.3,
                    mtime_ms: now_ms(),
                    mode: u.4,
                    status: EntryStatus::Normal,
                    case_conflict: false,
                })?;
                w.unstage(&s.path)?;
            }
            w.set_revision(rev)
        })?;
        // 落库后校准 mtime/size 成磁盘真实值（否则下次 status 必定重算哈希）
        for s in local {
            if s.status == crate::engine::scan::StatusCode::Deleted || s.kind == EntryKind::Dir {
                continue;
            }
            if let Some((_, _, _)) = stats.get(&s.path) {
                if let Some(u) = uploads.iter().find(|u| u.0 == s.path) {
                    self.write_entry_from_disk(&s.path, &u.2, rev)?;
                }
            }
        }
        Ok(())
    }

    // ---------- 更新（§6.3） ----------

    pub async fn update(&mut self, on_progress: Option<ProgressFn>) -> ClientResult<UpdateOutcome> {
        let info = self.api.repo_info(&self.repo).await?;
        let head = info.get("head_rev").and_then(|v| v.as_i64()).unwrap_or(0);
        let from = self.wc.revision();
        if head <= from {
            return Ok(UpdateOutcome {
                rev: from,
                updated: vec![],
                deleted: vec![],
                conflicts: vec![],
                skipped: vec![],
            });
        }

        let prefixes: Vec<String> = {
            let sp = self.sparse_paths();
            if sp.is_empty() {
                vec![String::new()]
            } else {
                sp
            }
        };
        let mut rows = Vec::new();
        for p in &prefixes {
            let mut r = self
                .api
                .changes(&self.repo, from + 1, Some(head), if p.is_empty() { None } else { Some(p) })
                .await?;
            rows.append(&mut r);
        }

        // 折叠：同一路径只保留最后一条（changes 已按 rev 升序）
        let mut latest: HashMap<String, crate::engine::api::ChangeRow> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for r in rows {
            if !latest.contains_key(&r.path) {
                order.push(r.path.clone());
            }
            latest.insert(r.path.clone(), r);
        }

        let local_status = self.status(false)?;
        let modified: HashSet<String> = local_status
            .iter()
            .filter(|s| is_local_change(s.status))
            .map(|s| s.path.clone())
            .collect();

        let mut outcome = UpdateOutcome {
            rev: head,
            updated: vec![],
            deleted: vec![],
            conflicts: vec![],
            skipped: vec![],
        };

        let total = order.len();
        for (i, path) in order.iter().enumerate() {
            let ch = latest.get(path).cloned().unwrap();
            if let Some(cb) = &on_progress {
                cb(ProgressEvent {
                    phase: "apply",
                    done: i + 1,
                    total,
                    current: Some(path.clone()),
                });
            }

            if ch.op == "delete" {
                if modified.contains(path) {
                    // 本地改过、服务端删了：保留本地文件，标记为冲突交给人处理（绝不静默丢数据）
                    let prev = self.wc.get_entry(path)?;
                    let mine_hash = self.hash_of_mine(path, false).await;
                    let kind = prev.as_ref().map(|p| p.kind).unwrap_or(EntryKind::File);
                    let base_hash = prev.as_ref().and_then(|p| p.base_hash.clone());
                    self.wc.tx(|w| {
                        if let Some(p) = &prev {
                            let mut row = p.clone();
                            row.status = EntryStatus::Conflicted;
                            w.upsert_entry(&row)?;
                        }
                        w.put_conflict(&ConflictRow {
                            path: path.clone(),
                            kind,
                            base_hash,
                            theirs_hash: None,
                            mine_hash,
                            theirs_rev: ch.rev,
                            reason: ConflictReason::DeletedRemotely,
                            created_at: now_ms(),
                        })
                    })?;
                    outcome.conflicts.push(path.clone());
                    continue;
                }
                let abs = self.abs_of(path);
                tokio::task::spawn_blocking(move || remove_path(&abs))
                    .await
                    .map_err(|e| ClientError::new("IO", e.to_string()))??;
                self.wc.tx(|w| w.delete_entry(path))?;
                outcome.deleted.push(path.clone());
                continue;
            }

            if ch.kind == "dir" {
                let abs = self.abs_of(path);
                tokio::task::spawn_blocking(move || std::fs::create_dir_all(&abs))
                    .await
                    .map_err(|e| ClientError::new("IO", e.to_string()))??;
                let p = path.clone();
                self.wc.tx(|w| {
                    w.upsert_entry(&EntryRow {
                        path: p,
                        kind: EntryKind::Dir,
                        base_rev: ch.rev,
                        base_hash: None,
                        size: 0,
                        mtime_ms: 0,
                        mode: 0o755,
                        status: EntryStatus::Normal,
                        case_conflict: false,
                    })
                })?;
                continue;
            }

            let Some(theirs_hash) = ch.blob_hash.clone() else {
                outcome.skipped.push(path.clone());
                continue;
            };

            let base = self.wc.get_entry(path)?;
            if modified.contains(path)
                && base
                    .as_ref()
                    .map(|b| b.base_hash.as_deref() != Some(theirs_hash.as_str()))
                    .unwrap_or(false)
            {
                // 双方都改：把本地版本另存为 `<path>.mine`，服务端版本落到工作文件
                let abs = self.abs_of(path);
                let mine = PathBuf::from(format!("{}{}", abs.to_string_lossy(), MINE_SUFFIX));
                tokio::task::spawn_blocking(move || rename_with_retry(&abs, &mine))
                    .await
                    .map_err(|e| ClientError::new("IO", e.to_string()))??;
                self.install_file(path, &theirs_hash).await?;
                self.write_entry_from_disk(path, &theirs_hash, ch.rev)?;
                // 先算哈希（I/O 不能放在事务回调里）
                let mine_hash = self.hash_of_mine(path, true).await;
                let base_hash = base.as_ref().and_then(|b| b.base_hash.clone());
                let cur = self.wc.get_entry(path)?.ok_or_else(|| {
                    ClientError::wc("WC_CORRUPT", format!("缺少条目：{path}"))
                })?;
                self.wc.tx(|w| {
                    let mut row = cur.clone();
                    row.status = EntryStatus::Conflicted;
                    w.upsert_entry(&row)?;
                    // 登记冲突：entry.base_hash 已经被覆盖成服务端版本，共同祖先的哈希只能留在这里
                    w.put_conflict(&ConflictRow {
                        path: path.clone(),
                        kind: EntryKind::File,
                        base_hash,
                        theirs_hash: Some(theirs_hash.clone()),
                        mine_hash,
                        theirs_rev: ch.rev,
                        reason: ConflictReason::BothModified,
                        created_at: now_ms(),
                    })
                })?;
                outcome.conflicts.push(path.clone());
                continue;
            }

            if let Err(_e) = self.install_file(path, &theirs_hash).await {
                // 本机文件系统限制（长路径 / 保留名 / 文件被占用）→ skip + 报告，不阻断整个 update
                outcome.skipped.push(path.clone());
                continue;
            }
            self.write_entry_from_disk(path, &theirs_hash, ch.rev)?;
            {
                let p = path.clone();
                self.wc.tx(|w| {
                    let mut a = parent_of(&p).map(|x| x.to_string());
                    while let Some(cur) = a {
                        if w.has_entry(&cur)? {
                            break;
                        }
                        w.upsert_entry(&EntryRow {
                            path: cur.clone(),
                            kind: EntryKind::Dir,
                            base_rev: ch.rev,
                            base_hash: None,
                            size: 0,
                            mtime_ms: 0,
                            mode: 0o755,
                            status: EntryStatus::Normal,
                            case_conflict: false,
                        })?;
                        a = parent_of(&cur).map(|x| x.to_string());
                    }
                    Ok(())
                })?;
            }
            outcome.updated.push(path.clone());
        }

        self.wc.tx(|w| w.set_revision(head))?;
        Ok(outcome)
    }

    // ---------- 锁（§7.2） ----------

    pub async fn list_locks(&mut self, path: Option<&str>) -> ClientResult<Vec<LockInfo>> {
        self.api.list_locks(&self.repo, path, None).await
    }

    /// 加锁（§5.2 v0.4.17：只剩文件锁，不再有 `kind`）。
    ///
    /// `wc.db` 的 `locks.kind` 列保留、恒写 `file`（列不能再删，否则存量库要重建表）。
    pub async fn lock(&mut self, path: &str, comment: Option<&str>) -> ClientResult<LockInfo> {
        let p = normalize_nfc(path);
        let info = self.api.acquire_lock(&self.repo, &p, comment, None).await?;
        self.wc.put_lock(&crate::engine::db::LockRow {
            path: p,
            kind: EntryKind::File,
            token: info.token.clone().unwrap_or_default(),
            owner: info.owner.clone().into(),
            comment: comment.map(|c| c.to_string()),
            expires_at: info.expires_at.clone(),
        })?;
        Ok(info)
    }

    /// 批量加锁（§6.5 "勾选即提交"的自动补锁）。**串行**，第一个失败即抛。
    pub async fn lock_many(&mut self, paths: &[String]) -> ClientResult<Vec<String>> {
        let mut done = Vec::new();
        for p in paths {
            self.lock(p, None).await?;
            done.push(p.clone());
        }
        Ok(done)
    }

    pub async fn unlock(
        &mut self,
        path: &str,
        break_lock: bool,
        reason: Option<&str>,
    ) -> ClientResult<()> {
        let p = normalize_nfc(path);
        let held = self.wc.get_lock(&p)?;
        self.api
            .release_lock(
                &self.repo,
                &p,
                held.as_ref().and_then(|h| {
                    if h.token.is_empty() {
                        None
                    } else {
                        Some(h.token.as_str())
                    }
                }),
                break_lock,
                reason,
            )
            .await?;
        self.wc.remove_lock(&p)?;
        Ok(())
    }

    pub fn my_locks(&self) -> ClientResult<Vec<crate::engine::db::LockRow>> {
        self.wc.all_locks()
    }

    // ---------- 冲突（§6.2 / §6.5 冲突视图） ----------

    /// 当前未解决的冲突，带"界面上能做什么"的判定。
    pub fn conflicts(&self) -> ClientResult<Vec<ConflictInfo>> {
        let rows = self.wc.all_conflicts()?;
        let mut out = Vec::new();
        for row in rows {
            let sides = self.read_sides(&row);
            out.push(ConflictInfo {
                path: row.path.clone(),
                kind: row.kind,
                reason: row.reason,
                has_mine: sides.mine.kind != SideKind::Missing,
                has_theirs: sides.theirs.kind != SideKind::Missing,
                mergeable: sides.base.kind == SideKind::Text
                    && sides.mine.kind == SideKind::Text
                    && sides.theirs.kind == SideKind::Text,
                theirs_rev: row.theirs_rev,
                sides: ConflictSidesSummary {
                    base: sides.base.kind,
                    mine: sides.mine.kind,
                    theirs: sides.theirs.kind,
                },
            });
        }
        Ok(out)
    }

    /// 读三方内容（本地 / 服务端 / 基线）用于对比。
    pub fn conflict_sides(&self, path: &str) -> ClientResult<ConflictSides> {
        let rel = normalize_nfc(path);
        let row = self
            .wc
            .get_conflict(&rel)?
            .ok_or_else(|| ClientError::wc("NOT_FOUND", format!("该路径没有未解决的冲突：{path}")))?;
        let sides = self.read_sides(&row);
        let mergeable = sides.base.kind == SideKind::Text
            && sides.mine.kind == SideKind::Text
            && sides.theirs.kind == SideKind::Text;
        Ok(ConflictSides {
            path: row.path,
            mergeable,
            base: sides.base,
            mine: sides.mine,
            theirs: sides.theirs,
        })
    }

    /// 解决冲突。
    ///
    /// - `theirs`：接受服务端版本（工作文件已经是它，删掉 `.mine` 即可 → 回到 `normal`）
    /// - `mine`：取本地版本（`both-modified` 用 `.mine` 覆盖工作文件；
    ///   `deleted-remotely` 则是"服务端删了但我保留"，必须退掉基线条目并重新 add）
    /// - `merged`：写入调用方给的合并结果（必须是文本）
    pub async fn resolve_conflict(
        &mut self,
        path: &str,
        choice: ConflictResolution,
        content: Option<&str>,
    ) -> ClientResult<()> {
        let rel = normalize_nfc(path);
        let row = self
            .wc
            .get_conflict(&rel)?
            .ok_or_else(|| ClientError::wc("NOT_FOUND", format!("该路径没有未解决的冲突：{path}")))?;
        let abs = self.abs_of(&rel);
        let abs_mine = PathBuf::from(format!("{}{}", abs.to_string_lossy(), MINE_SUFFIX));

        match choice {
            ConflictResolution::Theirs => {
                if row.reason == ConflictReason::DeletedRemotely {
                    // 接受删除：文件还留在磁盘上，删掉并退登记
                    let a = abs.clone();
                    tokio::task::spawn_blocking(move || remove_path(&a))
                        .await
                        .map_err(|e| ClientError::new("IO", e.to_string()))??;
                    self.wc.tx(|w| {
                        w.delete_entry(&rel)?;
                        w.unstage(&rel)
                    })?;
                } else if let Some(hash) = row.theirs_hash.clone() {
                    // 工作文件已经是服务端版本，只需落基线与状态
                    self.write_entry_from_disk(&rel, &hash, row.theirs_rev)?;
                }
                remove_file_quiet(&abs_mine)?;
            }
            ConflictResolution::Mine => {
                if row.reason == ConflictReason::BothModified {
                    if !abs_mine.exists() {
                        return Err(ClientError::wc(
                            "NOT_FOUND",
                            format!("本地版本已不在：{rel}{MINE_SUFFIX}"),
                        ));
                    }
                    let (from, to) = (abs_mine.clone(), abs.clone());
                    tokio::task::spawn_blocking(move || rename_with_retry(&from, &to))
                        .await
                        .map_err(|e| ClientError::new("IO", e.to_string()))??;
                    let meta = std::fs::metadata(&abs)?;
                    // base 仍是服务端版本 → 内容与基线不同，状态是 modified，可直接提交
                    self.write_local_change_entry(
                        &rel,
                        row.kind,
                        meta.len() as i64,
                        mode_bits(&meta),
                        row.theirs_rev,
                        row.theirs_hash.clone(),
                    )?;
                } else {
                    // 服务端已删除、本地保留：退掉基线条目并重新纳入（提交时是 add）
                    remove_file_quiet(&abs_mine)?;
                    self.wc.tx(|w| {
                        w.delete_entry(&rel)?;
                        w.stage(&rel, PendingOp::Add)
                    })?;
                }
            }
            ConflictResolution::Merged => {
                let Some(body) = content else {
                    return Err(ClientError::wc("IO", "合并结果不能为空"));
                };
                if row.reason != ConflictReason::BothModified {
                    return Err(ClientError::wc("CONFLICT", "服务端已删除该路径，不能做内容合并"));
                }
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&abs, body)?;
                remove_file_quiet(&abs_mine)?;
                let meta = std::fs::metadata(&abs)?;
                self.write_local_change_entry(
                    &rel,
                    row.kind,
                    meta.len() as i64,
                    mode_bits(&meta),
                    row.theirs_rev,
                    row.theirs_hash.clone(),
                )?;
            }
        }

        self.wc.tx(|w| w.remove_conflict(&rel))?;
        Ok(())
    }

    /// 读一行的三方内容（供 `conflicts()` 与 `conflict_sides()` 共用）。
    fn read_sides(&self, row: &ConflictRow) -> ConflictSidesInner {
        let abs = self.abs_of(&row.path);
        let abs_mine = PathBuf::from(format!("{}{}", abs.to_string_lossy(), MINE_SUFFIX));

        // base：共同祖先，从 pristine 里取（内容寻址，一直在缓存里）
        let mut base = SideContent {
            kind: SideKind::Missing,
            size: 0,
            text: None,
        };
        if let Some(hash) = &row.base_hash {
            let _ = ensure_pristine(self.root(), hash, &self.cache_dir);
            base = to_side(read_text_capped(&pristine_path_for(self.root(), hash), None, None));
        }

        // mine：both-modified 在 `<path>.mine`；deleted-remotely 时工作文件本身就是本地版本
        let mine_path = if row.reason == ConflictReason::BothModified {
            abs_mine
        } else {
            abs.clone()
        };
        let mine = to_side(read_text_capped(&mine_path, None, None));

        // theirs：both-modified 时工作文件已被换成服务端版本；deleted-remotely 没有服务端内容
        let theirs = if row.reason == ConflictReason::BothModified {
            to_side(read_text_capped(&abs, None, None))
        } else {
            SideContent {
                kind: SideKind::Missing,
                size: 0,
                text: None,
            }
        };

        ConflictSidesInner {
            base,
            mine,
            theirs,
        }
    }

    /// 本地版本的内容哈希：优先 `<path>.mine`，退回工作文件；都不存在则为 None。
    async fn hash_of_mine(&mut self, rel: &str, prefer_mine_file: bool) -> Option<String> {
        let abs = self.abs_of(rel);
        let candidates: Vec<PathBuf> = if prefer_mine_file {
            vec![
                PathBuf::from(format!("{}{}", abs.to_string_lossy(), MINE_SUFFIX)),
                abs,
            ]
        } else {
            vec![abs]
        };
        for p in candidates {
            if p.exists() {
                return match tokio::task::spawn_blocking(move || hash_file(&p)).await {
                    Ok(Ok(h)) => Some(h),
                    _ => None,
                };
            }
        }
        None
    }
}

struct ConflictSidesInner {
    base: SideContent,
    mine: SideContent,
    theirs: SideContent,
}

fn to_side(r: TextReadResult) -> SideContent {
    match r {
        TextReadResult::Text { text, size } => SideContent {
            kind: SideKind::Text,
            size: size as i64,
            text: Some(text),
        },
        TextReadResult::Binary { size } => SideContent {
            kind: SideKind::Binary,
            size: size as i64,
            text: None,
        },
        TextReadResult::TooLarge { size } => SideContent {
            kind: SideKind::TooLarge,
            size: size as i64,
            text: None,
        },
        TextReadResult::Missing { size } => SideContent {
            kind: SideKind::Missing,
            size: size as i64,
            text: None,
        },
    }
}

/// 检出时用的安装器（把网络 + 缓存 + 安装三步封装起来，便于有界并发）。
struct Installer {
    api: Arc<ApiClient>,
    repo: String,
    root: PathBuf,
    cache_dir: PathBuf,
}

impl Installer {
    async fn install_file(&self, rel: &str, hash: &str) -> ClientResult<()> {
        if !cache_path_for(hash, &self.cache_dir).exists() {
            let tmp = self
                .cache_dir
                .join("tmp")
                .join(format!("{hash}.dl-{}", std::process::id()));
            if let Some(parent) = tmp.parent() {
                std::fs::create_dir_all(parent)?;
            }
            self.api.download_blob_to(&self.repo, hash, &tmp).await?;
            store_to_cache(&tmp, hash, &self.cache_dir)?;
        }
        ensure_pristine(&self.root, hash, &self.cache_dir)?;
        install_working_file(&self.root, hash, &join_rel(&self.root, rel))?;
        Ok(())
    }
}

fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for seg in rel.split('/') {
        if !seg.is_empty() {
            p.push(seg);
        }
    }
    p
}

#[cfg(unix)]
fn mode_bits(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::PermissionsExt;
    (meta.permissions().mode() & 0o111) as i64
}

#[cfg(not(unix))]
fn mode_bits(_meta: &std::fs::Metadata) -> i64 {
    0
}

/// 删除文件或目录（不存在视为成功）。
fn remove_path(abs: &Path) -> std::io::Result<()> {
    let r = if abs.is_dir() {
        std::fs::remove_dir_all(abs)
    } else {
        std::fs::remove_file(abs)
    };
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn remove_file_quiet(p: &Path) -> ClientResult<()> {
    match std::fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ClientError::wc("IO", e.to_string())),
    }
}

/// Windows 上 Photoshop / Office 会持有文件句柄，导致 rename 失败（§6.6 ④）。
/// 三次退避重试（1s / 2s / 4s），仍失败则抛出，绝不留下半截状态。
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    with_retry(
        RetryOptions {
            retries: 3,
            base_delay_ms: 1000,
        },
        || std::fs::rename(from, to),
    )
}

/// 上传：小文件整块，大文件分块（§7.2）。
async fn upload_blob(
    api: &ApiClient,
    repo: &str,
    abs_path: &Path,
    hash: &str,
    size: i64,
) -> ClientResult<()> {
    if !needs_chunked_upload(size as u64) {
        let p = abs_path.to_path_buf();
        let buf = tokio::task::spawn_blocking(move || std::fs::read(&p))
            .await
            .map_err(|e| ClientError::new("IO", e.to_string()))??;
        api.put_blob(repo, hash, buf).await?;
        return Ok(());
    }
    let chunk_size = DEFAULT_CHUNK_SIZE as i64;
    let total = (size + chunk_size - 1) / chunk_size;
    let session = api.create_upload(repo, hash, size, chunk_size).await?;
    let received: HashSet<i64> = session.received_chunks.iter().copied().collect();
    for n in 0..total {
        if received.contains(&n) {
            continue;
        }
        let p = abs_path.to_path_buf();
        let offset = (n * chunk_size) as u64;
        let buf = tokio::task::spawn_blocking(move || read_range(&p, offset, chunk_size as usize))
            .await
            .map_err(|e| ClientError::new("IO", e.to_string()))??;
        api.put_chunk(repo, &session.upload_id, n, buf).await?;
    }
    api.complete_upload(repo, &session.upload_id).await?;
    Ok(())
}

fn read_range(abs_path: &Path, offset: u64, length: usize) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(abs_path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; length];
    let mut read = 0usize;
    while read < length {
        let n = f.read(&mut buf[read..])?;
        if n == 0 {
            break;
        }
        read += n;
    }
    buf.truncate(read);
    Ok(buf)
}

/// 扫描警告（供 CLI 展示）。
pub fn scan_warnings(root: &Path) -> Vec<String> {
    scan_disk(root, None).warnings
}

/// 供 `with_retry_async` 用（避免未使用告警；实际在更新/下载路径里按需调用）。
#[allow(dead_code)]
async fn retry_io<T, F, Fut>(f: F) -> std::io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    with_retry_async(RetryOptions::default(), f).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::api::{HttpRequest, HttpResponse, Transport};
    use crate::engine::api::BoxFuture;
    use std::sync::Mutex;

    /// 极简假服务端：只实现本模块用到的端点。
    struct FakeServer {
        pub head_rev: std::sync::atomic::AtomicI64,
        pub blobs: Mutex<HashMap<String, Vec<u8>>>,
        pub locks: Mutex<HashMap<String, (String, String)>>, // path -> (owner, token)
        pub commits: Mutex<Vec<serde_json::Value>>,
        pub prepares: Mutex<Vec<serde_json::Value>>,
        pub require_lock: std::sync::atomic::AtomicBool,
        pub prepared: Mutex<Option<String>>,
    }

    impl FakeServer {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                head_rev: std::sync::atomic::AtomicI64::new(1),
                blobs: Mutex::new(HashMap::new()),
                locks: Mutex::new(HashMap::new()),
                commits: Mutex::new(Vec::new()),
                prepares: Mutex::new(Vec::new()),
                require_lock: std::sync::atomic::AtomicBool::new(false),
                prepared: Mutex::new(None),
            })
        }
    }

    fn ok_json(v: serde_json::Value) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: vec![],
            body: serde_json::to_vec(&v).unwrap(),
        }
    }

    fn err_json(status: u16, code: &str, details: serde_json::Value) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![],
            body: serde_json::to_vec(&serde_json::json!({
                "error": { "code": code, "message": code, "details": details }
            }))
            .unwrap(),
        }
    }

    impl Transport for FakeServer {
        fn send<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, ClientResult<HttpResponse>> {
            Box::pin(async move {
                let path = req.url.split("/api/v1").nth(1).unwrap_or("").to_string();
                let path = path.split('?').next().unwrap_or("").to_string();

                if path.ends_with("/info") {
                    return Ok(ok_json(serde_json::json!({
                        "head_rev": self.head_rev.load(std::sync::atomic::Ordering::SeqCst)
                    })));
                }
                if path.ends_with("/tree") {
                    return Ok(ok_json(serde_json::json!({
                        "items": [
                            { "path": "a", "kind": "dir" },
                            { "path": "a/hello.txt", "kind": "file", "blob_hash": "h1", "size": 2 }
                        ]
                    })));
                }
                if path.contains("/blobs/") && req.method == "GET" {
                    let hash = path.rsplit('/').next().unwrap().to_string();
                    let body = self
                        .blobs
                        .lock()
                        .unwrap()
                        .get(&hash)
                        .cloned()
                        .unwrap_or_default();
                    return Ok(HttpResponse {
                        status: 200,
                        headers: vec![],
                        body,
                    });
                }
                if path.contains("/blobs/missing") {
                    return Ok(ok_json(serde_json::json!({ "missing": [] })));
                }
                if path.contains("/commit/prepare") {
                    if let Ok(body) = serde_json::from_slice::<serde_json::Value>(
                        req.body.as_deref().unwrap_or(b"{}"),
                    ) {
                        self.prepares.lock().unwrap().push(body);
                    }
                    if self.require_lock.load(std::sync::atomic::Ordering::SeqCst) {
                        let body: serde_json::Value =
                            serde_json::from_slice(req.body.as_deref().unwrap_or(b"{}")).unwrap();
                        let paths: Vec<String> = body["changes"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|c| c["path"].as_str().unwrap().to_string())
                            .collect();
                        let locks = self.locks.lock().unwrap();
                        let missing: Vec<String> =
                            paths.into_iter().filter(|p| !locks.contains_key(p)).collect();
                        if !missing.is_empty() {
                            return Ok(err_json(
                                412,
                                "NEEDS_LOCK",
                                serde_json::json!({ "paths": missing }),
                            ));
                        }
                    }
                    *self.prepared.lock().unwrap() = Some("tok".into());
                    return Ok(ok_json(serde_json::json!({
                        "commit_id": "c", "commit_token": "tok", "need_blobs": []
                    })));
                }
                if path.ends_with("/commit") && req.method == "POST" {
                    let body: serde_json::Value =
                        serde_json::from_slice(req.body.as_deref().unwrap_or(b"{}")).unwrap();
                    self.commits.lock().unwrap().push(body);
                    let rev = self.head_rev.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    return Ok(ok_json(serde_json::json!({
                        "commit_id": "c", "rev": rev, "replayed": false
                    })));
                }
                if path.ends_with("/locks") && req.method == "POST" {
                    let body: serde_json::Value =
                        serde_json::from_slice(req.body.as_deref().unwrap_or(b"{}")).unwrap();
                    let p = body["path"].as_str().unwrap_or("").to_string();
                    self.locks
                        .lock()
                        .unwrap()
                        .insert(p.clone(), ("me".into(), "lk".into()));
                    return Ok(ok_json(serde_json::json!({
                        "path": p, "kind": "file", "owner": "me", "token": "lk"
                    })));
                }
                if path.contains("/locks/") && req.method == "DELETE" {
                    let p = path.split("/locks/").nth(1).unwrap_or("").to_string();
                    self.locks.lock().unwrap().remove(&p);
                    return Ok(ok_json(serde_json::json!({})));
                }
                if path.ends_with("/locks") && req.method == "GET" {
                    let items: Vec<serde_json::Value> = self
                        .locks
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(p, (o, _))| serde_json::json!({ "path": p, "owner": o, "kind": "file" }))
                        .collect();
                    return Ok(ok_json(serde_json::json!({ "items": items })));
                }
                if path.ends_with("/changes") {
                    return Ok(HttpResponse {
                        status: 200,
                        headers: vec![],
                        body: b"".to_vec(),
                    });
                }
                Ok(err_json(404, "NOT_FOUND", serde_json::json!({})))
            })
        }

        fn download<'a>(
            &'a self,
            req: HttpRequest,
            dest: &'a Path,
        ) -> BoxFuture<'a, ClientResult<u64>> {
            Box::pin(async move {
                let hash = req.url.rsplit('/').next().unwrap_or("").to_string();
                let body = self
                    .blobs
                    .lock()
                    .unwrap()
                    .get(&hash)
                    .cloned()
                    .unwrap_or_default();
                std::fs::write(dest, &body).unwrap();
                Ok(body.len() as u64)
            })
        }
    }

    fn api(server: Arc<FakeServer>) -> Arc<ApiClient> {
        Arc::new(ApiClient::new("http://fake", Some("t".into()), server))
    }

    async fn checkout(dir: &Path, server: Arc<FakeServer>) -> WorkingCopy {
        server.blobs.lock().unwrap().insert("h1".into(), b"hi".to_vec());
        WorkingCopy::checkout(
            CheckoutOptions {
                root: dir.to_path_buf(),
                api: api(server),
                repo: "r".into(),
                user: Some("u".into()),
                cache_dir: Some(dir.join("cache")),
                concurrency: Some(2),
                sparse: vec![],
            },
            None,
        )
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn checkout_installs_files_and_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let server = FakeServer::new();
        let wc = checkout(&wc_dir, server).await;
        assert_eq!(
            std::fs::read(wc_dir.join("a/hello.txt")).unwrap(),
            b"hi"
        );
        assert_eq!(wc.revision(), 1);
        assert!(wc.wc.has_entry("a").unwrap());
        assert!(wc.wc.has_entry("a/hello.txt").unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn checkout_refuses_existing_working_copy() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let server = FakeServer::new();
        let _wc = checkout(&wc_dir, server.clone()).await;
        let e = WorkingCopy::checkout(
            CheckoutOptions {
                root: wc_dir,
                api: api(server),
                repo: "r".into(),
                user: None,
                cache_dir: None,
                concurrency: None,
                sparse: vec![],
            },
            None,
        )
        .await
        .err().expect("应当失败");
        assert_eq!(e.code, "NOT_A_WORKING_COPY");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_reports_modified_after_local_edit() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let server = FakeServer::new();
        let mut wc = checkout(&wc_dir, server).await;
        std::fs::write(wc_dir.join("a/hello.txt"), b"changed").unwrap();
        let items = wc.status(false).unwrap();
        let item = items.iter().find(|i| i.path == "a/hello.txt").unwrap();
        assert_eq!(item.status, crate::engine::scan::StatusCode::Modified);
        let _ = &mut wc;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_stages_file_and_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let server = FakeServer::new();
        let mut wc = checkout(&wc_dir, server).await;
        std::fs::write(wc_dir.join("a/new.txt"), b"n").unwrap();
        let n = wc.add(&["a/new.txt".to_string()]).unwrap();
        assert_eq!(n, 1);
        assert_eq!(wc.wc.pending_op("a/new.txt").unwrap(), Some(PendingOp::Add));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_rejects_invalid_path() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let mut wc = checkout(&wc_dir, FakeServer::new()).await;
        let e = wc.add(&["a/b?.psd".to_string()]).err().expect("应当失败");
        assert_eq!(e.code, "PATH_INVALID");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commit_uploads_and_bumps_revision() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let server = FakeServer::new();
        let mut wc = checkout(&wc_dir, server.clone()).await;
        std::fs::write(wc_dir.join("a/new.txt"), b"n").unwrap();
        wc.add(&["a/new.txt".to_string()]).unwrap();
        let out = wc.commit(CommitOptions::default(), None).await.unwrap();
        assert_eq!(out.rev, 2);
        assert_eq!(out.committed, vec!["a/new.txt".to_string()]);
        assert_eq!(wc.revision(), 2);
        assert_eq!(
            wc.wc.get_entry("a/new.txt").unwrap().unwrap().status,
            EntryStatus::Normal
        );
        let body = server.prepares.lock().unwrap()[0].clone();
        assert_eq!(body["changes"][0]["op"], serde_json::json!("add"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commit_without_changes_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let mut wc = checkout(&wc_dir, FakeServer::new()).await;
        let e = wc.commit(CommitOptions::default(), None).await.err().expect("应当失败");
        assert_eq!(e.code, "NO_CHANGES");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commit_auto_locks_when_server_requires() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let server = FakeServer::new();
        let mut wc = checkout(&wc_dir, server.clone()).await;
        server
            .require_lock
            .store(true, std::sync::atomic::Ordering::SeqCst);
        std::fs::write(wc_dir.join("a/new.txt"), b"n").unwrap();
        wc.add(&["a/new.txt".to_string()]).unwrap();
        let out = wc.commit(CommitOptions::default(), None).await.unwrap();
        assert_eq!(out.rev, 2);
        assert!(
            server.locks.lock().unwrap().is_empty(),
            "提交成功后应释放本次自动补的锁"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_stages_delete_only_for_tracked_paths() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let mut wc = checkout(&wc_dir, FakeServer::new()).await;

        // 已纳管 → 记 delete
        wc.remove(&["a/hello.txt".to_string()]).await.unwrap();
        assert_eq!(wc.wc.pending_op("a/hello.txt").unwrap(), Some(PendingOp::Delete));
        assert!(!wc_dir.join("a/hello.txt").exists());

        // 未纳管 → 不记任何东西
        std::fs::write(wc_dir.join("a/untracked.txt"), b"u").unwrap();
        wc.remove(&["a/untracked.txt".to_string()]).await.unwrap();
        assert_eq!(wc.wc.pending_op("a/untracked.txt").unwrap(), None);
        assert!(!wc_dir.join("a/untracked.txt").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revert_restores_baseline_content() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let mut wc = checkout(&wc_dir, FakeServer::new()).await;
        std::fs::write(wc_dir.join("a/hello.txt"), b"oops").unwrap();
        wc.revert(&["a/hello.txt".to_string()]).await.unwrap();
        assert_eq!(std::fs::read(wc_dir.join("a/hello.txt")).unwrap(), b"hi");
        let items = wc.status(false).unwrap();
        let it = items.iter().find(|i| i.path == "a/hello.txt").unwrap();
        assert_eq!(it.status, crate::engine::scan::StatusCode::Normal);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revert_unknown_path_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let mut wc = checkout(&wc_dir, FakeServer::new()).await;
        let e = wc.revert(&["nope.txt".to_string()]).await.err().expect("应当失败");
        assert_eq!(e.code, "NOT_FOUND");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ignore_file_roundtrip_and_empty_means_delete() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let mut wc = checkout(&wc_dir, FakeServer::new()).await;
        assert_eq!(wc.read_ignore_file(), "");
        wc.write_ignore_file("*.tmp\n").await.unwrap();
        assert_eq!(wc.read_ignore_file(), "*.tmp\n");
        wc.write_ignore_file("   \n").await.unwrap();
        assert!(!wc_dir.join(".b-artifactignore").exists(), "清空即删文件");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lock_and_unlock_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let server = FakeServer::new();
        let mut wc = checkout(&wc_dir, server.clone()).await;
        let info = wc.lock("a/hello.txt", Some("mine")).await.unwrap();
        assert_eq!(info.owner, "me");
        assert!(wc.wc.get_lock("a/hello.txt").unwrap().is_some());
        wc.unlock("a/hello.txt", false, None).await.unwrap();
        assert!(wc.wc.get_lock("a/hello.txt").unwrap().is_none());
        assert!(server.locks.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn update_is_noop_when_head_is_not_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let mut wc = checkout(&wc_dir, FakeServer::new()).await;
        let out = wc.update(None).await.unwrap();
        assert_eq!(out.rev, 1);
        assert!(out.updated.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn close_releases_process_lock() {
        let dir = tempfile::tempdir().unwrap();
        let wc_dir = dir.path().join("wc");
        let mut wc = checkout(&wc_dir, FakeServer::new()).await;
        let lock = wc_dir.join(WC_DIR).join(crate::engine::db::WC_LOCK);
        assert!(lock.exists());
        wc.close();
        assert!(!lock.exists());
    }
}
