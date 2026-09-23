//! 目录级权限引擎（方案 §4）——**纯函数，不碰数据库**，便于单测全覆盖。
//!
//! 数据访问层负责把 `acl_rules` 行读成 [`AclRule`]，本模块只回答两个问题：
//!
//! * 一个目录下哪些规则对当前用户生效（[`pick`]）
//! * 一条路径最终得到什么权限级别（[`effective_level`]）
//!
//! 术语（§4.2 定死）：`inherit=false` 的含义**不是**"只控制本目录"，而是**该目录是一条
//! 继承屏障（barrier）**——当用户对恰好该前缀没有任何匹配规则时，屏障阻止继续向父目录
//! 回溯，直接得到 `none`。屏障的检查与用户是否在该目录命中规则无关。

use std::collections::HashSet;

// ---------- 权限级别（§4.1） ----------

/// `none`（显式拒绝）< `read` < `write` < `admin`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Level {
    #[default]
    None,
    Read,
    Write,
    Admin,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "none" => Some(Level::None),
            "read" => Some(Level::Read),
            "write" => Some(Level::Write),
            "admin" => Some(Level::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Level::None => "none",
            Level::Read => "read",
            Level::Write => "write",
            Level::Admin => "admin",
        }
    }

    /// 是否满足某操作的最低权限要求（§4.3 权限 → 操作矩阵）。
    pub fn at_least(self, need: Level) -> bool {
        self >= need
    }
}

// ---------- 主体与规则（§4.1） ----------

/// 规则主体：`user:<id>` / `group:<id>` / `everyone`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    Everyone,
    Group(i64),
    User(i64),
}

impl Subject {
    /// 主体具体度（§4.2 `pick`）：越大越优先。用户 > 所在组 > 所有人。
    pub fn rank(self) -> u8 {
        match self {
            Subject::Everyone => 0,
            Subject::Group(_) => 1,
            Subject::User(_) => 2,
        }
    }

    pub fn parse(kind: &str, id: i64) -> Option<Subject> {
        match kind {
            "everyone" => Some(Subject::Everyone),
            "group" => Some(Subject::Group(id)),
            "user" => Some(Subject::User(id)),
            _ => None,
        }
    }

    /// 该主体是否命中此用户。
    pub fn matches(self, user: &Principal) -> bool {
        match self {
            Subject::Everyone => true,
            Subject::Group(g) => user.groups.contains(&g),
            Subject::User(u) => user.id == u,
        }
    }
}

/// 权限求解的输入主体：用户 id + 所属组 + 是否系统管理员。
#[derive(Debug, Clone, Default)]
pub struct Principal {
    pub id: i64,
    pub groups: HashSet<i64>,
    pub is_admin: bool,
}

impl Principal {
    pub fn new(id: i64, groups: Vec<i64>, is_admin: bool) -> Principal {
        Principal { id, groups: groups.into_iter().collect(), is_admin }
    }
}

/// 一条 ACL 规则（对应 `acl_rules` 一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclRule {
    pub id: i64,
    /// `''` = 仓库根。按**路径段**匹配，不是字符串前缀。
    pub path_prefix: String,
    pub subject: Subject,
    pub level: Level,
    /// `false` = 继承屏障（§4.2）
    pub inherit: bool,
}

// ---------- 解析算法（§4.2） ----------

/// `pick(rules, user)`：按 (主体具体度 DESC, 级别 ASC) 取最优规则。
///
/// 级别升序意味着**同优先级下 `none` 胜过 `read`**（显式拒绝 > 隐式允许），
/// 避免"组授权把个人封禁覆盖掉"。
pub fn pick<'a>(rules: &'a [AclRule], user: &Principal) -> Option<&'a AclRule> {
    let mut best: Option<&'a AclRule> = None;
    for r in rules {
        if !r.subject.matches(user) {
            continue;
        }
        match best {
            None => best = Some(r),
            Some(b) => {
                let (rank, brank) = (r.subject.rank(), b.subject.rank());
                if rank > brank || (rank == brank && r.level < b.level) {
                    best = Some(r);
                }
            }
        }
    }
    best
}

/// 单层的解析结果。
///
/// * `Some(level)` —— 命中规则（返回其级别）；或**未命中但该层存在屏障** → `Some(Level::None)`
/// * `None` —— 未命中且无屏障，继续向父目录回溯
///
/// 注意屏障规则本身也参与 `pick`（§4.2 注意点 1：屏障 = "这个目录的权限只由它自己的
/// 规则决定"，如果用户在屏障目录有命中规则，照样返回该级别）。
pub fn resolve_layer(rules: &[AclRule], user: &Principal) -> Option<Level> {
    if let Some(hit) = pick(rules, user) {
        return Some(hit.level);
    }
    if rules.iter().any(|r| !r.inherit) {
        return Some(Level::None);
    }
    None
}

