//! b-artifact 服务端入口（§3.5 / §11.1）。
//!
//! 参数解析 + 进程生命周期；路由与业务逻辑在库 crate（lib.rs）中。

use b_artifact_server::auth;
use b_artifact_server::audit;
use b_artifact_server::config::Config;
use b_artifact_server::error;
use b_artifact_server::state::{AppState, SharedState};
use b_artifact_server::storage::blob::BlobStore;
use b_artifact_server::storage::db;
use b_artifact_server::storage::pool::DbPool;
use b_artifact_server::api;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

const USAGE: &str = "\
b-artifact-server — 集中式二进制资产版本管理服务端

用法: b-artifact-server [选项]

选项:
  --config <FILE>          TOML 配置文件（见 docs/架构设计方案.md §11.2）
  --data-dir <DIR>         数据目录（默认 ./data，覆盖配置文件）
  --listen <ADDR>          监听地址（默认 127.0.0.1:8080，覆盖配置文件）
  --admin-dir <DIR>        管理端前端构建产物目录（默认 admin/dist）
  --create-admin <NAME>    交互式创建本地管理员后退出（不启动服务）
  -V, --version            打印版本
  -h, --help               打印帮助
";

fn main() {
    let mut config_path: Option<String> = None;
    let mut data_dir: Option<String> = None;
    let mut listen: Option<String> = None;
    let mut admin_dir: Option<String> = None;
    let mut create_admin: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config_path = args.next(),
            "--data-dir" => data_dir = args.next(),
            "--listen" => listen = args.next(),
            "--admin-dir" => admin_dir = args.next(),
            "--create-admin" => create_admin = args.next(),
            "-V" | "--version" => {
                println!("b-artifact-server {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other => {
                eprintln!("未知参数: {other}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    if let Err(e) = run(config_path.as_deref(), data_dir, listen, admin_dir, create_admin) {
        eprintln!("b-artifact-server: {e}");
        std::process::exit(1);
    }
}

fn run(
    config_path: Option<&str>,
    data_dir: Option<String>,
    listen: Option<String>,
    admin_dir: Option<String>,
    create_admin: Option<String>,
) -> Result<(), error::AppError> {
    let mut cfg = Config::load(config_path.map(PathBuf::from).as_deref())?;
    if let Some(d) = data_dir {
        cfg.server.data_dir = d;
    }
    if let Some(l) = listen {
        cfg.server.listen = l;
    }
    if let Some(a) = admin_dir {
        cfg.server.admin_dir = a;
    }

    let data_dir = PathBuf::from(&cfg.server.data_dir);
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(data_dir.join("logs"))?;
    std::fs::create_dir_all(data_dir.join("uploads"))?;

    // 元数据库 + 迁移
    let db_path = data_dir.join("b-artifact.db");
    let mut conn = db::open(&db_path)?;
    db::migrate(&mut conn)?;

    // --create-admin：独立命令模式（不启动服务）
    if let Some(name) = create_admin {
        create_admin_user(&mut conn, &name)?;
        return Ok(());
    }
    drop(conn);

    let blob = BlobStore::new(&data_dir)?;
    let db = DbPool::new(&db_path, cfg.db.pool_size)?;
    let state = SharedState(Arc::new(AppState::new(
        db,
        blob,
        cfg.clone(),
        data_dir.join("uploads"),
    )));
    let app = api::router(state);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| error::AppError::Internal(format!("tokio runtime: {e}")))?;

    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&cfg.server.listen)
            .await
            .map_err(|e| error::AppError::Internal(format!(
                "监听 {} 失败: {e}", cfg.server.listen
            )))?;
        println!(
            "b-artifact-server v{} 已启动: http://{}  (data_dir: {})",
            env!("CARGO_PKG_VERSION"),
            cfg.server.listen,
            data_dir.display()
        );
        let admin_dir = PathBuf::from(&cfg.server.admin_dir);
        if admin_dir.join("index.html").exists() {
            println!("管理端: http://{}/admin/", cfg.server.listen);
        } else {
            println!(
                "管理端: 未构建（{} 下没有 index.html），`cd admin && npm run build` 后自动生效",
                admin_dir.display()
            );
        }
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|e| error::AppError::Internal(format!("server error: {e}")))
    })
}

/// 用户名规则：字母数字与 . _ -，长度 1~64（与 LDAP uid 常见约束兼容）。
fn valid_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        && !name.starts_with('.')
        && !name.starts_with('-')
}

/// 交互式创建本地管理员（stdin 两次输入密码，不回显由终端 noecho 决定，M1 简化实现）。
fn create_admin_user(conn: &mut rusqlite::Connection, username: &str) -> Result<(), error::AppError> {
    if !valid_username(username) {
        return Err(error::AppError::InvalidArgument(format!(
            "用户名 `{username}` 不合法（仅限字母数字与 . _ -，长度 1~64）"
        )));
    }

    let exists: Option<i64> = conn
        .query_row(
            "SELECT id FROM users WHERE username = ?1",
            rusqlite::params![username],
            |r| r.get(0),
        )
        .ok();
    if exists.is_some() {
        return Err(error::AppError::Conflict {
            code: "NAME_COLLISION",
            message: format!("用户 `{username}` 已存在"),
            details: serde_json::json!({ "username": username }),
        });
    }

    let mut stdout = std::io::stdout();
    print!("为管理员 `{username}` 设置密码（≥8 位，需含数字/大写/小写/符号）: ");
    stdout.flush().ok();
    let pw1 = read_line_trimmed()?;
    print!("再次输入密码: ");
    stdout.flush().ok();
    let pw2 = read_line_trimmed()?;
    println!();

    // §9.2 口令强度（v0.4.17）：CLI 与 HTTP 两侧共用同一个策略入口
    auth::password::validate_policy(&pw1)?;
    if pw1 != pw2 {
        return Err(error::AppError::InvalidArgument("两次输入的密码不一致".into()));
    }

    let phc = auth::password::hash_password(&pw1)?;
    let tx = conn
        .transaction()
        .map_err(|e| error::AppError::Internal(format!("开启事务失败: {e}")))?;
    tx.execute(
        "INSERT INTO users (username, source, password_hash, is_admin, disabled, created_at)
         VALUES (?1, 'local', ?2, 1, 0, ?3)",
        rusqlite::params![username, phc, chrono::Utc::now().to_rfc3339()],
    )?;
    let uid = tx.last_insert_rowid();
    audit::log(
        &tx,
        Some(uid),
        None,
        "user.create_admin",
        &format!("user:{uid}"),
        &format!("CLI --create-admin，用户名 {username}"),
        "localhost",
    )?;
    tx.commit()
        .map_err(|e| error::AppError::Internal(format!("提交事务失败: {e}")))?;

    println!("管理员 `{username}` 已创建（注意：users 表不再为空，首登自动管理员不会触发）");
    Ok(())
}

fn read_line_trimmed() -> Result<String, error::AppError> {
    let mut buf = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut buf)
        .map_err(|e| error::AppError::Internal(format!("读取输入失败: {e}")))?;
    Ok(buf.trim_end_matches(['\r', '\n']).to_string())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    println!("收到退出信号，正在关闭…");
}
