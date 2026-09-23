-- M1：pending_commits 补充载荷列。
-- 方案 §5.3/§7.3：POST /commit 只携带 {commit_id, commit_token, message}，
-- 变更清单必须在 prepare 阶段由服务端留存（v0.4.3 §3.2 遗漏此列，v0.4.4 补齐）。
ALTER TABLE pending_commits ADD COLUMN payload_json TEXT NOT NULL DEFAULT '{}';
