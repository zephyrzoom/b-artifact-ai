//! b-artifact 集中式二进制资产版本管理服务端——库入口。
//!
//! 二进制目标（main.rs）只负责参数解析与进程生命周期；
//! 全部可复用逻辑在此库中，供集成测试（tests/api_m1.rs）直接装配路由。

pub mod acl;
pub mod api;
pub mod audit;
pub mod auth;
pub mod locks;
pub mod config;
pub mod error;
pub mod state;
pub mod storage;
