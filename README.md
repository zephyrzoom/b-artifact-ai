# b-artifact

面向**二进制资产**（美术资源、设计稿、工程文件、影音素材）的类 SVN 集中式版本管理系统。

- 服务端：Rust（axum + SQLite + 内容寻址 blob 存储）
- 管理端：Vue 3（由服务端静态托管）
- 客户端：Electron（工作副本目录树、目录级部分检出、文件锁 + 先锁后提交）

## 当前状态

| 里程碑 | 状态 |
|---|---|
| 方案设计 | ✅ `docs/架构设计方案.md`（**v0.4.17**，决策点全部定案；#2 锁策略 / #5 目录锁于 v0.4.17 修订） |
| M0 服务端骨架 | ✅ 完成：schema 迁移、BlobStore（zstd auto）、Path-History 核心（Tombstone / restore / HEAD 快路径 / manifest_hash 异步）、可启动空服务端 |
| M1 核心链路（认证 + 建仓 + 分块上传 + 两阶段提交 + 浏览查询） | ✅ 完成：本地/LDAP 认证、会话、登录限流、`--create-admin`、仓库管理、整块/分块上传、下载（Range/HEAD/zstd 流式解压）、两阶段提交（幂等重放/OUT_OF_DATE）、tree/log/changes/info；单测 26 + 集成测试 5 + 冒烟 34 全绿 |
| M1.5 压测 | ✅ 完成：造数脚本（`xs` 1k → `full` **1M 文件 / 100k 修订**）、进程内 axum 压测 harness、四档实测。**结论：Path-History 两条核心查询在 `full` 档不可用**（`list_dir_at_root_head` 86.9 s、`tree?rev=` 端到端 75.5 s、`apply_commit_1000` 267.8 s），定位 6 个瓶颈（B1 前缀 LIKE 未下推索引、B2 语句重复 prepare 为 P0 阻断项）。**上述三条已于 M1.6 修复**。见[压测报告](docs/压测报告-M1.5.md) |
| M1.6 性能修复轮 | ✅ 完成：B1 前缀下推（半开区间）、B2 语句缓存、B4 `parent_path` + 目录物化、B5 部分索引、B6 连接池；顺带修掉 `rebuild_head_tree` 丢显式空目录的真 bug、清理 prepare 幂等 hack。xs/s/m 三档回归：`list_dir_head_root` 206 ms → **5 µs**、`tree_root_depth1` 203.87 ms → **210 µs**、`apply_commit_1000` 67.5 s → **21 ms**、并发 `tree_root_depth1_c8` 5/s → **3451/s**。见[压测报告](docs/压测报告-M1.6.md) |
| M2 权限与锁（ACL 屏障、目录锁、强制解锁、审计） | ✅ 完成：迁移 004（`idx_group_members_user` / `idx_locks_live` / `idx_locks_owner_live` / `idx_head_entries_blob`）、ACL 纯函数引擎（`pick` 主体具体度 + 继承屏障 + 回溯）、`effective_level` 逐路径校验、读路径（info/tree/log/changes + 仓库列表）按权限**过滤**、锁服务（file/dir 递归互斥、本人幂等、强制解锁留痕与 reason 必填、子树连带强制解锁、心跳续期）、strict 仓库 `needs_lock`、审计日志；单测 63 + 集成测试 5（M1）+ 4（M2）+ 冒烟 52 全绿；`cargo llvm-cov` 行覆盖 `acl.rs` 99.1% / `storage/acl.rs` 96.7% / `locks.rs` 96.5%（验收门槛 ≥80%） |
| M3 管理端 Vue 3 | ✅ 完成：服务端管理 API（`stats` / `users` / `groups` / `repos` / `acl` 含**有效权限预览器**与"谁有权限"反查 / `audit` 含 CSV 导出 / `maintenance` 含 rebuild-refcount 与 GC / `purge`），全部要求系统管理员；`/admin` 静态托管（SPA 回退 + 带 hash 资源长缓存 + 路径穿越防护）；Vue 3 管理端 6 视图 2 组件。**顺带修掉两个遗留真 bug**：`purge` 的 SQLite 嵌套事务冲突、本地用户首登不授予系统管理员。单测 76 + 集成 5（认证）+ 5（M1）+ 4（M2）+ 9（M3）+ 冒烟 52（M1）/ 66（M3）全绿；全库行覆盖 **85.9%**（门槛 ≥80%，cargo test 可达模块全部达标） |
| M4 客户端（引擎 + CLI） | ✅ 第一轮完成：TypeScript 引擎 `client/src/core`（api / db / wc / scan / paths / ignore / hash / pristine / errors / home / util）+ `cli.ts`（login、repos、checkout、status、add、remove、revert、commit、update、lock、unlock、locks、log，全部支持 `--json`）；单测 **110** 全绿 + `tsc --noEmit` 零错误 + `scripts/m4_smoke.sh` **68 组断言**全绿（自建服务端，覆盖登录/建仓/检出/状态机/提交/部分检出/更新/冲突/删除/锁/边界）。**Electron 壳与 UI 顺延** |
| M4 第二轮（管理端前端测试） | ✅ 完成：`admin/` Vitest **131** 项 + `vue-tsc` 零错误 + `scripts/m4_admin_e2e.sh` 真实浏览器 **31** 项（登录 / 用户 / 组 / 权限矩阵含屏障 / 预览器 / 谁有权限 / 锁与强制解锁 / purge 二次确认 / 审计）。把组件里的判定逻辑抽成 `src/utils/{acl,dirs,purge}.ts` 纯函数以便单测；顺带修掉 2 个真 bug（`formatBytes` KB 档被整数化、`PurgeDialog` 初始挂载不预填前缀）。清掉了 M3 起欠的 §15.2 P1 项 |
| M4 第三阶段（Electron 壳 + 渲染层） | ✅ 完成：`src/shared/`（IPC 通道白名单 + payload schema + 统一信封）、`src/main/`（窗口安全基线 / IPC 注册器 / 通道处理器 / 配置 / 会话 / 错误）、`preload.ts`（contextBridge 窄接口）、`src/renderer/`（Vue 3 + Pinia + Element Plus：登录 / 仓库 / 工作副本 / 锁 + 状态表 + 传输队列）。客户端单测 **302** 全绿 + 两份类型检查零错误 + `scripts/m4_electron_smoke.sh` 真实 Electron **30 项**全绿（`npm run build && npm start` 即可跑起来）。本轮抓到并修掉 4 个真 bug：adopt 同一工作副本实例把自己关掉、响应式数组过 IPC 被拒、`unversioned` 行不可勾选导致"标记新增"点不动、漏引 Element Plus CSS 导致界面无样式 |
| M4 第四阶段（冲突解决） | ✅ 完成：wc.db 迁移 v2 新增 `conflicts` 表（老库自动升级、存量数据不丢）、`core/text.ts`（二进制探测 + 上限内文本读取）、引擎 `conflicts`/`conflictSides`/`resolveConflict`（取本地 / 取服务端 / 写入合并）、三个 IPC 通道、`ConflictView` + `ConflictResolver`（本地 / 服务端 / 基线三方并列，文本可编辑合并，二进制只能二选一）。客户端单测 **364** 全绿 + 真实 Electron 冒烟 **45 项**全绿（含"制造冲突 → 三方对比 → 合并 / 保留被删文件 → 提交入库"整条链路）。修掉 1 个真 bug：解决冲突后 `status` 把 modified 误报成 normal（`mtime+size` 快速路径被自己刚写回的 mtime 命中） |
| M4 第五阶段（历史 + 设置视图） | ✅ 完成：`core/revisions.ts`（修订下定位文件 + 流式下载旧版本，重名不覆盖）、`core/cache.ts`（缓存占用统计与清理，含残留临时文件）、五个 IPC 通道、`HistoryView`（修订列表分页 / 单修订文件明细 / 下载此版本）、`SettingsView`（并发数 / 忽略规则 / 缓存目录与占用 / 清理 / 最近工作副本）。**§6.5 的八个视图至此全部落地**。客户端单测 **437** 全绿 + 真实 Electron 冒烟 **59 项**全绿（含"下载 r3 旧内容并确认工作副本未被覆盖"、"改配置后读主进程落盘的 config.json"）。本轮还修掉一个**门禁缺陷**：渲染层的 `vue-tsc` 因继承的 `exclude` 一直在空转，修好后立刻抓出 5 处真实类型问题 |
| M4 第六阶段（目录树 + 勾选式部分检出） | ✅ 完成：`components/FileTree.vue`（`el-tree` 懒加载、按需拉一层、只显示 ACL 允许看到的条目、已选前缀标签）+ `utils/sparse.ts`（选择集不变量：去空去重、去掉被祖先覆盖的后代；勾选=收窄或放宽；换仓库自动清空）。**§6.5 至此全部落地**。客户端单测 **476** 全绿 + 真实 Electron 冒烟 **67 项**全绿。本轮修掉 1 个真 bug（靠截图发现）：部分检出的前缀目录没有基线条目，刚检出就显示"1 个未纳管" |
| M4 第七阶段（打包出可分发产物） | ✅ 完成：`electron-builder.yml`（asar 只带 `dist/` 与 `package.json`，**1.5 MB**）、`scripts/make-icon.mjs`（零依赖手写 PNG 编码器出 1024×1024 图标）、`scripts/m4_package.sh`（构建 → 打包 → **拿打包产物跑同一套 67 项冒烟**）。产物：`b-artifact.app`（307 MB）+ `b-artifact-0.1.0-mac.zip`（126 MB）。遗留：dmg 需联网下载工具包（本沙箱不可达）；**代码签名与公证未做**，对外分发需补 |
| **M4.8 需求变更轮（v0.4.17）** | ✅ **完成**（2026-09-15，来自真实使用的 8 项反馈）：①删 `advisory` 与目录锁，唯一语义 = 每个变更路径都必须本人持锁，建仓不再选策略 ②口令强度策略（≥8 **字符** + 数字/大写/小写/符号，替换现有 4 处 `len() < 8`——其中三处数的是**字节**）③管理端仓库列表「先锁后提交」列显示空白标签的修复（随①删列）④用户信息去邮箱 ⑤加锁入口移入工作副本 ⑥工作副本改**目录树** ⑦`fs.watch` 自动同步 + 去「标记新增」+ 说明可空 + 树上直接增删 ⑧「打开已有副本」入口。**上线反馈陆续改了这些**：锁不再单独占菜单（并入工作副本树）、修掉「选中目录加锁却锁住某个文件且解不开」、目录树支持**双击展开/收起**、界面术语统一为「强制解锁」、**刷新不再冲掉勾选**；**加锁改成点了就锁（不弹备注框）**、**锁动作只在右键菜单**（工具栏不再放按钮）、**提交后自动解锁**（本次提交文件上本人持有的锁全部释放）、右键菜单点别处即关、**从仓库页检出/打开后自动跳到工作副本页**、「打开已有副本」弹窗的操作列钉右（不必拉横向滚动条）+ 检出后最近列表即时刷新、**刚打开工作副本时树上不预勾选任何项**、**删除/还原按勾选项执行**（不是高亮那一行，且删除后清空勾选）；「**部分检出**」这一轮改名（原名"稀疏检出"）；**去掉「新建文件」功能**（新建目录保留）；**忽略规则从设置移到工作副本**（编辑本副本的 `.b-artifactignore`，不再有全局追加规则）；顺带修掉"监听抑制窗口里的事件被丢弃"与"窗口被遮挡时定时器被节流"；**仓库页的部分检出目录树不再要求先打开工作副本**（仓库级读通道显式带 repo）；「忽略规则」与「打开已有副本」弹窗**可拖动**（不再挡住后面的目录树）；**设置里可配「检出目录的默认路径」**，检出时默认 `该路径/仓库名`。门禁：服务端单测 **79**（+集成 23）、客户端单测 **558** + 真实 Electron **143 项**、管理端单测 **137** + 浏览器 E2E **31 项**、冒烟 m1 **54** / m3 **69** / m4 **68** 全绿 |
| M4 剩余（文本 diff、Windows 矩阵、签名） | ⬜ 未开始 |
| **M1.7 服务端性能轮（历史修订列目录）** | ✅ 完成：清掉 M1.6 唯一未达标项。`tree?rev=<旧修订>` 端到端 **6557.6 ms → 15.77 ms（416×）**、`rev=head` **7.4 s → 0.049 ms**。三个根因：缺 HEAD 快路径、按整棵子树枚举候选（改为"HEAD 直接子项 ∪ (rev, head] 删除增量"）、**`head_entries.changed_rev` 存量库被污染**（新增迁移 006 从 `changes` 重算，修掉界面上"最后变更修订"全显示同一个值的错）。新增差分验证脚本 `scripts/verify_list_dir_at.py`（旧语义参考实现 vs 实际输出，30/30 组一致）。见 [压测报告](docs/压测报告-M1.7.md) |
| M5 打磨发布 | ⬜ 未开始 |

