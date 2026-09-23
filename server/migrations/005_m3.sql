-- M3 管理端（§7.2 管理接口 / §3.7 GC / §3.6 purge）
-- 说明：管理端引入了三种全新查询模式，001~004 的索引都覆盖不到，逐个补。

-- ========== blob 引用重建（§3.7 关键语义 2） ==========
-- `rebuild_refcount()` 按 blob_hash 反查 changes。blob 是**全局**的（跨仓库去重），
-- 反查时不能带 repo_id，而 003 建的 idx_changes_blob 是 (repo_id, blob_hash)
-- 复合索引——repo_id 是前导列，缺了它就用不上。必须再建单列索引。
CREATE INDEX idx_changes_blob_hash ON changes(blob_hash) WHERE blob_hash IS NOT NULL;

-- ========== 审计日志筛选（§7.2 GET /admin/audit） ==========
-- 管理页按 用户 / 动作 / 仓库 + 时间倒序过滤；001 只有 ts 单列索引。
-- user_id 可为 NULL，故索引建在 (user_id, ts DESC) 上，IS NULL 查询走全表但量级可控。
CREATE INDEX idx_audit_user   ON audit_log(user_id, ts DESC);
CREATE INDEX idx_audit_action ON audit_log(action, ts DESC);
CREATE INDEX idx_audit_repo   ON audit_log(repo_id, ts DESC);

-- ========== 概览统计（§8.2 Dashboard） ==========
-- 按仓库统计 HEAD 文件数 / 体积。主键 (repo_id, path) 只能界住 repo_id，
-- 带 kind='file' 过滤仍要回表逐行判；补 (repo_id, kind) 后可纯索引计数。
CREATE INDEX idx_head_entries_kind ON head_entries(repo_id, kind);
