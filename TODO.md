# mofang-ticket 实现 TODO

> 由 `DESIGN.md` 第 14 节细化而来，按「可勾选、有依赖、可验收」拆解。每条的验收锚点指向 `DESIGN.md` 对应章节。完成后把 `[ ]` 改为 `[x]`。

## Phase 0 — 准备与骨架

- [x] **0.1 环境确认**：宿主 `≥ v0.1.18`；部署 OS/CPU/GNU（musl 不支持动态加载）；`[official_host.webhook]` 可开启。→ 本机 `cargo 1.97.1`，Windows x64 msvc。
- [x] **0.2 建库**：在仓库根创建 crate `qimen-dynamic-plugin-mofang-ticket`（`Cargo.toml` + `src/lib.rs`）。
- [x] **0.3 Cargo.toml**：`[lib] crate-type=["cdylib"]`；依赖 `abi-stable-host-api 0.1.13` + `qimen-dynamic-plugin-derive 0.1.13`（同版本）+ `serde`/`serde_json` + `hmac`/`sha2`/`hex`。验收：DESIGN.md §2。
- [x] **0.4 空骨架编译**：`#[dynamic_plugin(id="mofang-ticket", version="0.1.0", api="0.6")]` 可编译。

## Phase 1 — 标准 Webhook + 群推送（本次落地目标）

- [x] **1.1 统一信封模型**：serde 结构体 `Envelope`/`Ticket` + 事件字段（changes/actor/content/internal_note 保留为 `Value`）。验收：DESIGN.md §4.1/§4.2。
- [x] **1.2 签名校验**：HMAC-SHA256 constant-time（`Mac::verify_slice`）；时间窗 `|now-ts|≤300s`；`nonce` 有界缓存；对**原始 body 字节**验签（用 header 原始 ts 字符串）。失败 401/400。验收：§4.3、§18.2。
- [x] **1.3 幂等去重**：`event.id` 有界缓存（TTL 24h），重复投递返回 `dup:true`。验收：§4.1/§13。
- [x] **1.4 Webhook 路由**：`#[webhook(method="POST", path="/events")]`；同步回调「验签→nonce→信封校验→去重」；响应 200/401/400/503。验收：§4.4。
- [x] **1.5 配置解析 + 最小 Schema**：`inbound.{enabled,secret,timestamp_tolerance_secs,nonce_cache_size}`；`config.schema.json` 根节点 object、密钥 `writeOnly+x-qimen-secret`。`notify.targets` 随 1.6。验收：§11。
- [x] **1.6 群推送（MVP 透传）**：`notify.targets` 解析 + `BotApi::for_account(...).send_group_msg(...)` 把原始 body 原文透传 + `SendEnqueueStatus.is_accepted()` 检查。模板渲染（text+at 段）留待后续。验收：§8.1。
- [x] **1.7 单元测试**：验签通过/失败/篡改 ts/时间窗越界/非法 hex、信封解析、配置解析（9 项全绿）。验收：§16。
- [ ] **1.8 构建 + 加载验证**：`cargo build --release` 已通过（产物 `qimen_dynamic_plugin_mofang_ticket.dll`）；**宿主加载 + 端到端 `POST /events` 待真实宿主**。验收：§15。

## Phase 2a — 管理员档案

- [ ] **2a.1 档案存储**：`{data_dir}/profiles/{account_id}/{sender_id}.json`，临时文件 + 原子 rename。验收：§6.4。
- [ ] **2a.2 字段策略**：`admin_profiles.fields.*` 的 `editable` + `scope(none/internal/customer)` 加载与「写入/渲染」双处执法。验收：§6.2。
- [ ] **2a.3 `/ticket profile` 命令**：查看 / `set key=value` / `unset` / `clear`，锁定字段拒绝并提示。验收：§6.3。

## Phase 2b — 出站人工介入

- [ ] **2b.1 命令**：`list/detail/accept/transfer/reply/close`（根命令 `/ticket`，`role=admin`，`scope=group`，参数解析不 panic）。验收：§9。
- [ ] **2b.2 双上下文模板引擎**：`customer/internal` 上下文；占位符全集 + 回退链；转义/截断/不二次展开；`validate_config` 拒绝越权引用。验收：§7.2–§7.5。
- [ ] **2b.3 outbound worker 线程**：`#[init]` 启动、`#[shutdown]` 停止并 join；有界 mpsc；同步 HTTP（`ureq`+rustls）POST 到 `outbound.url`；超时 + 有限重试 + 指数退避。验收：§8.2、§12。
- [ ] **2b.4 出站签名 + 回执**：同构 HMAC 头；完成后 `BotApi::for_account` 主动回发结果（成功/失败 + 客户可见正文预览，不含敏感字段）。验收：§4.3、§8.2。

## Phase 3 — 持久化 / 增强 / 发布

- [ ] **3.1 投影持久化**：`storage.enabled` + SQLite（可选），「工单→群消息 ID」映射以支持群内跟进。验收：§5。
- [ ] **3.2 富文本增强**：官方 QQ Markdown/卡片按平台实测后作为增强（不破坏 text/at 基线）。验收：§10.5。
- [ ] **3.3 商城发布**：LICENSE、驱动矩阵（onebot11 / qq-official 实测场景）、按 target 的资产名/大小/SHA256、构建证明。验收：skill `marketplace-publishing.md`。

---

## 依赖关系速览

```text
Phase 0 ─▶ 1.1 ─▶ 1.2 ─▶ 1.3 ─▶ 1.4 ─▶ 1.5 ─▶ 1.6 ─▶ 1.7 ─▶ 1.8   （Phase 1 可交付）
                                              │
                                              └▶ 2a.1 ─▶ 2a.2 ─▶ 2a.3 ─▶ 2b.1 ─▶ 2b.2 ─▶ 2b.3 ─▶ 2b.4
                                                                                    （Phase 2 可交付）
                                                                                        └▶ Phase 3
```

## 每个阶段的「完成」定义

- **Phase 1**：`DESIGN.md §16` 前 3 项验收 + 1.7 单测全绿 + 1.8 真实宿主一次端到端（魔方模拟推送 → 群收到）。
- **Phase 2a**：档案可自助改、字段策略三处一致执法、重启保留。
- **Phase 2b**：`reply/accept/transfer/close` 端到端（命令 → outbound worker → 魔方回执 → 群回执），双上下文客户可见正文只含 `customer` 字段。
- **Phase 3**：商城收录要求全满足。
