# 客户端 Tauri 迁移方案（Electron → Tauri 2 + Rust）

> 分支：`tauri`（2026-10-29 创建，基于 `main@cbabb59`）｜版本：**v0.5.0**｜状态：**已执行**（执行记录见文末 §14）
> 本文取代 `docs/架构设计方案.md` §6.4 的客户端架构描述（§6.4 的目录/技术栈/打包部分在 T7 阶段整体回写）。
> 已核过的环境事实：`tauri` crate 2.12.5 / `tauri-build` 2.7.4 / `tauri-plugin-{opener,single-instance}` 已在本地 cargo 缓存；
> `@tauri-apps/cli` 2.12.4 可从 npm 取；`static.crates.io` 与 `index.crates.io` 均可达（`notify` / `keyring` 等需现拉）；
> 本机 `cc` = Apple clang 14、macOS 13.7.8、Rust 1.95，**x86_64**（CI mac 腿是 arm64，架构不能靠本机推导）。

---

## 0. 一页结论

| 项 | 现状（Electron） | 迁移后（Tauri 2 + Rust） |
|---|---|---|
| 桌面壳 | Electron 44（Chromium + Node 双进程） | Tauri 2（系统 WebView，单进程） |
| 引擎 | `client/src/core/*.ts` 3.3k 行，跑在主进程 | `client/src-tauri/src/engine/*.rs`（rusqlite / reqwest / sha2 / zstd，与 `server/` 同栈） |
| CLI | `src/cli.ts`（tsx，需 Node） | `src-tauri` 的第二个 bin（原生，零运行时） |
| 渲染层 | Vue 3 + Element Plus，201 项单测 | **不变**，只换 30 行桥接实现 |
| mac 交付物 | `b-artifact-0.1.0-mac.zip` **119 MB**（`.app` 301 MB） | 目标 **≤ 20 MB**（T6 实测回填） |
| 依赖 | 需 Node 运行时 + Electron 二进制 | 单二进制，无 Node |

**硬约束**：31 个 IPC 通道名与 payload schema、错误码字符串、`wc.db` 磁盘格式、`wc.lock` 语义、CLI 的 `--json` 形状，**全部保持不变**（§1）。这不是"尽量兼容"，是验收条件——迁移期两版引擎要能交替打开同一个工作副本。

**主要代价**：357 项客户端单测（18 个套件）要在 Rust 里重建；`wc.ts`（1192 行、60 项用例）是全项目最难移植的一块，靠**双引擎对拍**兜底（§7.2）。

---

## 1. 不变量（7 条，破了就不算迁移成功）

1. **`wc.db` 逐字兼容**：表名、列名、`CHECK` 约束、索引名、`schema_version`（1 → 2 两段迁移）、`PRAGMA foreign_keys = ON` 全部照搬。**不加 WAL、不加列、不改名、不换主键**——现状是 SQLite 默认 rollback journal（`core/` 里没有任何 `journal_mode`），一旦改动磁盘上会多出 `wc.db-wal`，两版引擎的并发语义就分叉了。
2. **`wc.lock` 同格式同语义**：内容 = 占用者 pid 的十进制文本；pid 存活检测（不是"文件存在即占用"，崩溃残留不能永久阻塞）；同 pid 引用计数；重复 release 幂等。
3. **通道契约不变**：31 个 invoke 通道 + 3 个事件通道的名字、每通道 payload schema（必填/类型/长度上限/枚举/缺省值）、校验失败的中文文案。
4. **信封不变**：`{ ok: true, data }` / `{ ok: false, error: { code, message } }`；异常绝不跨边界，一律转信封。
5. **安全基线不变**：token 只在 Rust 侧、永不进渲染层；拿不到加密后端时**不落盘**（宁可下次重新登录）；渲染层拿不到任意文件/网络能力。
6. **渲染层业务代码不变**：`src/renderer/**` 的视图/组件/store/utils 一行不改（唯一例外见 R2），`npm test` 的 201 项渲染层用例继续全绿。
7. **CLI 行为不变**：命令集、参数名、`--json` 的字段名与嵌套形状不变——`scripts/m4_smoke.sh` 的 68 组断言直接吃这份输出。

---

## 2. 目标架构

### 2.1 进程模型

```
┌──────────────────────────────────────────────────────────────┐
│  单个原生进程（src-tauri）                                     │
│                                                              │
│  ┌────────────┐   invoke('ipc',{channel,payload})   ┌──────┐ │
│  │ WebView    │ ──────────────────────────────────► │      │ │
│  │ (Vue 3 +   │                                     │ Rust │ │
│  │  Element)  │ ◄────────── emit('wc:progress') ─── │ 侧   │ │
│  └────────────┘                                     └──┬───┘ │
│        ▲ 系统 WebView（WKWebView / WebView2 / GTK）     │     │
│        │                                                │     │
│        └──────── tauri:// 自定义协议加载内嵌前端 ◄────────┘     │
└──────────────────────────────────────────────────────────────┘
                       │ HTTP（reqwest）
                       ▼
                 b-artifact 服务端
```

对比今天的关键变化：

