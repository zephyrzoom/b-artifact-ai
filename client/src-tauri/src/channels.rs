/**
 * IPC 通道契约（Rust 侧唯一事实源）。
 *
 * 与 TS 版 `src/shared/channels.ts` 逐条对应（名字、必填、类型、长度上限、枚举、缺省值、
 * 以及**校验失败的中文文案**）。三条不变量：
 *   1. 白名单就是这张表 —— 前端调不到表外的通道；
 *   2. 每个通道自带 payload schema，进 Rust 第一件事就是按 schema 校验；
 *   3. 校验顺序与失败文案与 TS 版一致，渲染层无需感知实现语言换了。
 */
use serde::Serialize;
use serde_json::{Map, Value};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldType {
    Str,
    Num,
    Bool,
    StrArray,
}

#[derive(Clone, Copy, Debug)]
pub enum DefaultVal {
    Null,
    Bool(bool),
    Num(f64),
    Str(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub name: &'static str,
    pub ty: FieldType,
    pub required: bool,
    pub max_len: Option<usize>,
    pub values: Option<&'static [&'static str]>,
    pub default: Option<DefaultVal>,
}

impl Field {
    const fn new(name: &'static str, ty: FieldType) -> Self {
        Self {
            name,
            ty,
            required: false,
            max_len: None,
            values: None,
            default: None,
        }
    }
    const fn req(mut self) -> Self {
        self.required = true;
        self
    }
    const fn max(mut self, n: usize) -> Self {
        self.max_len = Some(n);
        self
    }
    const fn one_of(mut self, v: &'static [&'static str]) -> Self {
        self.values = Some(v);
        self
    }
}

const fn s(name: &'static str) -> Field {
    Field::new(name, FieldType::Str)
}
const fn n(name: &'static str) -> Field {
    Field::new(name, FieldType::Num)
}
const fn b(name: &'static str) -> Field {
    Field::new(name, FieldType::Bool)
}
const fn arr(name: &'static str) -> Field {
    Field::new(name, FieldType::StrArray)
}

/// 路径类参数的长度上限（与 §6.6 的全路径 1024 字节上限对齐，留出富余）。
pub const PATH_MAX: usize = 2048;
/// 冲突合并内容的长度上限（与文本对比上限同量级）。
pub const MERGE_MAX: usize = 2 * 1024 * 1024;

pub struct ChannelSpec {
    pub name: &'static str,
    pub fields: &'static [Field],
}