> 旧的 M0 草稿代码已归档至 `_attic/server-m0-draft/`，不作为实现基础。

### 还剩什么（截至 2026-09-15）

M0 ~ M3 已完成并且有回归网；M4 客户端做了六轮，**功能层面已经完整**（服务端 / 管理端 / 客户端三个面都能真跑起来）。
未完成的项按性质分三类：

| 类别 | 项 | 性质 |
|---|---|---|
| ~~**M4.8 需求变更**（v0.4.17）~~ ✅ **已完成** | 锁语义收敛、口令强度、工作副本目录树与自动同步、去邮箱、加锁入口收敛、打开已有副本 | 契约与冒烟用例已同批更新（`lock_policy` / `kind: 'dir'` 全部下线） |
| **M4 体验补完** | 历史视图的**文本 diff**（现在只能"下载旧版本自己比"） | 锦上添花 |
| **M4 平台验证** | Windows 平台矩阵 | §6.6 的那些差异（大小写不敏感、保留名、路径分隔符、mtime 精度）需要 Windows 机器实测，本机做不了 |
| **M4 分发收尾** | 代码签名 + 公证；dmg 目标 | 沙箱里没有 Developer ID 证书（产物本机可跑，分发给别人会被 Gatekeeper 拦）；dmg 要联网下载工具包 |
| **M5 打磨发布** | GC/purge **全链路**（定时调度 + 启动脏检测）、TLS、运维手册 | 功能已有（管理端可手工触发），缺的是"无人值守"与部署安全 |
| **P2 未来**（v1 范围外） | LDAP 组同步、离线提交缓冲、只读镜像/异地同步、目录 Merkle 树、`auth/ldap.rs` 单测（需真实 LDAP） | 方案里明确留给 v1.1 / v2 |

