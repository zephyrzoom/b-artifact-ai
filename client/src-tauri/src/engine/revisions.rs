/**
 * 历史修订的只读访问（§6.5 历史视图）。
 *
 * 与 `wc.rs` 的分工：这里**不碰 wc.db**——看历史、下载旧版本是"仓库级"操作，
 * 只要登录态 + 仓库名即可，不需要先检出。
 *
 * ⚠️ 已知性能边界：非 HEAD 修订的列目录（`tree?rev=N`）在服务端是 `list_dir_at_rev` 路径，
 * M1.6 实测在 24 万文件档位是 7.4s（唯一未达标核心项，见 §15.2）。所以这里的用法要克制：
 * 只在用户**选中某个修订**时按需拉一层目录，不做预取、不递归展开。
 */
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::engine::api::ApiClient;
use crate::engine::errors::{ClientError, ClientResult};
use crate::engine::paths::{base_name, parent_of};

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct RevisionFile {
    pub path: String,
    pub blob_hash: String,
    pub size: i64,
    /// 该文件内容最后一次变化的修订。
    pub changed_rev: i64,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct DownloadOutcome {
    /// 实际落盘路径（重名时会自动带修订号后缀）。
    pub saved: String,
    pub size: i64,
    pub rev: i64,
    pub path: String,
}

/// `rev <= 0` 统一表示 HEAD（与服务端 `tree?rev=0` 的约定一致）。
pub fn normalize_rev(rev: Option<i64>) -> i64 {
    match rev {
        Some(r) if r > 0 => r,
        _ => 0,
    }
}

/// 在某个修订下定位一个文件。
///
/// 只拉**父目录一层**（depth=1）而不是整棵树：既省流量，也避开大仓库上
/// `list_dir_at_rev` 的慢路径。
pub async fn resolve_file_at_revision(
    client: &ApiClient,
    repo: &str,
    path: &str,
    rev: i64,
) -> ClientResult<RevisionFile> {
    let parent = parent_of(path).unwrap_or("");
    let tree = client
        .tree(repo, Some(normalize_rev(Some(rev))), Some(parent), Some(1))
        .await?;
    let hit = tree
        .items
        .iter()
        .find(|it| it.path == path && it.kind == "file");
    match hit {
        Some(it) if it.blob_hash.is_some() => Ok(RevisionFile {
            path: it.path.clone(),
            blob_hash: it.blob_hash.clone().unwrap_or_default(),
            size: it.size,
            changed_rev: it.changed_rev,
        }),
        _ => {
            let where_ = if rev > 0 {
                format!("r{rev}")
            } else {
                "HEAD".to_string()
            };
            Err(ClientError::wc(
                "NOT_FOUND",
                format!("{where_} 下没有这个文件：{path}"),
            ))
        }
    }
}

/// Node 的 `path.extname` 语义：`.gitignore` 不算扩展名，`a.tar.gz` → `.gz`。
fn extname(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[i..],
        _ => "",
    }
}

fn path_exists(p: &Path) -> bool {
    p.exists()
}

/// 目标文件名：优先原名，重名时退回 `<stem>.r<rev><ext>`，再重名就加序号。
///
/// 不做静默覆盖——用户从历史里"下载此版本"往往正是为了跟当前版本对照，
/// 覆盖掉已经在那儿的那份会很难解释。
pub fn unique_target_name(dir: &Path, path: &str, rev: i64) -> String {
    let base = base_name(path);
    let first = dir.join(base);
    if !path_exists(&first) {
        return first.to_string_lossy().to_string();
    }

    let ext = extname(base);
    let stem = &base[..base.len() - ext.len()];
    let rev = normalize_rev(Some(rev));
    let mut candidate = dir.join(format!("{stem}.r{rev}{ext}"));
    let mut i = 2;
    while path_exists(&candidate) && i < 100 {
        candidate = dir.join(format!("{stem}.r{rev} ({i}){ext}"));
        i += 1;
    }
    candidate.to_string_lossy().to_string()
}