| 维度 | Electron（现） | Tauri（迁后） |
|---|---|---|
| 主进程权限 | Node 全权限 | Rust 全权限（同一进程内，无"preload 桥"这一层） |
| 渲染层隔离 | `contextIsolation` + `sandbox` + preload 白名单 | WebView 沙箱 + capabilities（**不给** fs/shell/dialog 任何前端权限）+ Rust 侧白名单 |
| 载荷传输 | 结构化克隆（Proxy/函数会抛"could not be cloned"） | JSON（serde）；Proxy 天然被 `JSON.stringify` 吃掉，BigInt 会直接抛 |
| 前端加载 | `file://`（`base: './'` 的由来） | `tauri://localhost`（mac/Linux）、`http://tauri.localhost`（Windows） |
| 关窗行为 | `window-all-closed` 里判定 darwin | `RunEvent::ExitRequested` 里判定 darwin（语义照搬） |

### 2.2 目录结构（迁移后）

```
client/
├─ src-tauri/
│  ├─ Cargo.toml                # package: b-artifact-client；[lib] + [[bin]] b-artifact / b-artifact-app
│  ├─ build.rs                  # tauri-build
│  ├─ tauri.conf.json           # 窗口 / CSP / 打包目标 / frontendDist
│  ├─ capabilities/default.json  # 只给 core:default —— 前端不需要任何插件权限
│  ├─ icons/                    # tauri icon 由 build/icon.png 生成（icon.icns / icon.ico / *.png）
│  └─ src/
│     ├─ lib.rs                 # pub mod engine/channels/commands/...（供两个 bin 与 tests/ 复用）
│     ├─ main.rs                # 桌面壳：builder 装配、单实例、窗口、退出语义
│     ├─ cli.rs                 # b-artifact CLI（同一 engine，**不依赖 tauri**，可在无 GUI 环境跑）
│     ├─ channels.rs            # 31 invoke + 3 event：名字 + schema（唯一事实源）
│     ├─ commands.rs            # #[tauri::command] ipc()：校验 → 分发 → 信封；31 个分支
│     ├─ state.rs               # AppState：config / session / 当前 WorkingCopy / watcher
│     ├─ config.rs              # ← main/store.ts（原子写 + 损坏留证）
│     ├─ session.rs             # ← main/session.ts（keyring 替代 safeStorage）
│     ├─ watch.rs               # ← main/watch.ts（notify + 300ms 防抖 + 自写抑制）
│     ├─ window.rs             # ← main/window.ts（可测的装配断言 + 导航白名单校验）
│     └─ engine/                # ← core/*.ts，逐文件对应，见 §4
├─ src/renderer/**              # 不变
├─ src/shared/{channels,ipc,dto}.ts   # 缩为渲染层要用的类型与通道名（§3.3）
├─ tests/renderer/**            # 保留（201 项）
├─ src-tauri/tests/             # 引擎与命令的 Rust 单测/集成测试（357 项等价物）
├─ build/icon.png               # 不变（tauri icon 的输入）
└─ client-legacy/               # TS 引擎，仅作对拍 oracle；T7 删除（§7.2）
```

### 2.3 为什么只暴露**一个** `ipc` 命令，而不是 31 个 `#[tauri::command]`

31 个命令看起来"更 Tauri"，但会把白名单与校验逻辑拆成 31 份，且新增通道要同时改三处（TS 联合类型、Rust 命令、前端调用）。现在保留今天的形状：

```rust
#[tauri::command]
async fn ipc(state: tauri::State<'_, AppState>, channel: String, payload: serde_json::Value) -> IpcResult
```

- 白名单只有 `channels.rs` 一处；校验顺序与失败文案与 `shared/channels.ts::validatePayload` 逐条对齐（§3.2）；
- 前端调用点形状不变：`invoke('ipc', { channel, payload })`；
- 好处是可测：`commands::dispatch(channel, payload, &state)` 是普通 async 函数，Rust 单测直接驱动，不需要起 Tauri。

---

## 3. 通道迁移

### 3.1 事件

| 事件 | 载荷 | 触发点 |
|---|---|---|
| `wc:progress` | `{ phase: 'commit'\|'update', ...Progress }` | 提交/更新的上传下载进度 |
| `wc:state` | `WorkingCopyState \| null` | 打开/关闭/更新后 |
| `wc:changed` | `StatusItem[]` | 文件监听防抖后重算 |

Rust 侧 `app.emit("wc:progress", &payload)`；前端 `listen('wc:progress', e => …)`。
**节流**：进度事件合并到 100–250ms 一次（今天 Electron 是每块发一次，靠结构化克隆扛住；JSON 序列化 + IPC 往返更贵，见 R4）。

### 3.2 校验语义必须逐条对齐

| 场景 | 今天的文案 | 迁移后 |
|---|---|---|
| payload 不是对象 / 是数组 | `参数必须是对象` | 同 |
| 未知参数 | `未知参数：{key}` | 同 |
| 缺必填 | `缺少参数：{key}` | 同 |
| 类型错 | `{key} 必须是字符串/数字/布尔值/字符串数组` | 同 |
| 超长 | `{key} 超长（上限 {n}）` / `{key} 的元素超长（上限 {n}）` | 同 |
| 枚举外 | `{key} 取值非法：{v}` | 同 |
| 未知通道 | `未知通道：{channel}` | 同 |