> 说明：§15.2 里标着"P0 必做"的六项（commit_id 幂等、目录 Tombstone、refcount 可重建、GC 排除 pending、分块上传、wc.db）
> **实现早已落地**，此前是清单没回勾——已在 v0.4.14 逐条修正并注明代码落点。

## 快速启动（服务端）

```bash
cd server
cargo run -- --data-dir ./data --listen 127.0.0.1:8080

curl http://127.0.0.1:8080/health
```

打开管理端：<http://127.0.0.1:8080/admin/>（需先构建前端，见下）。

### 构建管理端前端

管理端是独立的 Vite 工程，构建产物 `admin/dist` 由服务端在 `/admin` 下静态托管：

```bash
cd admin
npm install
npm run build          # → admin/dist
cd ../server
cargo run -- --data-dir ./data --listen 127.0.0.1:8080 --admin-dir ../admin/dist
```

- `--admin-dir` 省略时默认 `admin/dist`（相对当前工作目录）。
- 前端 `vite.config.ts` 的 `base` 必须是 `/admin/`，否则资源路径与挂载点对不上。
- 未构建时访问 `/admin/` 会返回一段提示页（含构建命令），不会白屏。

开发期可只跑前端的 Vite dev server（需另配 API 代理），见 `admin/vite.config.ts`。