pub const CHANNEL_SPECS: &[ChannelSpec] = &[
    // ---------- 应用信息与配置 ----------
    ChannelSpec { name: "app:info", fields: &[] },
    ChannelSpec { name: "app:config", fields: &[] },
    ChannelSpec {
        name: "app:setConfig",
        fields: &[
            n("concurrency"),
            s("cacheDir").max(PATH_MAX),
            // 检出目录的默认父目录（设置页配置；检出时用它 + 仓库名）
            s("defaultCheckoutParent").max(PATH_MAX),
        ],
    },
    ChannelSpec { name: "app:pickDir", fields: &[s("title").max(200)] },
    // ---------- 登录（token 只留在 Rust 侧，绝不进渲染层） ----------
    ChannelSpec {
        name: "auth:login",
        fields: &[
            s("server").req().max(512),
            s("username").req().max(200),
            s("password").req().max(1024),
        ],
    },
    ChannelSpec { name: "auth:logout", fields: &[] },
    ChannelSpec { name: "auth:state", fields: &[] },
    ChannelSpec { name: "auth:forgetServer", fields: &[s("server").req().max(512)] },
    // ---------- 仓库 ----------
    ChannelSpec { name: "repos:list", fields: &[] },
    // ---------- 工作副本 ----------
    ChannelSpec { name: "wc:recent", fields: &[] },
    ChannelSpec {
        name: "wc:checkout",
        fields: &[
            s("repo").req().max(200),
            s("dir").req().max(PATH_MAX),
            arr("sparse").max(PATH_MAX),
        ],
    },
    ChannelSpec { name: "wc:open", fields: &[s("dir").req().max(PATH_MAX)] },
    ChannelSpec { name: "wc:close", fields: &[] },
    ChannelSpec { name: "wc:status", fields: &[b("forceHash")] },
    ChannelSpec { name: "wc:add", fields: &[arr("paths").req().max(PATH_MAX)] },
    ChannelSpec { name: "wc:remove", fields: &[arr("paths").req().max(PATH_MAX)] },
    ChannelSpec { name: "wc:revert", fields: &[arr("paths").req().max(PATH_MAX)] },
    ChannelSpec { name: "wc:mkdir", fields: &[s("path").req().max(PATH_MAX)] },
    ChannelSpec { name: "wc:ignoreRules", fields: &[] },
    ChannelSpec {
        name: "wc:setIgnoreRules",
        fields: &[s("content").req().max(65536)],
    },
    // v0.4.17：**提交说明可空**（服务端 message 本就是可选），所以不再是 required
    ChannelSpec {
        name: "wc:commit",
        fields: &[s("message").max(10000), arr("paths").max(PATH_MAX)],
    },
    ChannelSpec { name: "wc:update", fields: &[] },
    // ---------- 冲突 ----------
    ChannelSpec { name: "wc:conflicts", fields: &[] },
    ChannelSpec { name: "wc:conflictSides", fields: &[s("path").req().max(PATH_MAX)] },
    ChannelSpec {
        name: "wc:resolveConflict",
        fields: &[
            s("path").req().max(PATH_MAX),
            s("choice")
                .req()
                .one_of(&["mine", "theirs", "merged"]),
            // 合并结果（choice === "merged" 时必填，由处理器再校验）
            s("content").max(MERGE_MAX),
        ],
    },
    // ---------- 锁 ----------
    ChannelSpec { name: "wc:locks", fields: &[s("path").max(PATH_MAX)] },
    ChannelSpec {
        name: "wc:lock",
        fields: &[s("path").req().max(PATH_MAX), s("comment").max(500)],
    },
    ChannelSpec {
        name: "wc:unlock",
        fields: &[
            s("path").req().max(PATH_MAX),
            b("breakLock"),
            s("reason").max(500),
        ],
    },
    // ---------- 历史修订 ----------
    ChannelSpec {
        name: "repo:log",
        fields: &[
            // **显式给仓库**：仓库页在没有打开工作副本时也要能看历史/目录树
            s("repo").req().max(PATH_MAX),
            n("limit"),
            n("offset"),
            s("prefix").max(PATH_MAX),
        ],
    },
    ChannelSpec {
        name: "repo:treeAt",
        fields: &[
            s("repo").req().max(PATH_MAX),
            n("rev"), // 0 / 缺省 = HEAD
            s("path").max(PATH_MAX),
            n("depth"),
        ],
    },
    ChannelSpec {
        name: "repo:downloadRevision",
        fields: &[
            s("repo").req().max(PATH_MAX),
            s("path").req().max(PATH_MAX),
            n("rev"),
            s("targetDir").req().max(PATH_MAX),
        ],
    },
    // ---------- 配置与缓存 ----------
    ChannelSpec { name: "config:cacheStats", fields: &[] },
    ChannelSpec { name: "config:clearCache", fields: &[b("keepTmp")] },
    // ---------- 系统 ----------
    ChannelSpec { name: "shell:revealPath", fields: &[s("path").req().max(PATH_MAX)] },
];

/// Rust → 渲染层的事件通道（单向推送，渲染层只能监听）。
pub const EVENT_CHANNELS: &[&str] = &["wc:progress", "wc:state", "wc:changed"];

pub fn is_channel(v: &str) -> bool {
    CHANNEL_SPECS.iter().any(|c| c.name == v)
}

pub fn is_event_channel(v: &str) -> bool {
    EVENT_CHANNELS.contains(&v)
}

pub fn channel_names() -> Vec<&'static str> {
    CHANNEL_SPECS.iter().map(|c| c.name).collect()
}

fn spec_of(channel: &str) -> Option<&'static [Field]> {
    CHANNEL_SPECS
        .iter()
        .find(|c| c.name == channel)
        .map(|c| c.fields)
}