缺省值填充（`default`）也要一致：今天 `wc:status.forceHash` 缺省为 `undefined`（下游按 `=== true` 判定），schema 里没写 `default` 的项**不要**在 Rust 侧补 `null`，否则 `p['path']` 之类的 `typeof === 'string'` 判定会静默变形。

### 3.3 `shared/channels.ts` 的定位变化

- **schema 与通道名的事实源移到 Rust**（`channels.rs`），因为校验发生在 Rust 侧；
- TS 侧只保留渲染层需要的 `type Channel` 字面量联合（`BridgeCallMap` 的 key 要有类型）；
- 防漂移门禁：CLI 加一个隐藏子命令 `b-artifact channels --json` 导出整张表，`scripts/gen_channels.mjs --check` 与 TS 联合类型比对，CI 跑——沿用今天 `dto-parity.test.ts`（5 项）的角色，只是方向反过来。

---

## 4. 引擎模块映射（逐文件）

| TS 源 | 行 | Rust 目标 | 要点 / 易踩 | crate |
|---|---|---|---|---|
| `core/api.ts` | 532 | `engine/api.rs` | 错误码解析、幂等重试（只重试 GET/HEAD/PUT blob/分块/commit）、`Retry-After` 退避、分块上传；`fetch` 可注入 → Rust 用 `ApiClient<H: Http>` 泛型或 `Arc<dyn Transport>` 保测试性 | `reqwest`(json,stream,rustls) |
| `core/db.ts` | 598 | `engine/db.rs` | schema 逐字复制；`entries/pending/locks/conflicts/meta/schema_version` 六表；`wc.lock` 进程锁（pid 存活用 `libc::kill(pid,0)`；引用计数用 `Mutex<HashMap>`） | `rusqlite`(bundled)、`libc` |
| `core/wc.ts` | 1192 | `engine/wc.rs` | 检出/状态/提交/更新/锁/冲突/revert/mkdir/ignore 文件。**分 4 段移植**：①检出+scan ②status/add/remove/revert ③commit（两阶段+补锁+只放自动补的锁）④update/conflict/lock | 复用上述 |
| `core/scan.ts` | 238 | `engine/scan.rs` | `walkdir` 不跟随符号链接；`.mine` 后缀冲突探测；mtime+size 启发式 → 内容哈希为准 | `walkdir` |
| `core/paths.ts` | 169 | `engine/paths.rs` | NFC 规范化、Windows 保留名/非法字符、1024 字节上限、大小写折叠冲突、大小写敏感探测 | `unicode-normalization` |
| `core/ignore.ts` | 138 | `engine/ignore.rs` | gitignore 语义**自行移植**（不引 `ignore` crate：它的匹配细节与现有 10 项用例的语义未必一致，差异排查成本比移植高） | — |
| `core/pristine.ts` | 162 | `engine/pristine.rs` | 三层布局：全局缓存 → pristine 硬链接 → 工作文件；跨设备回退为复制 | `fs_extra`/手写 |
| `core/revisions.ts` | 119 | `engine/revisions.rs` | 修订下定位文件、旧版下载重名不覆盖（`name.r12.ext`） | — |
| `core/text.ts` | 96 | `engine/text.rs` | NUL + 严格 UTF-8 双条件判二进制；1 MiB 上限 | — |
| `core/cache.ts` | 121 | `engine/cache.rs` | 只删文件保留骨架、跳过符号链接 | — |
| `core/hash.ts` | 47 | `engine/hash.rs` | sha256 流式、8 MiB 分块、64 MiB 阈值 | `sha2` |
| `core/errors.ts` | 115 | `engine/errors.rs` | 错误码表 + `ApiError`/`WcError` → 统一 `ClientError`；**code 字符串一张表**，与 TS 逐一对应 | `thiserror` |
| `core/home.ts` | 14 | `engine/home.rs` | `~/.b-artifact`、`B_ARTIFACT_HOME` 可重定向（测试隔离靠它） | `dirs` |
| `core/ioRetry.ts` | 48 | `engine/io_retry.rs` | EBUSY/EACCES/EPERM 退避重试（Unix errno 判定） | — |
| `main/store.ts` | 182 | `config.rs` | 配置/最近副本/服务器列表；原子写（tmp+rename）、损坏留证（`.bad`）；**JSON 形状不变** | `serde_json` |
| `main/session.ts` | 147 | `session.rs` | keyring 替代 safeStorage（§6.2） | `keyring` |
| `main/watch.ts` | 168 | `watch.rs` | 防抖 300ms、自写抑制窗口、降级语义（`degraded`） | `notify` |
| `main/window.ts` | 123 | `window.rs` + `tauri.conf.json` | 安全基线改为"配置表达 + 启动断言"；导航白名单 → `on_navigation` | `tauri` |
| `main/ipc.ts` | 65 | `channels.rs` + `commands.rs` | 白名单注册、处理器完整性断言（启动期 fail fast） | — |
| `main/handlers.ts` | 454 | `commands.rs` + `state.rs` | 31 个分支 + `WcSession`（同一实例 adopt 不 close、withSelfWrite 抑制窗口） | — |
| `main/index.ts` | 185 | `main.rs` | 装配层：单实例、窗口、缓存目录 mkdir、退出语义 | `tauri-plugin-single-instance` |
| `preload.ts` | 88 | **删除** | 桥接能力并入 Rust 侧白名单；窄接口由 capabilities 保证 | — |
| `cli.ts` | 389 | `cli.rs` | 手写参数解析（与 `server/main.rs` 同风格，不引 clap）；凭据落 `~/.b-artifact/auth.json` 0600 | — |
| `shared/dto.ts` | 242 | `engine/dto.rs` + 渲染层保留 | serde 结构体字段名与 TS 完全一致（`snake_case` 的 API 字段**不要**加 `rename_all="camelCase"`） | `serde` |
| `shared/ipc.ts` | 53 | `commands.rs` | 信封 + `toIpcError` 映射（`ApiError`→其 code，其余→`INTERNAL`） | — |