说明：v0.3 起**无需预建管理员**——系统里第一个成功登录的用户自动成为系统管理员（方案 §9.1）。
若需要预先创建本地管理员（如 CI / 冒烟环境）：

```bash
cargo run -- --data-dir ./data --create-admin admin
```

运行测试（单测 + 认证 / M1 / M2 / M3 端到端集成测试）：

```bash
cd server && cargo test
```

核心模块行覆盖（M2 验收项 ≥80%，需 `rustup component add llvm-tools-preview` +
`cargo install cargo-llvm-cov`）：

```bash
cd server && cargo llvm-cov --summary-only
```

当前实测：全库 **85.9%**；`acl.rs` 99.1%、`locks.rs` 96.5%、`storage/purge.rs` 96.1%、
`storage/acl.rs` 92.8%、`api/admin.rs` 83.0%、`api/auth.rs` 82.7%、`api/static_files.rs` 84.4%。
未覆盖的只有两个 cargo test 天然不可达的文件：`auth/ldap.rs`（需真实 LDAP 服务）与
`main.rs`（CLI 入口，改由 `scripts/` 冒烟覆盖），见方案 §15.2。

运行全链路冒烟（需先建管理员并启动服务端，详见脚本头部注释）：

```bash
scripts/m1_smoke.sh        # M1 + M2：52 组断言
scripts/m3_smoke.sh        # M3 管理 API：66 组断言
scripts/m4_smoke.sh        # M4 客户端：68 组断言（自己起服务端，无需先建管理员）
scripts/m4_admin_e2e.sh    # 管理端真实浏览器 E2E：31 项断言（自己构建 + 起服务端 + 备浏览器）
scripts/m4_electron_smoke.sh  # 客户端 Electron 冒烟：143 项断言（真实应用走真实 IPC）
```

