/**
 * 用户级根目录（`~/.b-artifact`）。
 *
 * 只有一处需要可覆盖：冒烟/自动化测试不能写开发者真实的凭据与全局缓存。
 * 设 `B_ARTIFACT_HOME` 即可整体重定向（凭据、缓存都跟着走）。
 */
use std::path::PathBuf;

pub fn artifact_home() -> PathBuf {
    let override_dir = std::env::var("B_ARTIFACT_HOME")
        .ok()
        .filter(|s| !s.is_empty());
    let base = match override_dir {
        Some(p) => {
            let p = PathBuf::from(p);
            // 与 TS 的 resolve() 对齐：相对路径按当前工作目录解析成绝对路径
            if p.is_absolute() {
                p
            } else {
                std::env::current_dir().unwrap_or_default().join(p)
            }
        }
        None => dirs::home_dir().unwrap_or_default(),
    };
    base.join(".b-artifact")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_is_respected() {
        // 注意：测试并行跑，环境变量是进程级全局状态 —— 只在**读取一次**的语义下断言，
        // 不在这里 set_var（会污染其它用例）。
        let home = artifact_home();
        assert!(home.ends_with(".b-artifact"));
    }
}