/// 字符串"长度"按 **UTF-16 码元** 计（与 TS 的 `String.length` 一致）。
///
/// 用 `chars().count()` 会在 emoji/罕用字上得出更小的数，等于静默放宽了上限 ——
/// 上限本身就是防超大 payload 的，不能因为换了语言就变松。
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// 按 schema 校验 payload；返回规范化后的对象（缺省值已填充）。
///
/// 错误文案必须与 `shared/channels.ts::validatePayload` 一致。
pub fn validate_payload(fields: &[Field], input: Option<&Value>) -> Result<Map<String, Value>, String> {
    let empty = Value::Object(Map::new());
    let src = match input {
        None | Some(Value::Null) => &empty,
        Some(v) => v,
    };
    let rec = match src {
        Value::Object(m) => m,
        _ => return Err("参数必须是对象".to_string()),
    };

    for key in rec.keys() {
        if !fields.iter().any(|f| f.name == key) {
            return Err(format!("未知参数：{key}"));
        }
    }

    let mut out = Map::new();
    for f in fields {
        let raw = rec.get(f.name);
        let Some(raw) = raw else {
            if f.required {
                return Err(format!("缺少参数：{}", f.name));
            }
            continue;
        };

        match f.ty {
            FieldType::Str => {
                let Some(v) = raw.as_str() else {
                    return Err(format!("{} 必须是字符串", f.name));
                };
                if let Some(max) = f.max_len {
                    if utf16_len(v) > max {
                        return Err(format!("{} 超长（上限 {}）", f.name, max));
                    }
                }
                if let Some(values) = f.values {
                    if !values.contains(&v) {
                        return Err(format!("{} 取值非法：{}", f.name, v));
                    }
                }
                out.insert(f.name.to_string(), Value::String(v.to_string()));
            }
            FieldType::Num => {
                let Some(v) = raw.as_f64() else {
                    return Err(format!("{} 必须是数字", f.name));
                };
                if !v.is_finite() {
                    return Err(format!("{} 必须是数字", f.name));
                }
                out.insert(f.name.to_string(), raw.clone());
            }
            FieldType::Bool => {
                let Some(v) = raw.as_bool() else {
                    return Err(format!("{} 必须是布尔值", f.name));
                };
                out.insert(f.name.to_string(), Value::Bool(v));
            }
            FieldType::StrArray => {
                let Some(list) = raw.as_array() else {
                    return Err(format!("{} 必须是字符串数组", f.name));
                };
                let mut items = Vec::with_capacity(list.len());
                for item in list {
                    let Some(item) = item.as_str() else {
                        return Err(format!("{} 的元素必须是字符串", f.name));
                    };
                    if let Some(max) = f.max_len {
                        if utf16_len(item) > max {
                            return Err(format!("{} 的元素超长（上限 {}）", f.name, max));
                        }
                    }
                    items.push(Value::String(item.to_string()));
                }
                out.insert(f.name.to_string(), Value::Array(items));
            }
        }
    }

    Ok(out)
}

/// 按通道名校验 payload。
pub fn validate_channel_payload(
    channel: &str,
    payload: Option<&Value>,
) -> Result<Map<String, Value>, String> {
    let Some(fields) = spec_of(channel) else {
        return Err(format!("未知通道：{channel}"));
    };
    validate_payload(fields, payload)
}

// ---------- 供 CLI / 门禁导出的表快照 ----------

#[derive(Serialize)]
struct FieldDump {
    name: &'static str,
    #[serde(rename = "type")]
    ty: &'static str,
    required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_len: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    values: Option<&'static [&'static str]>,
}

#[derive(Serialize)]
struct ChannelDump {
    name: &'static str,
    fields: Vec<FieldDump>,
}

fn ty_name(t: FieldType) -> &'static str {
    match t {
        FieldType::Str => "string",
        FieldType::Num => "number",
        FieldType::Bool => "boolean",
        FieldType::StrArray => "string[]",
    }
}