> `m4_smoke.sh` / `m4_admin_e2e.sh` 与另两个不同：它们**自建服务端**（临时数据目录 +
> 独立端口）并把客户端状态重定向到临时目录，可重复执行、不留残留、不碰开发者真实的
> `~/.b-artifact`。`PORT=18325 scripts/m4_smoke.sh` 改端口，`KEEP=1` 保留工作目录与截图。
> E2E 首次运行会自动下载 Chrome for Testing 的 headless shell 到
> `~/.cache/b-artifact-tools/`（纯下载，不走 npm），之后复用；`CHROME_BIN=<path>` 可指定自有浏览器。

> 冒烟脚本用 `--listen` 固定端口（默认 18321）。若上一轮的服务端没退干净，新进程会
> `Address already in use` 退出、请求会打到旧二进制上（表现为新增端点全部 404）。
> 跑之前先确认端口空闲（`lsof -nP -iTCP:18321 -sTCP:LISTEN`）。
>
> 写冒烟脚本时注意：**带 JSON body 的 curl 必须写在顶层赋值里**
> （`R=$(curl ... -d '{...}')`）。嵌在 `$( )` 里再写 `\"` 转义会被 bash 二次处理，
> `{...}` 退化成花括号展开，body 被拆成多个 `-d` 参数 → 服务端 422。

