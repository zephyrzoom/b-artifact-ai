//! `acl_rules` 的读写与权限求解（方案 §4）。
//!
//! 纯规则语义在 [`crate::acl`]（纯函数、零 DB 依赖）；本模块只负责：
//! 把表读成规则集、按路径求级别、以及规则的增删改。

use crate::acl::{self, AclRule, Level, Principal, Subject};
use crate::error::AppError;
use rusqlite::{params, Connection};
use std::collections::HashMap;

/// 一个仓库的全部规则，按前缀分好组（避免每回溯一层都过滤全表）。
#[derive(Debug, Default)]
pub struct AclSet {
    by_prefix: HashMap<String, Vec<AclRule>>,
    all: Vec<AclRule>,
}

impl AclSet {
    pub fn level(&self, path: &str, user: &Principal) -> Level {
        acl::effective_level_by(
            |p| self.by_prefix.get(p).map(|v| v.as_slice()).unwrap_or(&[]),
            path,
            user,
        )
    }

    /// 校验某路径是否满足最低权限；不满足 → `403 PERMISSION_DENIED` 并指明路径（§4.3 逐路径校验）。
    pub fn require(&self, path: &str, user: &Principal, need: Level) -> Result<(), AppError> {
        let got = self.level(path, user);
        if got.at_least(need) {
            return Ok(());
        }
        Err(AppError::PermissionDenied(format!(
            "路径 `{}` 需要 {} 权限，当前为 {}",
            if path.is_empty() { "/" } else { path },
            need.as_str(),
            got.as_str()
        )))
    }

    pub fn rules(&self) -> &[AclRule] {
        &self.all
    }

    /// 某个前缀下的规则切片（有效权限预览器逐层回溯用，§8.3）。
    pub fn layer(&self, prefix: &str) -> &[AclRule] {
        self.by_prefix.get(prefix).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

impl AclSet {
    /// 有效权限解析链路（§8.2「有效权限预览器」）。
    ///
    /// 返回从最具体到最泛的每一层：该层有哪些规则、`pick` 命中了哪条、
    /// 是"命中"还是"屏障截断"还是"继续回溯"。把 §4.2 的算法可视化——
    /// 权限配置最容易出错的地方就在这里。
    pub fn trace(&self, path: &str, user: &Principal) -> serde_json::Value {
        if user.is_admin {
            return serde_json::json!({
                "path": path,
                "level": Level::Admin.as_str(),
                "reason": "系统管理员穿透一切（§4.2）",
                "steps": [],
            });
        }
        let mut steps = vec![];
        for prefix in acl::prefixes_for_path(path) {
            let rules = self.layer(&prefix);
            let shown = if prefix.is_empty() { "/" } else { prefix.as_str() };
            if rules.is_empty() {
                steps.push(serde_json::json!({
                    "prefix": prefix, "display": shown, "outcome": "skip",
                    "level": serde_json::Value::Null,
                    "reason": "该目录无规则，继续向父目录回溯",
                }));
                continue;
            }
            let listed: Vec<serde_json::Value> = rules.iter().map(rule_json).collect();
            match acl::pick(rules, user) {
                Some(hit) => {
                    steps.push(serde_json::json!({
                        "prefix": prefix, "display": shown, "outcome": "hit",
                        "level": hit.level.as_str(),
                        "rule_id": hit.id,
                        "rules": listed,
                        "reason": format!("命中规则 #{}（主体 {}）", hit.id, subject_label(&hit.subject)),
                    }));
                    return serde_json::json!({
                        "path": path,
                        "level": hit.level.as_str(),
                        "reason": format!("在 `{}` 命中规则 #{}", shown, hit.id),
                        "steps": steps,
                    });
                }
                None => {
                    let barrier = rules.iter().any(|r| !r.inherit);
                    if barrier {
                        steps.push(serde_json::json!({
                            "prefix": prefix, "display": shown, "outcome": "barrier",
                            "level": Level::None.as_str(),
                            "rules": listed,
                            "reason": "未命中任何规则，但该层存在继承屏障 → 截断回溯，结果为 none",
                        }));
                        return serde_json::json!({
                            "path": path,
                            "level": Level::None.as_str(),
                            "reason": format!("`{}` 是继承屏障且无命中规则，回溯在此截断", shown),
                            "steps": steps,
                        });
                    }
                    steps.push(serde_json::json!({
                        "prefix": prefix, "display": shown, "outcome": "miss",
                        "level": serde_json::Value::Null,
                        "rules": listed,
                        "reason": "有规则但都不匹配该用户，且无屏障 → 继续回溯",
                    }));
                }
            }
        }
        serde_json::json!({
            "path": path,
            "level": Level::None.as_str(),
            "reason": "回溯到仓库根仍无命中规则 → 默认拒绝（fail-closed，§4.2）",
            "steps": steps,
        })
    }
}

fn subject_label(s: &Subject) -> String {
    match s {
        Subject::Everyone => "everyone".to_string(),
        Subject::Group(g) => format!("group:{g}"),
        Subject::User(u) => format!("user:{u}"),
    }
}

/// 加载仓库的全部规则。
pub fn load(conn: &Connection, repo_id: i64) -> Result<AclSet, AppError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, path_prefix, subject_type, subject_id, level, inherit
           FROM acl_rules WHERE repo_id = ?1 ORDER BY path_prefix, id",
    )?;
    let rows = stmt.query_map(params![repo_id], |r| {
        let kind: String = r.get(2)?;
        let sid: i64 = r.get(3)?;
        let level: String = r.get(4)?;
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            acl::Subject::parse(&kind, sid),
            acl::Level::parse(&level),
            r.get::<_, i64>(5)? != 0,
        ))
    })?;
    let mut set = AclSet::default();
    for row in rows {
        let (id, path_prefix, subject, level, inherit) = row?;
        let subject = subject
            .ok_or_else(|| AppError::Internal(format!("acl_rules#{id}: 未知 subject_type")))?;
        let level =
            level.ok_or_else(|| AppError::Internal(format!("acl_rules#{id}: 未知 level")))?;
        let rule = AclRule { id, path_prefix: path_prefix.clone(), subject, level, inherit };
        set.by_prefix.entry(path_prefix).or_default().push(rule.clone());
        set.all.push(rule);
    }
    Ok(set)
}