/// 导出整张表（`b-artifact channels --json`）—— 给 `scripts/gen_channels.mjs --check`
/// 与 TS 侧的通道联合类型做防漂移比对，扮演 TS 版 `dto-parity.test.ts` 的角色。
pub fn dump_json() -> Value {
    let channels: Vec<ChannelDump> = CHANNEL_SPECS
        .iter()
        .map(|c| ChannelDump {
            name: c.name,
            fields: c
                .fields
                .iter()
                .map(|f| FieldDump {
                    name: f.name,
                    ty: ty_name(f.ty),
                    required: f.required,
                    max_len: f.max_len,
                    values: f.values,
                })
                .collect(),
        })
        .collect();
    serde_json::json!({
        "channels": channels,
        "events": EVENT_CHANNELS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_channel_is_rejected() {
        let e = validate_channel_payload("nope:nope", None).err().expect("应当失败");
        assert_eq!(e, "未知通道：nope:nope");
    }

    #[test]
    fn unknown_param_is_rejected() {
        let e = validate_channel_payload("wc:open", Some(&json!({ "dir": "/tmp/a", "x": 1 })))
            .err().expect("应当失败");
        assert_eq!(e, "未知参数：x");
    }

    #[test]
    fn missing_required_is_rejected() {
        let e = validate_channel_payload("wc:open", Some(&json!({}))).err().expect("应当失败");
        assert_eq!(e, "缺少参数：dir");
    }

    #[test]
    fn wrong_type_messages_match_ts() {
        assert_eq!(
            validate_payload(&[s("dir")], Some(&json!({ "dir": 1 }))).err().expect("应当失败"),
            "dir 必须是字符串"
        );
        assert_eq!(
            validate_payload(&[n("limit")], Some(&json!({ "limit": "1" }))).err().expect("应当失败"),
            "limit 必须是数字"
        );
        assert_eq!(
            validate_payload(&[b("forceHash")], Some(&json!({ "forceHash": "true" }))).err().expect("应当失败"),
            "forceHash 必须是布尔值"
        );
        assert_eq!(
            validate_payload(&[arr("paths")], Some(&json!({ "paths": ["a", 1] }))).err().expect("应当失败"),
            "paths 的元素必须是字符串"
        );
        assert_eq!(
            validate_payload(&[arr("paths")], Some(&json!({ "paths": "a" }))).err().expect("应当失败"),
            "paths 必须是字符串数组"
        );
    }

    #[test]
    fn non_object_payload_is_rejected() {
        assert_eq!(
            validate_payload(&[], Some(&json!([1, 2]))).err().expect("应当失败"),
            "参数必须是对象"
        );
    }

    #[test]
    fn max_len_counts_utf16_units() {
        // "…" 是 1 个码点 / 1 个 UTF-16 码元；emoji 是 1 个码点 / 2 个码元。
        // 上限 2 时：两个汉字通过，一个 emoji + 一个汉字（3 码元）不通过。
        assert!(validate_payload(&[s("p").max(2)], Some(&json!({ "p": "中文" }))).is_ok());
        let e = validate_payload(&[s("p").max(2)], Some(&json!({ "p": "🙂中" }))).err().expect("应当失败");
        assert_eq!(e, "p 超长（上限 2）");
    }

    #[test]
    fn enum_values_are_enforced() {
        let e = validate_channel_payload(
            "wc:resolveConflict",
            Some(&json!({ "path": "a.psd", "choice": "ours" })),
        )
        .err().expect("应当失败");
        assert_eq!(e, "choice 取值非法：ours");
    }

    #[test]
    fn absent_optional_fields_are_not_filled() {
        // TS 版没写 default 的项就是"缺省即缺席"，处理器靠 `typeof === 'string'` 判定，
        // Rust 侧若补 null 会静默改变语义。
        let v = validate_channel_payload("wc:locks", Some(&json!({}))).unwrap();
        assert!(!v.contains_key("path"));
        let v = validate_channel_payload("wc:lock", Some(&json!({ "path": "a" }))).unwrap();
        assert!(!v.contains_key("comment"));
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn null_and_undefined_payloads_are_empty_objects() {
        assert!(validate_channel_payload("app:config", None).unwrap().is_empty());
        assert!(validate_channel_payload("app:config", Some(&Value::Null))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn every_channel_name_is_unique() {
        let mut names = channel_names();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "通道名重复");
        assert_eq!(before, 34, "通道数量应为 34（与 shared/channels.ts 对齐）");
    }

    #[test]
    fn event_channels_are_not_invoke_channels() {
        for e in EVENT_CHANNELS {
            assert!(!is_channel(e), "事件通道 {e} 不应出现在 invoke 白名单里");
        }
    }
}