## 管理端测试（M4 第二轮补齐）

```bash
cd admin
npm test          # Vitest：137 项（format / http / api-contract / auth-store / acl / dirs / purge / password / components）
npm run typecheck # vue-tsc --noEmit
npm run test:cov  # 覆盖率：utils 与 stores 100%、api 97%、组件 92%（视图层交给 E2E）
```

- **单测**不依赖后端：请求层用自建 `fetch` 桩（断言 URL / 方法 / 请求体 = 端点契约），
  组件用 Element Plus 轻量替身（保留真实 DOM 语义，避开 happy-dom 下的 teleport / 尺寸观察）。
- **E2E** 见上面的 `scripts/m4_admin_e2e.sh`，真实服务端 + 真实浏览器，跑完把截图留在工作目录里。
- 权限矩阵 / 预览器 / purge 的判定逻辑抽在 `src/utils/{acl,dirs,purge}.ts`，组件只负责渲染——
  要改这些规则先改纯函数、再跑 `npm test`。

## 客户端桌面应用（M4 第三阶段）

主进程 / preload / 渲染层三层，`npm run build` 一次出三份产物：

```bash
cd client
npm install
# Electron 二进制的 npm postinstall 常被沙箱拦，用离线落位脚本补齐（纯下载，可重复执行）
../scripts/setup_electron.sh
npm run build      # dist/main（ESM） + dist/preload（**CJS**） + dist/renderer（Vue）
npm start          # 启动应用（脚本会清掉宿主注入的 ELECTRON_RUN_AS_NODE）
npm test           # Vitest：558 项（引擎 + shared/main/preload/renderer）
npm run typecheck  # tsc + vue-tsc 两份检查
```

- 开发期渲染层可走 Vite dev server：`npm run dev:renderer`，然后
  `B_ARTIFACT_DEV_URL=http://127.0.0.1:5273 npm start`。
- **`ELECTRON_RUN_AS_NODE` 必须清掉**：WorkBuddy 等 Electron 宿主会把它注入子进程，
  带着它 Electron 会退化成纯 Node（`--version` 打 Node 版本、窗口永不出现）。
  `npm start` 已经处理；手工跑二进制时记得 `env -u ELECTRON_RUN_AS_NODE`。
- 渲染层不碰 token、不发网络请求：一切都从 `window.bartifact` 过 preload，
  主进程按 `src/shared/channels.ts` 的白名单 + schema 校验后才落到引擎。

### 打包（出可分发产物）

```bash
cd client
npm run icon        # 生成 build/icon.png（1024×1024，零依赖手写 PNG 编码器）
npm run dist        # = build + icon + electron-builder → release/
npm run dist:dir    # 只出 .app（不压缩，快）
```

产物在 `client/release/`：

| 产物 | 大小 | 说明 |
|---|---|---|
| `mac/b-artifact.app` | ~307 MB | 可直接双击运行（绝大部分是 Electron Framework） |
| `b-artifact-0.1.0-mac.zip` | ~126 MB | 分发用：解压后拖进「应用程序」 |
| `Contents/Resources/app.asar` | **1.5 MB** | 只含 `dist/`（main/preload/renderer）与 `package.json` |

```bash
scripts/m4_package.sh    # 打包门禁：构建 → 打包 → 用打包产物跑同一套 143 项冒烟
```

两条要注意的：

- **`dependencies` 是空的**。渲染层已被 Vite 预打包进 `dist/renderer`，主进程只依赖 `electron`
  与 Node 内置模块——运行时不从 `node_modules` 取任何东西。所以 Vue / Element Plus / Pinia
  全在 `devDependencies`，asar 因此只有 1.5 MB。将来真加了运行时依赖，
  `files: ['!node_modules/**']` 会让它缺席——**打包产物冒烟会立刻暴露**。
