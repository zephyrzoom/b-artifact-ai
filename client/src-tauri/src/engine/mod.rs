/**
 * 引擎（原 `client/src/core/` 下那批 TS 文件的 Rust 移植）。
 *
 * 与 TS 版的对应关系见 `docs/客户端Tauri迁移方案.md` §4。所有模块都是纯逻辑：
 * 不依赖 tauri（CLI 与桌面壳共用），外部能力（网络、时钟、文件系统）尽量以参数注入，
 * 便于在 `cargo test` 里跑完整流程。
 */
pub mod api;
pub mod cache;
pub mod db;
pub mod errors;
pub mod hash;
pub mod home;
pub mod ignore;
pub mod io_retry;
pub mod paths;
pub mod pristine;
pub mod revisions;
pub mod scan;
pub mod text;
pub mod wc;