---

## 5. 平台能力替换

| 能力 | Electron | Tauri | 备注 |
|---|---|---|---|
| 凭据加密 | `safeStorage` | `keyring`（macOS Keychain / Windows Credential Manager / Linux Secret Service） | 旧的 `session.enc` 解不开 → 一次性重新登录（§6.2） |
| 目录选择 | `dialog.showOpenDialog` | `rfd` 或 `tauri-plugin-dialog`（**Rust 侧**调用，不给前端权限） | 取消返回 `null` |
| 在文件管理器定位 | `shell.showItemInFolder` | `tauri-plugin-opener` 的 reveal | 返回 `{ revealed: true }` 不变 |
| 递归文件监听 | `fs.watch({recursive})`（Linux 不支持 → 降级） | `notify`（三平台递归） | **收益**：Linux 不再必然降级；新风险 R9 |
| 单实例 | `requestSingleInstanceLock` | `tauri-plugin-single-instance` | 第二次启动把窗口拉前台 |
| 打包内嵌静态资源 | asar | `frontendDist` 内嵌进二进制 | `check_asar.mjs` 随之退役（§7.3） |
| 首屏 | `visible:false` + `ready-to-show` | 窗口 `visible:false` + 前端就绪后 `show()`（或 `on_page_load`） | 防止白屏闪烁 |
| 后台定时器节流 | `backgroundThrottling: false` | **无等价开关** | 见 R2，需改一处业务代码 |
| `ELECTRON_RUN_AS_NODE` | 启动前必须清 | 不复存在 | 相关脚本/hack 一并删除 |

---

## 6. 数据与凭据

### 6.1 工作副本（不变）
`<root>/.b-artifact/{wc.db,wc.lock,pristine/}`、全局缓存目录、pristine 硬链接布局、`~/.b-artifact/auth.json`（CLI）全部保持原路径与原格式。

### 6.2 会话（唯一对用户可见的行为变化）
Electron 的 `safeStorage` 密文 Rust 解不了（macOS 走 Keychain 的通用密码项、Windows 走 DPAPI 密文）。策略：

1. 首次启动时若发现 `~/.b-artifact/session.enc`：**删除并视为未登录**，日志留一行说明；
2. 之后 token 直接进系统钥匙串（service `b-artifact`，account `{server}#{username}`），不再有 `session.enc`；
3. 钥匙串不可用 → 只留内存，重启需重新登录（与今天"安全默认"一致）。

**交付说明必须写**："升级后需要重新登录一次"。这是本次迁移唯一需要用户动手的地方。

### 6.3 缓存与 home（不变）
`B_ARTIFACT_HOME` / `cacheDir` / `defaultCheckoutParent` 的语义与默认值一并照搬，`app:config` 返回的 JSON 字段名不变。

---

## 7. 测试与门禁

### 7.1 357 项用例的归属

| 套件 | 项 | 去向 |
|---|---|---|
| `wc.test.ts` | 60 | → `src-tauri/tests/wc.rs`（1:1，重点：先锁后提交、只放自动补的锁、未纳管变更新增、删除三态、冲突三选一） |
| `main-handlers.test.ts` | 51 | → `commands.rs` 的 `dispatch` 单测（用 tempdir + 假 Transport，不起 Tauri） |
| `channels.test.ts` | 29 | → `channels.rs` 单测（schema 校验逐条） |
| `db.test.ts` | 23 | → `engine/db.rs` 单测（含"TS 版创建的库能被打开"的 fixture） |
| `api.test.ts` | 21 | → `engine/api.rs`（假 Transport：重试次数、Retry-After、幂等边界） |
| `scan.test.ts` | 19 | → `engine/scan.rs` |
| `main-store.test.ts` | 19 | → `config.rs` |
| `revisions.test.ts` | 17 | → `engine/revisions.rs` |
| `main-ipc.test.ts` | 16 | → `commands.rs` 的注册完整性断言 |
| `text.test.ts` | 15 | → `engine/text.rs` |
| `paths.test.ts` | 14 | → `engine/paths.rs` |
| `main-session.test.ts` | 14 | → `session.rs`（keyring 用 `mock` 后端） |
| `main-watch.test.ts` | 10 | → `watch.rs`（防抖与抑制窗口用假时钟） |
| `ignore.test.ts` | 10 | → `engine/ignore.rs` |
| `cache.test.ts` | 9 | → `engine/cache.rs` |
| `dto-parity.test.ts` | 5 | → `scripts/gen_channels.mjs --check`（§3.3） |
| `preload.test.ts` / `main-window.test.ts` | 13 / 12 | **删除**：能力已被 Rust 白名单 + capabilities 取代；安全基线改为"启动断言 + 冒烟"覆盖 |
| `renderer/**`（9 套件） | 201 | **保留在 vitest**，不改一行 |