- **产物未做代码签名与公证**（沙箱里没有 Developer ID 证书）。本机可直接运行；
  分发给别人时对方需要右键「打开」绕过 Gatekeeper，正式对外发布要补签名 + 公证。
  `dmg` 目标同理暂时没出（它要额外下载工具包，见 `client/electron-builder.yml` 的注释）。

真跑一遍全链路（自建服务端 + 真实 Electron，143 项断言，含自动同步与刷新后勾选保持、树上增删、双击展开/收起、右键菜单、树上加锁/解锁/强制解锁、部分检出、冲突解决、历史下载与设置）：

```bash
scripts/m4_electron_smoke.sh       # KEEP=1 保留工作目录与截图
```

## 客户端 CLI（M4 第一轮）

引擎层是纯 TypeScript（`client/src/core`），CLI 直接复用它，Electron 主进程后续也复用同一套：

```bash
cd client
npm install
npm test            # Vitest：558 项（引擎 + shared/main/preload/renderer）
npm run typecheck   # tsc --noEmit

CLI="node node_modules/.bin/tsx src/cli.ts"
$CLI login    --server http://127.0.0.1:8080 --username admin --password '***'
$CLI repos
$CLI checkout --repo art --dir ~/art [--sparse characters]   # 部分检出
$CLI status
$CLI add characters/hero.psd          # 会连带把无基线的父目录纳入版本控制
$CLI commit -m "update hero"
$CLI update
$CLI lock characters/hero.psd -m "改贴图"
$CLI unlock characters/hero.psd [--break --reason "人已离职"]
```

所有命令都支持 `--json`（结构化输出，供脚本断言）。凭据与全局 blob 缓存默认在
`~/.b-artifact`，可用 `B_ARTIFACT_HOME` 整体重定向（冒烟脚本正是靠它隔离）。

## 文档

- [架构设计方案](docs/架构设计方案.md) —— 总体架构、数据模型、目录权限语义、锁模型、传输协议、管理端（§8）、测试方案（§12）、里程碑与风险
- [M1.5 性能压测报告](docs/压测报告-M1.5.md) —— 四档实测（1k ~ 1M 文件）、6 个瓶颈清单与修复方向
- [M1.6 性能修复报告](docs/压测报告-M1.6.md) —— B1/B2/B4/B5/B6 落地说明、xs/s/m 三档修复前后对比、B6 连接池调参结论
- [M1.7 历史修订列目录报告](docs/压测报告-M1.7.md) —— 三个根因、差分验证方法、m 档实测（416×）、已知边界

## 目录

```
server/    Rust 服务端（axum + SQLite + 内容寻址 blob 存储）
admin/     Vue 3 管理端（构建产物由服务端静态托管于 /admin）
client/    客户端：src/core（引擎）+ cli.ts + src/main,preload,renderer（Electron 应用）
scripts/   造数 / 压测 / 冒烟脚本
docs/      方案与压测报告
```

## 性能压测

```bash
# 一键跑全档位（造数 → 压测 → 原始 JSON 落到 bench-results/）
scripts/m15_bench.sh --scales xs,s,m,full --iterations 25 --budget 12 --concurrency 8,32

# 复用已有 fixture，只重跑压测
scripts/m15_bench.sh --scales xs,s --reuse

# 汇总表
python3 scripts/m15_summary.py bench-results

# 关键查询的 EXPLAIN QUERY PLAN 证据（含 B4 的 parent_path 方案验证）
python3 scripts/plan_evidence.py --scale full --demo-parent

# 修复前后对比表（两份 bench 结果目录 → markdown）
python3 scripts/m16_compare.py --before bench-results-m15-baseline --after bench-results-final --scales xs,s,m
```

> 并发组结论**必须取自完整跑**。单独 `scripts/m15_bench.sh --only conc` 因冷页缓存在本机不可复现（同配置两次可差 10 倍），只能用来定性、不能用来调参。