/// 把某个修订下的文件下载到本地目录（流式，不把大文件读进内存）。
pub async fn download_revision_file(
    client: &ApiClient,
    repo: &str,
    path: &str,
    rev: i64,
    target_dir: &str,
) -> ClientResult<DownloadOutcome> {
    let rev = normalize_rev(Some(rev));
    let file = resolve_file_at_revision(client, repo, path, rev).await?;

    std::fs::create_dir_all(target_dir)?;
    let dir = PathBuf::from(target_dir);
    let dest = PathBuf::from(unique_target_name(&dir, path, rev));
    let part = PathBuf::from(format!("{}.part-{}", dest.to_string_lossy(), std::process::id()));

    if let Err(e) = client
        .download_blob_to(repo, &file.blob_hash, &part)
        .await
    {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    // rename 在 Windows 上可能被占用阻碍：走 1s/2s/4s 退避
    if let Err(e) = crate::engine::io_retry::with_retry(
        crate::engine::io_retry::RetryOptions::default(),
        || std::fs::rename(&part, &dest),
    ) {
        let _ = std::fs::remove_file(&part);
        return Err(ClientError::wc("IO", e.to_string()));
    }

    Ok(DownloadOutcome {
        saved: dest.to_string_lossy().to_string(),
        size: file.size,
        rev,
        path: file.path,
    })
}

/// 展示用的短名（面包屑最后一段）。
pub fn short_name(p: &str) -> String {
    if p.is_empty() {
        "/".to_string()
    } else {
        base_name(p).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::api::{BoxFuture, HttpRequest, HttpResponse, Transport};
    use std::sync::Arc;

    struct Stub {
        tree_json: serde_json::Value,
        blob: Vec<u8>,
    }

    impl Transport for Stub {
        fn send<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, ClientResult<HttpResponse>> {
            Box::pin(async move {
                assert!(req.url.contains("/tree"), "只该请求 tree：{}", req.url);
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: serde_json::to_vec(&self.tree_json).unwrap(),
                })
            })
        }
        fn download<'a>(
            &'a self,
            _req: HttpRequest,
            dest: &'a Path,
        ) -> BoxFuture<'a, ClientResult<u64>> {
            Box::pin(async move {
                std::fs::write(dest, &self.blob).unwrap();
                Ok(self.blob.len() as u64)
            })
        }
    }

    fn client(stub: Stub) -> ApiClient {
        ApiClient::new("http://h", None, Arc::new(stub))
    }

    #[test]
    fn normalize_rev_treats_non_positive_as_head() {
        assert_eq!(normalize_rev(Some(0)), 0);
        assert_eq!(normalize_rev(Some(-3)), 0);
        assert_eq!(normalize_rev(None), 0);
        assert_eq!(normalize_rev(Some(7)), 7);
    }

    #[test]
    fn short_name_is_last_segment() {
        assert_eq!(short_name(""), "/");
        assert_eq!(short_name("a/b/c.psd"), "c.psd");
        assert_eq!(short_name("c.psd"), "c.psd");
    }

    #[test]
    fn unique_name_keeps_original_when_free() {
        let dir = tempfile::tempdir().unwrap();
        let p = unique_target_name(dir.path(), "art/hero.psd", 3);
        assert_eq!(PathBuf::from(&p), dir.path().join("hero.psd"));
    }

    #[test]
    fn unique_name_tags_revision_on_conflict() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hero.psd"), b"x").unwrap();
        let p = unique_target_name(dir.path(), "art/hero.psd", 3);
        assert_eq!(PathBuf::from(&p), dir.path().join("hero.r3.psd"));

        std::fs::write(dir.path().join("hero.r3.psd"), b"x").unwrap();
        let p = unique_target_name(dir.path(), "art/hero.psd", 3);
        assert_eq!(PathBuf::from(&p), dir.path().join("hero.r3 (2).psd"));
    }

    #[test]
    fn dotfiles_have_no_extension() {
        assert_eq!(extname(".b-artifactignore"), "");
        assert_eq!(extname("a.tar.gz"), ".gz");
        assert_eq!(extname("a"), "");
    }

    #[tokio::test]
    async fn resolves_file_and_downloads_it() {
        let dir = tempfile::tempdir().unwrap();
        let stub = Stub {
            tree_json: serde_json::json!({
                "items": [
                    { "path": "art", "kind": "dir" },
                    { "path": "art/hero.psd", "kind": "file", "blob_hash": "h1", "size": 2, "changed_rev": 4 }
                ]
            }),
            blob: b"ok".to_vec(),
        };
        let c = client(stub);
        let f = resolve_file_at_revision(&c, "r", "art/hero.psd", 5)
            .await
            .unwrap();
        assert_eq!(f.blob_hash, "h1");
        assert_eq!(f.changed_rev, 4);

        let out = download_revision_file(&c, "r", "art/hero.psd", 5, &dir.path().to_string_lossy())
            .await
            .unwrap();
        assert_eq!(out.rev, 5);
        assert_eq!(std::fs::read(&out.saved).unwrap(), b"ok");
        assert!(
            !PathBuf::from(&out.saved).to_string_lossy().contains(".part-"),
            "临时文件必须被改名"
        );
    }

    #[tokio::test]
    async fn missing_file_is_not_found() {
        let stub = Stub {
            tree_json: serde_json::json!({ "items": [] }),
            blob: vec![],
        };
        let c = client(stub);
        let e = resolve_file_at_revision(&c, "r", "art/nope.psd", 0)
            .await
            .err().expect("应当失败");
        assert_eq!(e.code, "NOT_FOUND");
        assert!(e.message.contains("HEAD"));
    }
}