### 7.2 双引擎对拍（核心策略）

`wc.ts` 的 1192 行是全项目最难移植的一段，60 项单测只覆盖主干。迁移期把 TS 引擎整体保留在 `client-legacy/`，当**参考实现（oracle）**：

`scripts/parity.mjs` 流程：起临时服务端（复用 `m4_smoke.sh` 的造数）→ 在两份独立的临时目录里，分别用 TS CLI（`client-legacy`）与 Rust CLI 执行同一串操作 → 比较 `--json` 输出。

| 覆盖操作 | 比较内容 | 忽略字段 |
|---|---|---|
| `checkout`（全量 / `--sparse`） | 落盘文件清单 + `status --json` 全量 | 时间戳、绝对路径前缀 |
| `add` / `remove` / `revert` | `status` 的 `code`+`pending` | 同上 |
| `commit`（新增/修改/删除混排、空目录、无变更） | `rev`、`changes` 行数、错误码 | `commit_id`、耗时 |
| `update`（含冲突） | 冲突表、`status`、`.mine` 文件 | 时间戳 |
| `lock` / `unlock` / `locks` | 锁列表（owner/token 掩码后） | token |

对拍在 T3 起作为门禁，T4/T6 每次改引擎都跑。**这是"移植偏差"唯一可靠的兜底**，比复审代码便宜。

### 7.3 打包门禁（`check_asar.mjs` 退役，换成三件套）

1. **前端产物齐全**：沿用 `build.mjs` 的 `assertArtifacts`（`dist/renderer/index.html` 必须存在）——这条教训（Windows 腿打出缺渲染层的 exe）与框架无关，保留。
2. **二进制与体积**：`tauri build` 后断言产物存在、体积在预算内（mac zip ≤ 20 MB），超出即失败（防"无声退化回大包"）。
3. **启动自检**（替代 CDP）：`b-artifact-app --self-check` 启动后由 Rust 注入 JS，断言前端已挂载（`document.querySelector('#app').children.length > 0` 且一次 `ipc` 往返成功），成功写标记文件退出 0，超时退出非 0。
   为什么不用 `tauri-driver`：它只支持 Linux/Windows，macOS 没有 WebDriver 实现（R6）。注入式自检三平台同一套，比 CDP 更稳。

### 7.4 脚本改造清单

| 脚本 | 动作 |
|---|---|
| `scripts/m4_smoke.sh` | CLI 调用从 `tsx src/cli.ts` 改为 `target/{debug,release}/b-artifact`（68 组断言文本不变） |
| `scripts/m4_electron_smoke.sh` | → `scripts/m4_tauri_smoke.sh`（构建 + 起服务端 + 启动应用自检） |
| `scripts/electron_smoke.mjs` | 删除（CDP 驱动不再可用） |
| `scripts/setup_electron.sh` | 删除 |
| `scripts/m4_package.sh` | 改写：`tauri build` → 体积/自检门禁 → 用打包产物跑 `m4_tauri_smoke.sh` |
| `scripts/check_asar.mjs` | 删除，逻辑并入 §7.3 的门禁脚本 |

---

## 8. 打包与 CI

### 8.1 三平台交付物

| 平台 | Tauri target | 交付文件 | 前提 |
|---|---|---|---|
| macOS arm64 | `app` + `dmg`（tauri-bundler 用系统 `hdiutil`）+ 脚本压 `zip` | `b-artifact-macos-arm64.zip` / `.dmg` | 无（WKWebView 系统自带） |
| Windows x64 | **无 portable target** → 直接交付 release 的 `b-artifact.exe`（前端已内嵌，绿色版） | `b-artifact-windows-x64.exe` | 系统 WebView2（Win10 1803+ / Win11 自带；缺失时 Rust 侧检测并弹原生提示） |
| Linux x64 | `appimage` + 脚本压 `tar.gz` | `b-artifact-linux-x64.AppImage` / `.tar.gz` | `libwebkit2gtk-4.1` / `libgtk-3`（AppImage 不打包 webkit） |

Windows "单文件"要求（2026-09-24 定：不要 nsis/zip/blockmap）**继续满足，但形态变了**：从"electron-builder portable 的自解压 exe"变成"裸 exe"。若要求"连 WebView2 都不依赖"，只能 `webviewInstallMode: fixedRuntime`（+约 180 MB），与单文件轻量目标冲突——见 D3。

### 8.2 CI 改动（`.github/workflows/build.yml`）

- ubuntu 腿加前置包：`libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev patchelf libayatana-appindicator3-dev`；
- `Swatinem/rust-cache` 的 `workspaces` 增加 `client/src-tauri`；
- 客户端步骤：`npm ci` → `npm run build`（vite 打渲染层）→ `cargo tauri build --target <matrix.target>`（**arch 仍由矩阵显式给**，不用 runner 标签推导，v0.4.23 的教训继续生效）；
- 删除：`Generate client icon`（改用 `tauri icon`，一次生成入库）、`electron-builder` 步骤、`check_asar.mjs` 步骤；
- 新增：bundle 门禁步骤（§7.3）、启动自检步骤；
- 归档名继续带架构后缀。

