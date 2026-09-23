//! 管理端静态资源托管（§8.1）。
//!
//! 设计要点：
//! * 磁盘目录托管（`server.admin_dir`，默认 `admin/dist`），开发期直接指向 Vite 构建产物；
//!   生产部署把它指向与二进制同级的目录即可（M5 再补 `rust-embed` 内嵌做单文件部署）。
//! * **SPA 回退**：未命中的无扩展名路径一律回落到 `index.html`（Vue Router history 模式）。
//! * 安全：逐段拒绝 `.` / `..` / 空段与反斜杠，杜绝路径穿越。
//! * 缓存：`assets/` 下的文件名由 Vite 带内容哈希 → `immutable` 长缓存；
//!   `index.html` 必须 `no-cache`，否则发版后用户拿到旧入口。

use crate::state::SharedState;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use std::path::{Component, PathBuf};

/// 允许托管的扩展名 → MIME（不在表内的一律按 `application/octet-stream` 下载）。
fn mime_of(ext: &str) -> &'static str {
    match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// 把请求路径（已去掉 `/admin` 前缀）解析为目录内的安全相对路径。
/// 返回 `None` 表示疑似路径穿越。
fn resolve(root: &std::path::Path, rel: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." || seg.contains('\\') || seg.contains('\0') {
            return None;
        }
        // 再兜一层：Component 归一化后必须仍是普通名字段
        match PathBuf::from(seg).components().next() {
            Some(Component::Normal(_)) => out.push(seg),
            _ => return None,
        }
    }
    Some(out)
}

/// 是否视为"静态资源"（有扩展名）；否则按前端路由处理。
fn looks_like_asset(rel: &str) -> bool {
    rel.rsplit('/')
        .next()
        .map(|f| f.rsplit_once('.').map(|(_, e)| !e.is_empty()).unwrap_or(false))
        .unwrap_or(false)
}

async fn serve(State(state): State<SharedState>, rel: String) -> Response {
    let root = PathBuf::from(&state.config.server.admin_dir);
    let Some(target) = resolve(&root, &rel) else {
        return (StatusCode::BAD_REQUEST, "非法路径").into_response();
    };

    let mut path = target.clone();
    if path.is_dir() {
        path.push("index.html");
    }

    // Vite 产物：命中就发；未命中且不像资源 → SPA 回退
    let mut is_spa_fallback = false;
    if tokio::fs::metadata(&path).await.is_err() {
        if looks_like_asset(&rel) {
            return (StatusCode::NOT_FOUND, "Not Found").into_response();
        }
        path = root.join("index.html");
        is_spa_fallback = true;
    }

    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let cache = if is_spa_fallback || path.file_name().is_some_and(|n| n == "index.html") {
                "no-cache"
            } else if rel.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "public, max-age=3600"
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, mime_of(&ext))
                .header(CONTENT_LENGTH, bytes.len())
                // 回退响应不能长缓存，否则真实资源上线后仍走缓存
                .header(CACHE_CONTROL, if is_spa_fallback { "no-cache" } else { cache })
                .body(Body::from(bytes))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => admin_missing_hint(&root).into_response(),
    }
}

/// 构建产物缺失时的友好提示（避免运维把 404 当成服务端挂了）。
fn admin_missing_hint(root: &std::path::Path) -> Html<String> {
    Html(format!(
        r#"<!doctype html><meta charset="utf-8">
<title>b-artifact 管理端</title>
<body style="font:14px/1.7 -apple-system,Segoe UI,sans-serif;padding:48px;max-width:720px;margin:auto">
<h2>管理端尚未构建</h2>
<p>服务端在 <code>{}</code> 下找不到 <code>index.html</code>。</p>
<pre style="background:#f5f5f5;padding:12px;border-radius:6px">cd admin &amp;&amp; npm install &amp;&amp; npm run build</pre>
<p>之后重新访问 <a href="/admin/">/admin/</a>；也可用 <code>--admin-dir</code> 或配置文件
<code>[server] admin_dir</code> 指向其它目录。</p>
</body>"#,
        root.display()
    ))
}

/// `GET /admin`
pub async fn admin_root(state: State<SharedState>) -> Response {
    serve(state, String::new()).await
}

/// `GET /admin/{*path}`
pub async fn admin_path(
    state: State<SharedState>,
    Path(p): Path<String>,
) -> Response {
    serve(state, p).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rejects_traversal() {
        let root = PathBuf::from("/srv/admin");
        assert_eq!(resolve(&root, "a/b.js").unwrap(), PathBuf::from("/srv/admin/a/b.js"));
        assert!(resolve(&root, "../etc/passwd").is_none());
        assert!(resolve(&root, "a/../../b").is_none());
        assert!(resolve(&root, "..\\win").is_none());
        // 空段被忽略，不构成穿越
        assert_eq!(resolve(&root, "a//b").unwrap(), PathBuf::from("/srv/admin/a/b"));
    }

    #[test]
    fn asset_detection() {
        assert!(looks_like_asset("assets/index-abc123.js"));
        assert!(looks_like_asset("favicon.ico"));
        assert!(!looks_like_asset("repos/demo/acl"));
        assert!(!looks_like_asset(""));
    }
}
