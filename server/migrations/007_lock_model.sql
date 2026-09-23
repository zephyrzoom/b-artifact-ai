-- v0.4.17 锁模型收敛（§5.1 / §5.2）：删掉 advisory 策略与目录锁。
--
-- 三件事：
--   1. 所有仓库统一成 strict —— "先锁后提交"不再有开关，每个变更路径都要本人持锁；
--   2. 清空 needs_lock —— glob 白名单语义已删除（原来只有 strict 下生效）；
--   3. 删掉存量目录锁 —— dir 锁的语义（递归覆盖子树、对新文件自动生效、破锁连带释放子树）
--      已从服务端整体移除，这些行不再有任何解释者。**这里是真删行**：
--      留着的话，`UNIQUE(repo_id, path)` 会挡住用户在同一目录名下加文件锁之外的任何操作，
--      而且它们永远不会被判定为有效锁（is_live 的调用方只走文件路径）。
--      需要留证的部署请在升级前 `SELECT * FROM locks WHERE kind = 'dir'` 备份。
--
-- 两列刻意保留（不 DROP COLUMN）：SQLite 删列要重建表，而 allow_anon 等列与索引都要跟着动；
-- 代价与收益不成比例。写入侧由 `storage::repo::create_repo` 固定写 'strict' / ''。

UPDATE repos SET lock_policy = 'strict';
UPDATE repos SET needs_lock = '';
DELETE FROM locks WHERE kind <> 'file';