/// 一条路径从最具体到最泛的全部前缀（含 `''` 根）。
///
/// 按**路径段**切分，因此 `art/a.psd` 得到 `["art", ""]`，
/// 而 `artist/a.psd` 得到 `["artist", ""]`——不会命中 `art` 的规则（§4.1）。
pub fn prefixes_for_path(path: &str) -> Vec<String> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut out = Vec::with_capacity(segs.len() + 1);
    for depth in (0..=segs.len()).rev() {
        out.push(segs[..depth].join("/"));
    }
    out
}

/// `effective_level(repo, path, user)`：自下而上回溯，**默认拒绝（fail-closed）**。
///
/// 系统管理员穿透一切（§4.2 第 1 行）。
pub fn effective_level(rules: &[AclRule], path: &str, user: &Principal) -> Level {
    if user.is_admin {
        return Level::Admin;
    }
    for prefix in prefixes_for_path(path) {
        let here: Vec<AclRule> = rules
            .iter()
            .filter(|r| r.path_prefix == prefix)
            .cloned()
            .collect();
        if let Some(level) = resolve_layer(&here, user) {
            return level;
        }
    }
    Level::None
}

/// DB 层专用：已按前缀分好组的规则表，避免每层过滤全表。
///
/// `lookup` 返回该前缀下的规则切片（没有则空切片）。
pub fn effective_level_by<'a>(
    lookup: impl Fn(&str) -> &'a [AclRule],
    path: &str,
    user: &Principal,
) -> Level {
    if user.is_admin {
        return Level::Admin;
    }
    for prefix in prefixes_for_path(path) {
        if let Some(level) = resolve_layer(lookup(&prefix), user) {
            return level;
        }
    }
    Level::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: i64, prefix: &str, subject: Subject, level: Level, inherit: bool) -> AclRule {
        AclRule { id, path_prefix: prefix.into(), subject, level, inherit }
    }

    /// §4.4 配置示例：仓库 `assets`
    ///
    /// | 目录 | 主体 | 级别 |
    /// |---|---|---|
    /// | `''` | everyone | read |
    /// | `characters` | group:art(1) | write |
    /// | `characters/boss` | group:art-lead(2) | write |
    /// | `characters/boss/secret` | everyone | none |
    /// | `tools` | user:zhang(100) | admin |
    /// | `engine` | group:engineer(3) | write |
    fn example_rules() -> Vec<AclRule> {
        vec![
            rule(1, "", Subject::Everyone, Level::Read, true),
            rule(2, "characters", Subject::Group(1), Level::Write, true),
            rule(3, "characters/boss", Subject::Group(2), Level::Write, true),
            rule(4, "characters/boss/secret", Subject::Everyone, Level::None, true),
            rule(5, "tools", Subject::User(100), Level::Admin, true),
            rule(6, "engine", Subject::Group(3), Level::Write, true),
        ]
    }

    /// 美术组（1）成员 li，非 art-lead。
    fn li() -> Principal {
        Principal::new(10, vec![1], false)
    }

    // ---- Level ----

    #[test]
    fn level_ordering_and_at_least() {
        assert!(Level::None < Level::Read);
        assert!(Level::Read < Level::Write);
        assert!(Level::Write < Level::Admin);
        assert!(Level::Admin.at_least(Level::Read));
        assert!(!Level::Read.at_least(Level::Write));
        assert!(Level::Read.at_least(Level::Read), "同级算满足");
        assert_eq!(Level::parse("admin").unwrap().as_str(), "admin");
        assert!(Level::parse("root").is_none());
    }

    // ---- prefixes_for_path：按路径段，不是字符串前缀 ----

    #[test]
    fn prefixes_are_segment_based() {
        // §4.2 伪代码 `for depth in (len(segs) .. 0)`：包含完整路径本身
        assert_eq!(prefixes_for_path("art/a.psd"), vec!["art/a.psd", "art", ""]);
        // 按路径段切分 → 不会命中 `art` 的规则
        assert_eq!(prefixes_for_path("artist/a.psd"), vec!["artist/a.psd", "artist", ""]);
        assert_eq!(prefixes_for_path(""), vec![""]);
        assert_eq!(prefixes_for_path("a/b/c"), vec!["a/b/c", "a/b", "a", ""]);
    }

    // ---- pick ----

    #[test]
    fn pick_prefers_more_specific_subject() {
        let rules = vec![
            rule(1, "x", Subject::Everyone, Level::Read, true),
            rule(2, "x", Subject::Group(1), Level::Write, true),
            rule(3, "x", Subject::User(10), Level::Admin, true),
        ];
        let hit = pick(&rules, &li()).unwrap();
        assert_eq!(hit.id, 3, "用户规则 > 组规则 > everyone");
    }

    #[test]
    fn pick_deny_wins_within_same_rank() {
        // 同为用户级：显式 none 胜过 read（§4.2 语义要点 3）
        let rules = vec![
            rule(1, "x", Subject::Group(1), Level::Read, true),
            rule(2, "x", Subject::Group(1), Level::None, true),
        ];
        assert_eq!(pick(&rules, &li()).unwrap().level, Level::None);
    }

    #[test]
    fn pick_returns_none_when_no_match() {
        let rules = vec![rule(1, "x", Subject::User(999), Level::Read, true)];
        assert!(pick(&rules, &li()).is_none());
    }

    // ---- effective_level：§4.2 语义要点 ----

    #[test]
    fn nearest_override() {
        // `characters/boss/secret` 的 everyone:none 就近覆盖 `characters` 的 group:art write
        let rules = example_rules();
        assert_eq!(
            effective_level(&rules, "characters/boss/secret/end.psd", &li()),
            Level::None
        );
        // `characters/mob` 未命中 boss 的规则 → 回溯到 characters
        assert_eq!(effective_level(&rules, "characters/mob/m1.fbx", &li()), Level::Write);
    }

    #[test]
    fn default_deny_when_no_rule() {
        let rules = vec![];
        assert_eq!(effective_level(&rules, "anything.psd", &li()), Level::None);
    }

    #[test]
    fn admin_penetrates_everything() {
        let admin = Principal::new(1, vec![], true);
        assert_eq!(effective_level(&[], "characters/boss/secret/x", &admin), Level::Admin);
        assert_eq!(
            effective_level(&example_rules(), "characters/boss/secret/x", &admin),
            Level::Admin
        );
    }

    #[test]
    fn segment_match_not_string_prefix() {
        // `art` 规则不应命中 `artist/...`
        let rules = vec![rule(1, "art", Subject::Everyone, Level::Write, true)];
        assert_eq!(effective_level(&rules, "art/a.psd", &li()), Level::Write);
        assert_eq!(effective_level(&rules, "artist/a.psd", &li()), Level::None);
    }

    // ---- 屏障（§4.2 v0.4 语义）----

    fn barrier_rules() -> Vec<AclRule> {
        let mut rules = example_rules();
        // §4.4 屏障示例：characters 上追加 group:engineer / read / inherit=false
        rules.push(rule(7, "characters", Subject::Group(3), Level::Read, false));
        rules
    }

    #[test]
    fn barrier_blocks_inheritance() {
        // 策划 zhao（无任何组）：characters/mob 无规则 → characters 上 art 组与 engineer 组
        // 都不命中 → 该层存在屏障 → NONE，即使仓库根给了 everyone read
        let zhao = Principal::new(30, vec![], false);
        assert_eq!(
            effective_level(&barrier_rules(), "characters/mob/m1.fbx", &zhao),
            Level::None
        );
    }

    #[test]
    fn barrier_blocks_inheritance_for_outside_group() {
        // 同上：art 组成员但也只有 art 组的 li 在 mob 下仍命中 characters 的 group:art write，
        // 说明屏障只在"未命中"时生效（§4.2 注意点 1）
        assert_eq!(
            effective_level(&barrier_rules(), "characters/mob/m1.fbx", &li()),
            Level::Write
        );
    }

    #[test]
    fn barrier_does_not_leak_to_other_subtrees() {
        // 屏障只作用于 characters 子树；tools/build.py 仍回溯到根的 everyone:read
        let wang = Principal::new(20, vec![3], false);
        assert_eq!(effective_level(&barrier_rules(), "tools/build.py", &wang), Level::Read);
    }

    #[test]
    fn barrier_rule_itself_participates_in_pick() {
        // 用户在屏障目录**有**命中 → 返回该规则级别（屏障规则本身也参与 pick）
        let wang = Principal::new(20, vec![3], false);
        // characters 目录下 wang 命中 group:engineer read(inherit=false)
        assert_eq!(effective_level(&barrier_rules(), "characters/a.psd", &wang), Level::Read);
    }

    #[test]
    fn barrier_at_root_denies_everything_below() {
        let rules = vec![
            rule(1, "", Subject::User(10), Level::Read, false), // 根上一条屏障
        ];
        let other = Principal::new(20, vec![], false);
        assert_eq!(effective_level(&rules, "a/b/c", &other), Level::None);
        assert_eq!(effective_level(&rules, "a/b/c", &li()), Level::Read);
    }

    #[test]
    fn barrier_needs_no_matching_rule_to_exist() {
        // 屏障的存在性与用户是否命中无关（§4.2 术语定死）
        let rules = vec![
            rule(1, "", Subject::Everyone, Level::Read, true),
            rule(2, "secret", Subject::User(999), Level::Admin, false),
        ];
        assert_eq!(effective_level(&rules, "secret/x", &li()), Level::None);
    }

    // ---- effective_level_by：与 effective_level 结果一致 ----

    #[test]
    fn by_lookup_matches_slice_version() {
        let rules = barrier_rules();
        let mut map: std::collections::HashMap<String, Vec<AclRule>> =
            std::collections::HashMap::new();
        for r in &rules {
            map.entry(r.path_prefix.clone()).or_default().push(r.clone());
        }
        for path in ["characters/boss/secret/e.psd", "tools/build.py", "engine/x.cpp"] {
            let slice = effective_level(&rules, path, &li());
            let by = effective_level_by(
                |p| map.get(p).map(|v| v.as_slice()).unwrap_or(&[]),
                path,
                &li(),
            );
            assert_eq!(slice, by, "路径 {path} 两种入口结果必须一致");
        }
    }
}