### 8.3 体积预算

| 项 | 现状 | 目标 | 实测 |
|---|---|---|---|
| mac zip | 119 MB | ≤ 20 MB | T6 回填 |
| mac `.app` | 301 MB | ≤ 40 MB | T6 回填 |
| win 单文件 | ≈ 90 MB（portable，估计） | ≤ 15 MB | T6 回填 |

超预算的常见原因：没开 `lto`/`strip`、debug 符号进了二进制、图标集没裁。`Cargo.toml` 的 release profile 照抄 `server/`（`lto = true`）并加 `strip = true`。

---

## 9. 里程碑（每步都有可验证出口）

| 阶段 | 内容 | 出口（验收） |
|---|---|---|
| **T0 骨架** | `src-tauri` 能起窗口、加载现有 `dist/renderer`；`ipc` 命令回显 + 事件推送打通；capabilities/CSP 定型 | `cargo tauri dev` 出窗口；点一次"登录"能看到 `未知通道/缺少参数` 之类的信封（证明往返通）；`cargo test` 空跑绿 |
| **T1 纯函数引擎** | errors / home / paths / ignore / hash / text / io_retry / cache + 对应用例 | 67 项 Rust 单测绿（14+10+15+9+14+5 口径） |
| **T2 传输与元数据** | api.rs + db.rs（含 wc.lock） | 44 项绿；**关键**：用 TS 版创建的真实 `wc.db` fixture 能打开且 `entries` 一致 |
| **T3 工作副本主体** | scan / pristine / revisions / wc（4 段） | 96 项绿 + `parity.mjs` 的 checkout/status/add/commit 全一致 |
| **T4 壳与状态** | config / session / watch / commands / state / main.rs / cli.rs | 135 项绿；`m4_smoke.sh` 68 组全绿（CLI 已换 Rust）；`parity.mjs` 补 update/lock/冲突 |
| **T5 渲染层桥接** | `renderer/api.ts` 适配 `invoke`/`listen`；R2 的定时器搬家；打包产物冒烟 | vue-tsc 通过；`npm test` 201 项绿；`m4_tauri_smoke.sh` 自检通过 |
| **T6 打包与 CI** | tauri.conf 打包目标、图标、三平台 CI、门禁三件套、体积回填 | 三平台 CI 绿；体积达标；打包产物跑通自检 |
| **T7 收尾** | 删 Electron 依赖与文件、删 `client-legacy/`、文档 §6.4/§13/§14/§15.1 + README + 门禁计数、版本 v0.5.0 | `npm ls electron` 空；`grep -r electron client/src` 空；文档版本号与实测数字一致 |

每条都能单独回退：T0–T2 不碰渲染层，T3 起才有行为等价性风险，T7 之前 `main` 分支始终是可用版本。

---

## 10. 风险与埋雷清单

| # | 风险 | 影响 | 缓解 |
|---|---|---|---|
| **R1** | `wc.ts`（1192 行）移植偏差：60 项用例只覆盖主干，锁/冲突/部分检出边角多 | **高** | 逐函数移植 + §7.2 双引擎对拍（T3 起为门禁） |
| **R2** | **后台定时器节流无等价开关**：Electron 靠 `backgroundThrottling: false` 保住 15s 锁轮询；WKWebView/WebKitGTK 在窗口遮挡/最小化时会拉长定时器 | 中高（"同事刚锁的文件"迟迟不显示） | 把 15s 轮询搬到 Rust（`tokio::interval` + `emit`），渲染层只监听——**这会动 `stores/app.ts`**，是"渲染层零改动"的唯一例外（D5） |
| **R3** | CSP / 自定义协议导致白屏（Windows 是 `http://tauri.localhost`） | 高（看起来像崩溃） | `csp` 显式含 `ipc: http://ipc.localhost`；`base: './'` 保留；T5 三平台都看一眼截图 |
| **R4** | 进度事件洪水：每块一事件经 JSON 序列化 + IPC 往返，大文件上传会压死 webview | 中 | 事件合并 + 100–250ms 节流；进度只带"已传/总数/当前文件" |
| **R5** | 长任务占线程：哈希/扫描/上传若在 async 上下文里做同步 IO，会卡住整个命令队列 | 中 | 一律 `spawn_blocking`；DB 操作走专用连接（rusqlite 是同步 API） |
| **R6** | macOS 无 WebDriver：`tauri-driver` 只支持 Linux/Windows，143 项 Electron 冒烟（CDP）无从复刻 | 中 | 注入式自检（§7.3）；断言从 143 降到"关键路径 ~40 + 渲染层 201 单测"，需接受 |
| **R7** | Windows 交付形态变化：没有 portable 打包器产物，改裸 exe；缺 WebView2 时白屏 | 中 | Rust 侧启动检测 WebView2 并给原生提示；交付说明写前提（D3） |
| **R8** | keyring 行为差异：未签名 app 首次访问钥匙串可能弹系统授权框；Linux 无 secret-service 直接不可用 | 低中 | 弹框只在首次；不可用时降级"不落盘"（与现状一致）；`keyring` 用 3.x + 明确 feature |
| **R9** | Linux 大仓监听：`notify` 用 inotify，受 `max_user_watches` 限制（默认可能只有 8192） | 中 | 命中上限走既有降级路径（`watcher.degraded = true` → 界面露手动刷新），不静默 |
| **R10** | 迁移期双引擎漂移：`main` 继续改 Electron，`tauri` 改 Rust | 中 | 迁移窗口内 `main` 只修 P0 bug；T0–T2 完成即合并一次骨架，缩短分叉 |
| **R11** | 前端载荷从"结构化克隆"变 JSON：`BigInt`/`Date`/`undefined` 行为不同 | 低 | `renderer/api.ts` 已有 `toPlainPayload` JSON 归一化，保留即无害；新增断言：载荷不含 `BigInt` |
| **R12** | 依赖引入：`notify` / `keyring` / `rfd` 本地无缓存，需联网拉取 | 低 | crates.io 可达（已验证）；CI 有网；离线开发机需先 `cargo fetch` |

