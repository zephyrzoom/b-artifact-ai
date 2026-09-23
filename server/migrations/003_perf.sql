-- M1.6 性能修复（压测报告 M1.5：B4 / B5）+ M1 遗留列
-- 说明：head_entries 的目录行由"仅显式 mkdir"扩展为"物化全部目录（含隐式）"，
--       使列目录可用 parent_path 等值查询（O(直接子项)）而非前缀扫描（O(整棵子树)）。
--       parent_path 的历史数据回填在 Rust 侧完成（SQLite 无"从右查找"函数）。

-- B4：父目录路径（'' = 仓库根）+ 是否显式目录（1 = mkdir 产生，空目录也不被回收）
ALTER TABLE head_entries ADD COLUMN parent_path TEXT NOT NULL DEFAULT '';
ALTER TABLE head_entries ADD COLUMN is_explicit INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_head_entries_parent ON head_entries(repo_id, parent_path);

-- B5：提交时 blob 新颖性反查（byte_delta）由整仓扫描变为索引点查
CREATE INDEX idx_changes_blob ON changes(repo_id, blob_hash) WHERE blob_hash IS NOT NULL;

-- M1 遗留（§5.5）：破锁留痕
ALTER TABLE locks ADD COLUMN broken_at    TEXT;                       -- 破锁时间
ALTER TABLE locks ADD COLUMN broken_by    INTEGER REFERENCES users(id);
ALTER TABLE locks ADD COLUMN break_reason TEXT NOT NULL DEFAULT '';
