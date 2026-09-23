use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(name = "b-artifact-server", version, about = "b-artifact 服务端（面向二进制资产的集中式版本管理）")]
pub struct Cli {
    /// 数据目录（blobs 与 SQLite 所在位置）
    #[arg(long, default_value = "./data")]
    pub data_dir: PathBuf,

    /// 监听地址
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub listen: String,

    /// 首次启动时创建管理员账号（users 表为空时生效）
    #[arg(long)]
    pub bootstrap_admin: bool,

    /// 管理员初始密码；不传则随机生成并打印到 stdout
    #[arg(long)]
    pub admin_password: Option<String>,

    /// blob 压缩策略：off | auto
    #[arg(long, default_value = "auto")]
    pub compression: String,

    /// 会话有效期（天）
    #[arg(long, default_value_t = 30)]
    pub session_ttl_days: i64,

    /// 单文件上传上限（MB）
    #[arg(long, default_value_t = 8192)]
    pub max_file_size_mb: u64,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub listen: String,
    pub compression: Compression,
    pub session_ttl_days: i64,
    pub max_file_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Off,
    Auto,
}

impl Config {
    pub fn from_cli(cli: &Cli) -> anyhow::Result<Self> {
        let compression = match cli.compression.as_str() {
            "off" | "raw" => Compression::Off,
            "auto" => Compression::Auto,
            other => anyhow::bail!("未知压缩策略: {other}（可选 off | auto）"),
        };
        Ok(Self {
            data_dir: cli.data_dir.clone(),
            listen: cli.listen.clone(),
            compression,
            session_ttl_days: cli.session_ttl_days,
            max_file_size: cli.max_file_size_mb.saturating_mul(1024 * 1024),
        })
    }
}