/// 单条路径的权限级别（一次性加载规则的简便入口；批量校验请用 [`load`] + [`AclSet`]）。
pub fn level_for(
    conn: &Connection,
    repo_id: i64,
    path: &str,
    user: &Principal,
) -> Result<Level, AppError> {
    Ok(load(conn, repo_id)?.level(path, user))
}

/// 写入/更新一条规则（`UNIQUE(repo_id, path_prefix, subject_type, subject_id)` 冲突即覆盖）。
pub fn upsert_rule(
    conn: &Connection,
    repo_id: i64,
    path_prefix: &str,
    subject: Subject,
    level: Level,
    inherit: bool,
) -> Result<i64, AppError> {
    // 根前缀 `''` 是合法的权限前缀（仓库根），但它不是合法的"变更路径"，
    // 所以只对非空前缀走 validate_path（段名合法、无穿越）。
    if !path_prefix.is_empty() {
        crate::storage::repo::validate_path(path_prefix)?;
    }
    let (kind, sid) = match subject {
        Subject::Everyone => ("everyone", 0i64),
        Subject::Group(g) => ("group", g),
        Subject::User(u) => ("user", u),
    };
    conn.execute(
        "INSERT INTO acl_rules (repo_id, path_prefix, subject_type, subject_id, level, inherit, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (repo_id, path_prefix, subject_type, subject_id)
         DO UPDATE SET level = excluded.level, inherit = excluded.inherit",
        params![
            repo_id,
            path_prefix,
            kind,
            sid,
            level.as_str(),
            inherit as i64,
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_rule(conn: &Connection, repo_id: i64, id: i64) -> Result<bool, AppError> {
    let n = conn.execute(
        "DELETE FROM acl_rules WHERE repo_id = ?1 AND id = ?2",
        params![repo_id, id],
    )?;
    Ok(n > 0)
}

/// 建仓基线（§4.2 语义要点 4）：`'' + everyone + read`，管理员可在管理页删除。
pub fn seed_default(conn: &Connection, repo_id: i64) -> Result<(), AppError> {
    upsert_rule(conn, repo_id, "", Subject::Everyone, Level::Read, true)?;
    Ok(())
}

/// 规则 → JSON（管理页权限矩阵用）。
pub fn rule_json(r: &AclRule) -> serde_json::Value {
    let (kind, sid) = match r.subject {
        Subject::Everyone => ("everyone", 0i64),
        Subject::Group(g) => ("group", g),
        Subject::User(u) => ("user", u),
    };
    serde_json::json!({
        "id": r.id,
        "path_prefix": r.path_prefix,
        "subject_type": kind,
        "subject_id": sid,
        "level": r.level.as_str(),
        "inherit": r.inherit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db;
    use crate::storage::repo::{create_repo, create_user};
    use rusqlite::Connection;

    fn setup() -> (tempfile::TempDir, Connection, i64, i64, i64) {
        let (dir, conn) = db::tests::test_db();
        let alice = create_user(&conn, "alice", true).unwrap();
        let li = create_user(&conn, "li", false).unwrap();
        let repo = create_repo(&conn, "assets", alice).unwrap();
        (dir, conn, repo, alice, li)
    }

    /// 建组并把 li 加入 art 组（组 id 直接用 create 的返回值）
    fn art_group(conn: &Connection, li: i64) -> i64 {
        conn.execute(
            "INSERT INTO groups (name, comment, created_at) VALUES ('art', '', '2026-01-01T00:00:00+00:00')",
            [],
        )
        .unwrap();
        let gid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO group_members (group_id, user_id) VALUES (?1, ?2)",
            params![gid, li],
        )
        .unwrap();
        gid
    }

    #[test]
    fn seed_default_gives_everyone_read() {
        let (_d, conn, repo, _alice, li) = setup();
        seed_default(&conn, repo).unwrap();
        let p = Principal::new(li, vec![], false);
        assert_eq!(level_for(&conn, repo, "any/where.txt", &p).unwrap(), Level::Read);
        assert!(!level_for(&conn, repo, "any/where.txt", &p).unwrap().at_least(Level::Write));
    }

    #[test]
    fn db_rules_drive_effective_level() {
        let (_d, conn, repo, alice, li) = setup();
        seed_default(&conn, repo).unwrap();
        let art = art_group(&conn, li);
        upsert_rule(&conn, repo, "characters", Subject::Group(art), Level::Write, true).unwrap();
        upsert_rule(&conn, repo, "characters/boss/secret", Subject::Everyone, Level::None, true)
            .unwrap();

        let p = Principal::new(li, vec![art], false);
        assert_eq!(level_for(&conn, repo, "characters/mob/m1.fbx", &p).unwrap(), Level::Write);
        assert_eq!(
            level_for(&conn, repo, "characters/boss/secret/e.psd", &p).unwrap(),
            Level::None,
            "就近覆盖：secret 的 everyone:none 压过 characters 的 write"
        );
        assert_eq!(level_for(&conn, repo, "tools/x.py", &p).unwrap(), Level::Read, "回落到根基线");

        // 系统管理员穿透
        let admin = Principal::new(alice, vec![], true);
        assert_eq!(
            level_for(&conn, repo, "characters/boss/secret/e.psd", &admin).unwrap(),
            Level::Admin
        );
    }

    #[test]
    fn upsert_overwrites_same_key() {
        let (_d, conn, repo, _alice, _li) = setup();
        // 建仓已写入 `'' + everyone + read` 基线，故这里是「基线 + 1 条」
        let base = load(&conn, repo).unwrap().rules().len();
        upsert_rule(&conn, repo, "art", Subject::User(7), Level::Read, true).unwrap();
        upsert_rule(&conn, repo, "art", Subject::User(7), Level::Admin, false).unwrap();
        let set = load(&conn, repo).unwrap();
        assert_eq!(set.rules().len(), base + 1, "同键覆盖而非新增");
        let r = set.rules().iter().find(|r| r.path_prefix == "art").unwrap();
        assert_eq!(r.level, Level::Admin);
        assert!(!r.inherit);
    }

    #[test]
    fn delete_rule_removes_it() {
        let (_d, conn, repo, _alice, _li) = setup();
        let id = upsert_rule(&conn, repo, "art", Subject::User(7), Level::Read, true).unwrap();
        assert!(delete_rule(&conn, repo, id).unwrap());
        assert!(
            load(&conn, repo).unwrap().rules().iter().all(|r| r.id != id),
            "删除后不应再出现该规则（建仓基线规则仍在）"
        );
        assert!(!delete_rule(&conn, repo, id).unwrap(), "重复删除返回 false");
    }

    #[test]
    fn require_reports_the_offending_path() {
        let (_d, conn, repo, _alice, li) = setup();
        seed_default(&conn, repo).unwrap();
        let set = load(&conn, repo).unwrap();
        let p = Principal::new(li, vec![], false);
        assert!(set.require("a.txt", &p, Level::Read).is_ok());
        let err = set.require("a.txt", &p, Level::Write).unwrap_err();
        assert_eq!(err.code(), "PERMISSION_DENIED");
        assert!(err.to_string().contains("a.txt"), "错误信息要指明越权路径");
    }
}
