-- M2 权限与锁（§4 ACL / §5 锁）
-- 说明：acl_rules / locks / audit_log 三张表在 001 已建，本迁移只补 M2 引入的
--       热路径索引与锁生命周期所需的列。全部语句对存量库安全（幂等由
--       schema_migrations 保证只跑一次）。

-- ========== ACL 热路径 ==========
-- 解析用户权限时需要"该用户属于哪些组"（§4.2 pick 里的 group 主体）。
-- group_members 主键是 (group_id, user_id)，按 user_id 反查会全表扫。
CREATE INDEX idx_group_members_user ON group_members(user_id);

-- 同一目录下多条规则的取值：UNIQUE(repo_id, path_prefix, subject_type, subject_id)
-- 已覆盖 (repo_id, path_prefix) 前缀，无需再加。

-- ========== 锁 ==========
-- 有效性判定只看未过期锁（§5.5：过期即失效），且"某路径是否被锁"是提交校验热路径。
CREATE INDEX idx_locks_live ON locks(repo_id, path) WHERE broken_at IS NULL;

-- 锁续期（§5.5 心跳 10 分钟一次）按 owner 批量取。
CREATE INDEX idx_locks_owner_live ON locks(repo_id, owner_id) WHERE broken_at IS NULL;

-- ========== 下载授权 ==========
-- GET /blobs/{hash} 需要反查"哪些路径引用了该 blob"再做逐路径 read 校验（§4.3）。
-- head_entries 只有 (repo_id, path) 主键，按 blob_hash 反查会全表扫。
CREATE INDEX idx_head_entries_blob ON head_entries(repo_id, blob_hash);
