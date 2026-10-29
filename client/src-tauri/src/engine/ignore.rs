/**
 * `.b-artifactignore` 解析（gitignore 语法子集，§6.2 `ignored` 状态）。
 *
 * 只实现实际会用到的部分：注释、取反、目录限定、锚定、glob 通配与字符类。
 * 与 TS 版逐行对应（含同一套 glob→正则 的翻译与「任意层级前缀」的写法），
 * 保证两版对同一份规则文件的判定完全一致。
 */
use std::path::Path;

use regex::Regex;

pub const IGNORE_FILE: &str = ".b-artifactignore";

struct Rule {
    re: Regex,
    negated: bool,
    dir_only: bool,
}

/// 与 TS 的 `escapeRe` 同一组字符：`. * + ? ^ $ { } ( ) | [ ] \`
fn escape_re(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// glob → 正则片段（只处理 `*`、`**`、`?`、`[abc]`）。
fn glob_to_regex(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut re = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '*' {
            let mut j = i;
            while j < chars.len() && chars[j] == '*' {
                j += 1;
            }
            let stars = j - i;
            if stars >= 2 {
                if j < chars.len() && chars[j] == '/' {
                    // `**/foo`：任意层级前缀（含零层）
                    re.push_str("(?:.*/)?");
                    i = j + 1;
                } else {
                    re.push_str(".*");
                    i = j;
                }
            } else {
                re.push_str("[^/]*");
                i = j;
            }
        } else if c == '?' {
            re.push_str("[^/]");
            i += 1;
        } else if c == '[' {
            match chars[i + 1..].iter().position(|&x| x == ']') {
                None => {
                    re.push_str("\\[");
                    i += 1;
                }
                Some(off) => {
                    let end = i + 1 + off;
                    let cls: String = chars[i + 1..end].iter().collect();
                    let cls = match cls.strip_prefix('!') {
                        Some(rest) => format!("^{rest}"),
                        None => cls,
                    };
                    re.push('[');
                    re.push_str(&cls);
                    re.push(']');
                    i = end + 1;
                }
            }
        } else {
            re.push_str(&escape_re(&c.to_string()));
            i += 1;
        }
    }
    re
}

/// 忽略规则集合。判定遵循 git 语义：**最后匹配的规则生效**，取反可覆盖先前的忽略。
#[derive(Default)]
pub struct IgnoreRules {
    rules: Vec<Rule>,
}

impl IgnoreRules {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// 从文本载入规则；`base` 为规则相对目录（`""` 表示根）。
    pub fn add(&mut self, base: &str, text: &str) -> &mut Self {
        for raw_line in text.split('\n') {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let mut negated = false;
            let mut pattern = line.to_string();
            if let Some(rest) = pattern.strip_prefix('!') {
                negated = true;
                pattern = rest.to_string();
            }
            // 行尾空格已被 trim 掉；git 里需要用 `\ ` 转义，这里不支持（与 TS 版一致）。
            let mut dir_only = false;
            if pattern.ends_with('/') {
                dir_only = true;
                pattern.pop();
            }
            if pattern.is_empty() {
                continue;
            }

            let mut anchored = false;
            if let Some(rest) = pattern.strip_prefix('/') {
                anchored = true;
                pattern = rest.to_string();
            } else if pattern.contains('/') {
                // 含分隔符（尾部 `/` 已剥离）的规则锚定到 base。
                anchored = true;
            }

            let body = glob_to_regex(&pattern);
            let prefix = if base.is_empty() {
                String::new()
            } else {
                format!("{base}/")
            };
            let src = if anchored {
                format!("^{}{}$", escape_re(&prefix), body)
            } else {
                format!("^{}(?:.*/)?{}$", escape_re(&prefix), body)
            };
            // 规则文本来自用户，非法正则不应让整个程序崩掉：跳过这一条即可。
            if let Ok(re) = Regex::new(&src) {
                self.rules.push(Rule {
                    re,
                    negated,
                    dir_only,
                });
            }
        }
        self
    }

    /// 从文件载入；文件不存在则忽略（等价于空规则集）。
    pub fn add_file(&mut self, base: &str, abs_path: &Path) -> &mut Self {
        match std::fs::read_to_string(abs_path) {
            Ok(text) => self.add(base, &text),
            Err(_) => self,
        }
    }

    /// 路径是否被忽略。
    pub fn matches(&self, rel_path: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for r in &self.rules {
            if r.dir_only && !is_dir {
                continue;
            }
            if r.re.is_match(rel_path) {
                ignored = !r.negated;
            }
        }
        ignored
    }

    pub fn size(&self) -> usize {
        self.rules.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(text: &str, base: &str) -> IgnoreRules {
        let mut r = IgnoreRules::new();
        r.add(base, text);
        r
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let r = rules("# 注释\n\n*.tmp\n", "");
        assert_eq!(r.size(), 1);
        assert!(r.matches("a.tmp", false));
    }

    #[test]
    fn unanchored_matches_any_depth() {
        let r = rules("*.psd", "");
        assert!(r.matches("a.psd", false));
        assert!(r.matches("deep/nest/a.psd", false));
        assert!(!r.matches("a.psdx", false));
    }

    #[test]
    fn leading_slash_anchors_to_base() {
        let r = rules("/build", "");
        assert!(r.matches("build", true));
        assert!(!r.matches("sub/build", true));
    }

    #[test]
    fn trailing_slash_means_dir_only() {
        let r = rules("cache/", "");
        assert!(r.matches("cache", true));
        assert!(!r.matches("cache", false));
    }

    #[test]
    fn pattern_with_separator_is_anchored() {
        let r = rules("a/b.txt", "");
        assert!(r.matches("a/b.txt", false));
        assert!(!r.matches("x/a/b.txt", false));
    }

    #[test]
    fn double_star_crosses_levels() {
        let r = rules("**/intermediate/**", "");
        assert!(r.matches("proj/intermediate/x.bin", false));
        assert!(r.matches("a/b/intermediate/x.bin", false));
        // git 语义：前导 `**/` 匹配「所有目录」，含当前目录，故根层也命中。
        assert!(r.matches("intermediate/x.bin", false));
        assert!(!r.matches("intermediateX/x.bin", false));
    }

    #[test]
    fn negation_overrides_earlier_rule() {
        let r = rules("*.psd\n!important.psd", "");
        assert!(r.matches("a.psd", false));
        assert!(!r.matches("important.psd", false));
    }

    #[test]
    fn char_class_and_single_char_wildcard() {
        let r = rules("v[0-9].bin\nq?.dat", "");
        assert!(r.matches("v3.bin", false));
        assert!(!r.matches("vx.bin", false));
        assert!(r.matches("q1.dat", false));
        assert!(!r.matches("q12.dat", false));
    }

    #[test]
    fn base_scopes_the_rule() {
        let r = rules("*.tmp", "art");
        assert!(r.matches("art/x.tmp", false));
        assert!(!r.matches("code/x.tmp", false));
    }

    #[test]
    fn add_file_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join(IGNORE_FILE);
        std::fs::write(&f, "*.bak\n").unwrap();
        let mut r = IgnoreRules::new();
        r.add_file("", &f);
        assert!(r.matches("x.bak", false));

        let mut r2 = IgnoreRules::new();
        r2.add_file("", &dir.path().join("nope"));
        assert_eq!(r2.size(), 0);
    }
}
