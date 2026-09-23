-- 修正 head_entries 文件行的 changed_rev（"该条目内容最后一次变化的修订"）。
--
-- 背景：这个字段在老库里不可信——M1.5 时代的一次批量回填把**所有行**都写成了当时的
-- head_rev（实测 24 万文件档位下 265,641 行全是 head_rev，而某个文件真实历史是
-- add@1 / modify@3551 / modify@14024）。它对外是可见字段（`tree` 响应带 changed_rev，
-- 管理端与客户端都展示"最后变更修订"），所以错值本身就是 bug；
-- 而历史修订列目录（`list_dir_at`）的快路径也以它为判据，必须可信。
--
-- 只重算**文件行**：文件在 `changes` 里必然有自己的行，MAX(rev) 就是准确答案。
-- 目录行的 changed_rev 是"物化时顺带记下的子项 rev"，语义本就模糊（既有行为，不动）。
--
-- 幂等：重算结果只取决于 changes，重复执行结果相同。
UPDATE head_entries
   SET changed_rev = (
       SELECT MAX(c.rev) FROM changes c
        WHERE c.repo_id = head_entries.repo_id AND c.path = head_entries.path
   )
 WHERE kind = 'file'
   AND EXISTS (
       SELECT 1 FROM changes c
        WHERE c.repo_id = head_entries.repo_id AND c.path = head_entries.path
   );