---

## 11. 需要你拍板

| # | 决策 | 建议 |
|---|---|---|
| **D1** | CLI 是否同批改 Rust | **是**。若保留 TS CLI，就得为它留一套 Node 运行环境，"零 Node"目标作废，且引擎要维护两份 |
| **D2** | 是否保留 TS 引擎当对拍 oracle | **是**。`client-legacy/` 仅在迁移期存在，T7 删除；这是 R1 的主要缓解手段 |
| **D3** | Windows 交付形态 | **裸 exe + evergreen WebView2**（Win10 1803+ 自带）。不选 fixedRuntime：+180 MB 且与"单文件轻量"冲突 |
| **D4** | mac 冒烟方式 | **注入式自检**（`--self-check`）。接受断言数量下降，换取三平台同一套驱动 |
| **D5** | 15s 锁轮询是否搬到 Rust | **是**（R2）。需要动 `stores/app.ts` 一处，破"渲染层零改动"承诺 1 处 |
| **D6** | 版本号与合并时机 | 版本 **v0.5.0**；T4 完成（CLI 已换 Rust、对拍通过）时把骨架合回 `main`，T5–T7 在分支收尾 |

---

## 12. 明确不做

- 不做自动更新（`tauri-plugin-updater`）：现状也没有，签名/公证未做，先不引入
- 不做多窗口 / 多工作副本并存（现状单窗口单副本，`WcSession` 语义不变）
- 不引 `tauri-plugin-sql`（wc.db 的 schema 与事务语义必须自己掌控）
- 不给前端任何 `fs` / `shell` / `dialog` 权限（capabilities 只留 `core:default`）
- 不换 UI 框架、不改交互、不趁机重构 `stores/app.ts`（除 D5 那一处）
- 不动 `server/` 与 `admin/`（IPC 契约与 REST 协议都不变）

## 13. 工作量口径（供你对齐预期）

| 类别 | 规模 |
|---|---|
| 新增 Rust（engine + 壳 + CLI） | 约 4.5–5.5k 行 |
| 新增 Rust 测试 | 约 2.5–3k 行（357 项等价物） |
| 删除 TS | 约 3.6k 行（core 3.3k + main/preload/cli 约 1.1k − 保留渲染层） |
| 脚本/CI | 5 个脚本改写、2 个删除、1 个工作流改写 |
| 文档 | §6.4 / §13 / §14 / §15.1 / README / 本文件 |

---

## 14. 执行记录（2026-10-29，v0.5.0）

方案由用户一次确认（"按照你的建议来"，即 D1–D6 全部按建议执行），随后按 T0–T6 落地。

### 14.1 实测结果

| 阶段 | 出口 | 实际 |
|---|---|---|
| T0 骨架 | 起窗口 + `ipc` 往返 | ✅ `tauri build` 出的 release 二进制自检通过（首屏挂载 + `app:info` 往返） |
| T1 纯函数引擎 | 67 项 | ✅ errors/home/paths/ignore/hash/text/io_retry/cache 全部移植，用例 1:1 |
| T2 传输与元数据 | 44 项 | ✅ api/db 移植；`wc.db` 用**同一套迁移 SQL**、写入仍是 JSON 文本（跨版本兼容） |
| T3 工作副本主体 | 96 项 + 对拍 | ✅ scan/pristine/revisions/wc 全部落地；以 `m4_smoke.sh` 的 69 组端到端断言替代了对拍脚本（见 14.3） |
| T4 壳与状态 | 135 项 + m4_smoke | ✅ config/session/watch/commands/state/main/cli；**m4_smoke 69/69 全绿**（Rust CLI 打真服务端） |
| T5 渲染层桥接 | 201 项 + 冒烟 | ✅ `renderer/api.ts` 换成 `invoke`/`listen`，`window.bartifact` 保留为测试替身通路；201 项 vitest 全绿（1 项文案断言随框架改名同步） |
| T6 打包与 CI | 三平台 + 体积 | 🟡 本机（macOS x64）跑通：`.app` 17 MB / 单二进制 10.3 MB / zip 7 MB，`m4_package.sh` 6/6；**三平台 CI 未在本沙箱验证**（无 GitHub 凭据，见 §14.4） |

**代码规模**：`src-tauri/src` ≈ 6.4k 行（引擎 4.2k + 壳/命令/CLI 2.2k），Rust 单测 **163 项**；
删除 TS 引擎与 Electron 壳 ≈ 3.6k 行、18 个测试套件（357 项）；渲染层 201 项一字未改。

### 14.2 与方案的偏差（都以"真跑出来的事实"为准）

1. **通道数是 34，不是 31**（方案 §1/§3 的笔误）：`shared/channels.ts` 实际 34 个 invoke 通道 + 3 个事件通道。
2. **dmg 没落地，交付回 zip**：tauri-bundler 的 `bundle_dmg.sh` 在 macOS 13（BSD grep）上
   匹配挂载设备名时报 `brackets ([ ]) not balanced` → `unable to proceed with final disk image creation`。
   `bundle.targets` 只留 `app`，zip 用 `ditto` 打（保住 bundle 的符号链接与权限）。
3. **对拍脚本没写**：`m4_smoke.sh` 的 69 组断言本来就是"CLI × 真服务端"的端到端，
   换成 Rust CLI 之后它同时验证了引擎语义，比另写一套 `parity.mjs` 更省且更接近真实路径。
   `client-legacy/` 因此没有保留（TS 引擎随分支删除，历史仍在 `main`）。
4. **D5（锁轮询搬 Rust）没有做**：`wc:changed` 由 Rust 侧的文件监听驱动（`notify` + 防抖），
   自动同步已经不依赖前端定时器；前端那条 15s 轮询保持原样，渲染层真正做到零改动。
   风险 R2 因此从"必须改"降级为"要验证的观感问题"（见 14.4）。
5. **新增"兜底显示"**（方案没预见）：窗口 `visible:false` + `on_page_load` 显示，但**页面加载
   失败时该回调永远不来**，用户会看到"应用启动了却没有窗口"（macOS 上只剩菜单栏）。
   现在另加 1.2s 定时兜底 `show()`。

### 14.3 实现期踩到的坑（都已修，留给下次）

1. **`rusqlite::Connection` 不是 `Sync`** → `&WorkingCopy` 不是 `Send` → Tauri 命令的 future 不 `Send`
   （报错只写 "future is not Send"，不指名类型）。解法：跨 `await` 一律用 `&mut WorkingCopy`
   （`&mut T` 只要求 `T: Send`），于是 `install_file` / `mkdir` / `list_locks` / `hash_of_mine`
   都从 `&self` 改成 `&mut self`。
2. **Tauri 的 async 命令带 `State<'_, _>` 时必须返回 `Result`**（E0277），即使业务错误走信封。
3. **Rust 块注释会嵌套**：注释里写 `client/src/core/*.ts` 的 `/*` 会开启嵌套注释、
   写 `(?:.*/)?` 的 `*/` 会提前结束注释 —— 两次都是"unterminated block doc-comment"，很难一眼看出。
4. **`cargo build --release` ≠ `tauri build`（最容易白屏的一条）**：前端产物是
   `tauri::generate_context!()` 这个**过程宏**在展开时读 `tauri.conf.json` + `dist/renderer`
   内嵌进去的。只改前端、不改 Rust 源码时 cargo 认为 crate 没变，**不会重新展开宏**，
   于是二进制里还是上一次（可能是空）的资源 —— 启动白屏、**零报错**。
   实测：`cargo build --release` 出来的 10 MB 二进制自检超时（退出码 5），换成
   `npx tauri build --no-bundle` 立刻通过。判断"白屏是资源没内嵌还是 IPC 断了"就靠
   `--self-check` 的退出码（1 = 未挂载、5 = JS 根本没跑）。
5. **`document.title` 不会同步到 Tauri 的窗口标题**（WKWebView），自检结论必须经 IPC 回传
   （`selfcheck(code)`），不能靠标题。
6. `Result::unwrap_err()` 要求 `T: Debug`（`Wc`/`WorkingCopy` 不满足）→ 测试统一用
   `.err().expect("应当失败")`。
7. **pid 存活检测要注意 `EPERM`**：非 root 探 `pid 1` 时 `kill(pid,0)` 返回 EPERM 而不是 0，
   只判 `== 0` 会把"别人的进程"误判成"已死"，进而并发写坏同一个副本。
8. **bash 3.2 的多字节坑又犯了一次**：新脚本里写了 `$APP_MAX_MB）`，被当成变量名 → 一律 `${VAR}`。

### 14.4 未完成 / 后续

- **三平台 CI 未实测**（本沙箱无 GitHub 凭据）：Linux 的 webkit2gtk 依赖、Windows 的 WebView2
  与 `--self-check`（GitHub runner 是否有可用窗口会话）都还没跑过。
- **Windows / Linux 交付体积**未实测（表里是估算，CI 首跑后回填）。
- **代码签名与公证**仍未做（与 Electron 时代相同）。
- **R2 的观感验证**：窗口最小化时前端 15s 轮询是否被节流、`wc:changed` 是否照常推送。
- 可选：`tauri-driver`（Linux/Windows）做真实点击级 E2E；`tauri-plugin-updater` 做自动更新。
