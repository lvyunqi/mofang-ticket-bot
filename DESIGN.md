# 魔方财务工单 · QimenBot 插件设计规范

> 状态：设计稿（未落地实现）。本文档是后续实现的唯一基线，先统一 Webhook 契约，再叠加人工介入能力；人工介入统一走「管理员档案 + 字段策略 + 内容合并排版」规范。

## 1. 目标与范围

本插件让 QimenBot 成为「魔方财务系统工单」与 QQ 群之间的双向消息网关：

- **入站**：魔方系统通过标准 Webhook 把工单事件推给插件，插件规范化后按统一排版主动推送到指定群。
- **出站**：管理员在群内通过命令「接单 / 转移 / 回复 / 关闭」，插件把「管理员档案 + 工单上下文 + 命令内容」合并成规范正文，翻译成标准 Webhook 事件推送到配置的魔方远端地址。
- **管理员档案**：管理员可自助配置自己的对外信息（姓名、职称、联系方式、回复落款）；哪些字段可改、哪些字段能展示给客户、哪些能进回复模板，全部由**字段策略**集中配置（第 6 节）。
- **内容合并排版**：区分「客户可见」与「内部可见」两套渲染上下文，模板编排预先设计好（第 7 节）。
- **协议**：同时兼容 OneBot 11 与官方 QQ Bot（下文第 10 节给出具体约束）。

**边界声明**：魔方财务系统是工单状态的唯一权威（system of record）。插件只做「规范化转发 + 命令前端 + 管理员档案 + 投影缓存」，不自行发明工单状态，不在本地重建权威状态机。

---

## 2. 插件类型选型：动态插件（API 0.6）

| 判定条件 | 结论 |
|---|---|
| 需要 API 0.5+ Webhook 收第三方推送 | 必须动态插件 |
| 需要后台主动发送（工单推送不依赖来信会话） | 动态插件实时发送 |
| 需要 API 0.6 在线配置（远端地址、群号、密钥、模板、字段策略） | 动态插件 `config_schema` |
| 需要管理员档案持久化（文件存储 + 后台写） | 动态插件 `data_dir` + worker 线程 |
| 无 QimenBot 主框架源码、独立仓库发布、热重载 | 动态插件 |
| 结论 | **`cdylib` 动态插件，`api = "0.6"`** |

固定技术基线（三套版本不得混用）：

| 项目 | 取值 |
|---|---|
| 插件 ID（发布后不可变） | `mofang-ticket` |
| QimenBot 宿主 | `v0.1.18` 及以上 |
| `abi-stable-host-api` | `0.1.13` |
| `qimen-dynamic-plugin-derive` | `0.1.13`（与上一行同版本） |
| 动态 ABI API | `"0.6"` |

---

## 3. 总体架构与数据流

```text
魔方财务系统 ──POST /webhooks/mofang-ticket/events──▶ QimenBot Webhook Gateway
                                                         │
                                                         ▼
                                              ┌──────────────────────────────┐
                                              │  mofang-ticket 动态插件         │
                                              │  ① 验签 / 去重 / 规范化           │
                                              │  ② 事件分发 + 投影缓存            │
                                              │  ③ 按「群推送排版模板」渲染         │
                                              └──────────┬───────────────────┘
                                                         │ BotApi::for_account
                                                         ▼
                                                群聊（管理员工单群）

群聊命令（管理员） ── /ticket accept|transfer|reply|close|profile ──▶ CommandRequest
                                                         │ 读取管理员档案 + 字段策略
                                                         │ 按「出站合并模板」渲染（区分客户/内部）
                                                         │ 入队
                                                         ▼
                                              ┌──────────────────────────────┐
                                              │  outbound worker 线程          │
                                              │  签名 + HTTP POST（重试退避）    │
                                              └──────────┬───────────────────┘
                                                         ▼
                                         魔方远端 Webhook 地址（配置项 outbound.url）
```

两个关键事实（已对 API 源码核实）：

1. **Webhook 回调没有 `qimen_context`**。入站推送的目标 Bot/群只能来自插件配置，不能从请求里推导。
2. **命令回调有 `qimen_context`**（在 `raw_event_json` 内），出站命令可用发送者的稳定 `account_id` 定位回发目标；管理员身份用 `sender_id`（字符串）。

---

## 4. 统一 Webhook 契约（核心）

为「统一标准」，入站与出站共用**同一套版本化信封 + 事件枚举 + 签名方案**。方向不同只是 `event` 语义与签名密钥不同。

### 4.1 统一信封

```json
{
  "version": 1,
  "event": "ticket.created",
  "id": "evt_01HW...",           // 事件幂等 ID，全链路去重
  "ts": 1700000000,              // 秒级时间戳，同时用于签名与重放窗口
  "ticket": {
    "id": "TK-20240101-0001",
    "subject": "…",
    "content": "…",
    "status": "open",
    "priority": "high",
    "customer":  { "id": "…", "name": "…", "contact": "…" },
    "assignee":  { "id": "…", "name": "…" },
    "created_at": "…",
    "updated_at": "…"
  },
  "changes": { "field": "status", "from": "open", "to": "closed" },
  "actor": {
    "id": "QQ字符串ID",            // 出站为管理员 QQ id；入站为魔方侧操作者
    "name": "张三",
    "title": "售后一组",
    "phone": "…",
    "email": "…",
    "signature": "张三 · 售后一组"
  },
  "content": "客户可见合并正文",     // 已按第 7 节客户上下文渲染，魔方可直接展示给客户
  "internal_note": "内部备注"       // 仅内部可见：管理员职称/联系方式等，不展示给客户
}
```

- `id`：全局唯一；插件用它做**幂等去重**（已处理事件丢弃，重复投递不重复推群）。
- `ts`：参与签名、用于重放窗口。
- `actor`：出站时由管理员档案填充（第 6 节），是**内部元数据**，可含 `phone/email`。
- `content`：**客户可见**合并正文（只含客户上下文允许的字段，见第 7 节）。
- `internal_note`：**内部可见**备注（职称、联系方式等），不进客户正文。
- `changes` / `actor` / `content` / `internal_note`：可选，按事件类型出现。
- 未识别 `version`：拒绝并返回 `400`，不猜测新字段含义。

### 4.2 事件枚举

**入站（魔方 → 插件）**：

| event | 含义 | 群推送动作 |
|---|---|---|
| `ticket.created` | 新建工单 | 推送「新工单」卡片 |
| `ticket.updated` | 通用更新 | 按 `changes` 摘要推送 |
| `ticket.replied` | 客户回复 | 推送「客户回复」 |
| `ticket.assigned` | 指派 / 被接单 | 推送「已接单」，@ 接单人 |
| `ticket.transferred` | 转移 | 推送「已转移」，@ 新处理人 |
| `ticket.closed` | 关闭 | 推送「已关闭」 |

**出站（插件 → 魔方）**：

| event | 触发命令 | 合并来源（第 7 节） |
|---|---|---|
| `ticket.assign` | `/ticket accept <id>` | 档案 + 工单 + 接单说明 |
| `ticket.transfer` | `/ticket transfer <id> @用户` | 档案 + 工单 + 目标用户 |
| `ticket.reply` | `/ticket reply <id> <内容>` | 档案 + 工单 + 回复正文 + 客户可见落款 |
| `ticket.close` | `/ticket close <id> [说明]` | 档案 + 工单 + 关闭说明 |

### 4.3 签名与安全头（双向同构）

| Header | 含义 |
|---|---|
| `X-Mofang-Timestamp` | Unix 秒 |
| `X-Mofang-Nonce` | 随机字符串，防重放 |
| `X-Mofang-Signature` | `hex( HMAC-SHA256(secret, "{ts}.{nonce}.{raw_body}") )` |

- **入站校验**（插件侧）：用配置的 `inbound.secret` 重算签名，`constant-time` 比较；`|now - ts| ≤ 300s`；`nonce` 在窗口内未见（有界 LRU）。任一失败返回 `401`/`400` 并记录。
- **出站签名**（插件侧）：用 `outbound.secret` 生成同样的三个头，交给魔方校验。
- 密钥只走 API 0.6 密钥通道（`writeOnly` + `x-qimen-secret`），绝不回传明文、不进日志。

### 4.4 路由与响应

- 插件导出 `POST /events`，完整地址 `{base_path}/{plugin_id}/events`，即默认 `/webhooks/mofang-ticket/events`。
- 同步 FFI 回调只做「验签 → 去重 → 入队/主动发送」，不做长任务。
- 响应约定：`200 {"ok":true,"id":"..."}` 表示已受理；验签失败 `401`；信封/时间戳/重放问题 `400`；body 超限由网关 `max_body_bytes` 拒绝。
- 预留 `GET /health`（可选）用于健康检查，返回 `200`，无需验签。

---

## 5. 工单投影模型与状态机

插件不持有权威状态，只维护**投影缓存**（供 `/ticket list` / 展示），由入站事件增量更新，键为 `ticket.id`：

```text
opened ──assigned──▶ processing ──replied(往返)──▶ resolved ──▶ closed
   │                     │
   └────transferred──────┘        （处理人变更，状态通常不变）
```

- 权威流转由魔方决定；插件仅按事件投影，不回写「我以为的状态」。
- `closed` 是终态：投影缓存可保留有限时长供查询，之后清理；`close` 由命令触发、经魔方确认后以入站 `ticket.closed` 回填。
- 投影缓存 key 命名空间：`{plugin_id}/{account_id}/{ticket.id}`，用 `account_id` 而非 `bot_instance` 分区（多 Bot 隔离）。
- 缓存为派生数据，可随时丢弃重建；重启后由后续入站事件自然补齐。Phase 2 可选 SQLite 持久化「工单 → 群消息 ID」映射以支持群内跟进。

---

## 6. 管理员档案（自助配置个人信息 + 字段策略）

管理员在群里自助维护自己的对外信息；出站事件（接单/转移/回复/关闭）自动携带并统一排版。**哪些字段可改、能展示给谁、能否进客户可见正文，全部由「字段策略」集中配置。**

### 6.1 档案模型

```json
{
  "schema_version": 1,
  "name": "张三",                // 对外称呼/真实姓名
  "title": "售后一组",           // 职称/部门
  "phone": "138…",             // 联系电话
  "email": "a@b.c",            // 邮箱
  "signature": "张三 · 售后一组", // 内部落款（群内展示/内部记录用）
  "updated_at": 1700000000
}
```

**身份键**：`{account_id}/{sender_id}`（QQ 字符串 ID）。跨协议用字符串 ID；同一管理员在不同 Bot 下档案独立（符合账号隔离原则）。

### 6.2 字段策略（可变更 + 展示范围）

每个档案字段由配置声明两项正交属性，插件据此在「写入」和「渲染」两处统一执法：

| 属性 | 取值 | 作用 |
|---|---|---|
| `editable` | `true` / `false` | 管理员本人能否通过 `profile set` 修改；`false` 表示锁定（仅 owner/配置可改） |
| `scope` | `none` / `internal` / `customer` | 字段能进入哪些渲染上下文（见 7.3） |

**`scope` 语义**：

| scope | 群内展示 | 出站 `actor` 元数据 | 客户可见正文/落款 |
|---|---|---|---|
| `none` | 否 | 否 | 否 |
| `internal` | 是 | 是 | **否** |
| `customer` | 是 | 是 | 是 |

**默认策略（示例「只给工单客户展示用户名」）**：

```toml
[admin_profiles.fields.name]
editable = true
scope = "customer"      # 用户名可进客户可见落款

[admin_profiles.fields.title]
editable = true
scope = "internal"      # 只内部可见，不进客户正文

[admin_profiles.fields.phone]
editable = true
scope = "internal"      # 只进出站 actor 元数据，绝不进客户正文

[admin_profiles.fields.email]
editable = true
scope = "internal"

[admin_profiles.fields.signature]
editable = true
scope = "internal"      # 内部落款，仅群内/内部记录
```

- 若想「客户也看到落款含职称」，把 `signature` 或 `title` 的 `scope` 改为 `customer` 即可，模板无需改。
- `editable = false` 的字段，`profile set` 被拒绝并提示「该字段由管理员锁定」。
- 模板若引用了其 `scope` 不允许的字段，`validate_config` 拒绝（编译期先挡一道，避免把手机号漏给客户）。

### 6.3 档案命令

根命令 `ticket` 下 `profile` 子命令（`role = "admin"`，`scope` 默认不限，建议 `group`）：

| 语法 | 作用 |
|---|---|
| `/ticket profile` | 查看自己档案（按字段策略回显，`none` 字段仅本人/owner 可见） |
| `/ticket profile set 字段=值 [字段=值 …]` | 增量更新（受 `editable` 限制） |
| `/ticket profile unset <字段>` | 清除单个字段（受 `editable` 限制） |
| `/ticket profile clear` | 清空档案 |

- 只有本人能改自己的档案（`sender_id` 匹配）；owner 可查看/清理他人档案。
- 解析用 `key=value`，`value` 含空格需整体用引号约定或「最后一个等号切分」；非法字段/超长值/被锁定字段返回明确提示，不 panic。
- 校验：`name` ≤ 32、`title` ≤ 32、`signature` ≤ 64、`phone`/`email` 基本格式校验；控制字符一律剥离。

### 6.4 存储与生命周期

- Phase 2：`{data_dir}/profiles/{account_id}/{sender_id}.json`，写入用「临时文件 + 原子 rename」；启动时惰性加载，命令按需读写。
- Phase 3：规模大时可迁 SQLite（`rusqlite` + `bundled`），schema 不变。
- 档案是**插件自有数据**，与 `config/plugins/*.toml`、`plugin-state.toml` 分离，不提交 Git。

---

## 7. 内容合并与排版规范（核心）

接单、转移、回复、关闭等操作必须把「管理员档案 + 工单上下文 + 命令输入」合并成**一套可预测、可配置、可审计**的规范正文。核心机制是**双渲染上下文**，把「给客户看的」和「给内部看的」彻底分开。

### 7.1 设计原则

1. **分层**：结构化字段（`actor`/`changes`）是权威、机器可读；`content` 是客户可见合并正文；`internal_note` 是内部备注。三者同源渲染，不各自硬编码。
2. **模板驱动 + 上下文隔离**：所有模板预先编排好（第 7.4 节总表），默认内置、可在 Schema 覆盖；每个模板绑定固定上下文，字段按 `scope` 过滤。
3. **幂等可审计**：同一命令、同一 `id` 生成的正文确定一致（时间除外），便于排查。

### 7.2 占位符全集

**工单**：`{ticket.id}` `{ticket.subject}` `{ticket.status}` `{ticket.priority}` `{ticket.customer.name}` `{ticket.customer.contact}` `{ticket.assignee.name}` `{ticket.created_at}` `{ticket.updated_at}`

**管理员（出站）**：`{admin.id}` `{admin.name}` `{admin.title}` `{admin.phone}` `{admin.email}` `{admin.signature}`

**动作**：`{actor.name}` `{actor.title}` `{target.name}` `{content}` `{time}` `{event}`

**回退链**：`{admin.signature}` 未配置时回退 `{admin.name}`，再回退 QQ 昵称，最后回退 `{admin.id}`；`{time}` 用部署时区格式 `YYYY-MM-DD HH:mm`。

### 7.3 渲染上下文（字段策略的执法点）

| 上下文 | 允许的字段 `scope` | 用途 |
|---|---|---|
| `customer` | 仅 `customer` | 客户可见正文 / 落款（`templates.reply.*`） |
| `internal` | `internal` 或 `customer` | 群推送、出站 `actor`、`internal_note`、接单/转移/关闭说明 |
| `self` | 全部 | 本人档案回显 |

- 渲染时字段 `scope` 不满足上下文 → 该占位符置空并告警；模板配置阶段 `validate_config` 直接拒绝越权引用。
- 这是「只给工单客户展示用户名」的机制保障：客户模板只能引用 `scope=customer` 的字段，手机号/邮箱天然进不了客户正文。

### 7.4 模板编排总表（预先设计）

| 模板键 | 上下文 | 用途 | 默认值 |
|---|---|---|---|
| `notify.templates.ticket.created` | internal | 群推送 | `新工单 {ticket.id}\n主题：{ticket.subject}\n客户：{ticket.customer.name}\n优先级：{ticket.priority}\n请及时接单` |
| `notify.templates.ticket.replied` | internal | 群推送 | `工单 {ticket.id} 有新回复\n主题：{ticket.subject}\n客户：{ticket.customer.name}` |
| `notify.templates.ticket.assigned` | internal | 群推送 | `工单 {ticket.id} 已接单\n处理人：{ticket.assignee.name}` |
| `notify.templates.ticket.transferred` | internal | 群推送 | `工单 {ticket.id} 已转移\n现处理人：{ticket.assignee.name}` |
| `notify.templates.ticket.closed` | internal | 群推送 | `工单 {ticket.id} 已关闭\n主题：{ticket.subject}` |
| `notify.templates.ticket.updated` | internal | 群推送 | `工单 {ticket.id} 更新：{changes.field}` |
| `templates.reply.body` | customer | 客户可见回复正文 | `{content}` |
| `templates.reply.signature` | customer | 客户可见落款 | `—— {admin.name}` |
| `templates.reply.internal_note` | internal | 内部备注（进 `internal_note`） | `{admin.name} · {admin.title}` |
| `templates.assign_note` | internal | 接单说明 | `由 {admin.name}（{admin.title}）接单：{ticket.id} {ticket.subject}` |
| `templates.transfer_note` | internal | 转移说明 | `由 {admin.name} 把工单 {ticket.id} 转移给 {target.name}` |
| `templates.close_note` | internal | 关闭说明 | `由 {admin.name}（{admin.title}）关闭：{ticket.id} {ticket.subject}` |

- 客户可见正文与落款分开成两个模板：`body` 负责承载正文，`signature` 负责承载署名，便于独立开关/覆盖。
- `internal_note` 可配置为空串以省略；其内容与 `actor` 一样只进内部侧。

### 7.5 出站事件合并规则

| 事件 | `content`（customer 上下文） | `internal_note`（internal 上下文） | 结构化补充 |
|---|---|---|---|
| `ticket.reply` | `body` + `"\n\n"` + `signature` | `internal_note` 模板 | `actor`=内部档案 |
| `ticket.assign` | （可空，默认不设） | `assign_note` 模板 | `changes.field="assignee"`，`changes.to`=档案 |
| `ticket.transfer` | （可空） | `transfer_note` 模板 | `changes.field="assignee"`，`changes.to`=`{target.id,target.name}` |
| `ticket.close` | （可空） | `close_note` 模板 | `changes.field="status"`，`changes.to="closed"` |

- **reply 双字段策略**：客户只看到 `content`（由 `body`+`signature` 合并，且仅含 `scope=customer` 字段）；魔方内部拿到 `actor` + `internal_note`（含职称/联系方式）。落款是否最终展示给客户由魔方决定，插件保证「客户正文里只有被允许的字段」。
- `transfer` 的 `target` 来自命令里的 @ 目标：QQ 字符串 ID + 群内昵称；到魔方用户的解析规则见第 17 节待确认项。
- 接单/转移/关闭默认不写客户可见正文，避免把内部动作措辞透给客户；如需透出可后续加 `customer` 上下文模板。

### 7.6 群推送排版（入站 → 群，internal 上下文）

按 `notify.templates.*` 套用文本模板，`@` 一律用 `at` 段（不用文本 `@`），正文用 `text` 段 + 换行；`ticket.assigned` / `ticket.transferred` 额外 `at(assignee)`。模板可经 `notify.templates` 覆盖（见第 11 节）。

### 7.7 转义 / 长度 / 安全约束

- 渲染前：剥离控制字符（`\x00-\x1f` 除 `\n`）；`subject` 截断 60、`content` 截断 2000、`name/title/signature` 按档案上限；截断追加 `…`。
- **占位符不二次展开**：字段值内的 `{...}` 原样输出，防注入伪造占位符。
- 未知占位符渲染为空串并告警，不输出原始 `{...}` 文本。
- **上下文越权引用**：`customer` 模板引用 `internal`/`none` 字段 → `validate_config` 拒绝；运行期再兜底置空。
- `phone`/`email` 只进出站 `actor`/`internal_note` 与私聊回执，不进群广播、不进客户正文、不进日志。
- 官方 QQ 场景不依赖 Markdown/HTML（`<br>`/`<font>` 等），排版以换行 + `text`/`at` 段为基线；富文本增强落地后实测。

---

## 8. 功能设计

### 8.1 入站：工单推送（Phase 1 落地范围）

1. 网关收到 POST → 验签 → 信封校验 → 幂等去重。
2. 事件规范化：把信封映射为「通用消息段」。
3. 按 `notify.targets` 中匹配 `events`/`priority` 的目标，用 `BotApi::for_account(account_id).send_group_msg(group_id, text)` 或 `SendBuilder::group(...).bot_account(...).text(...).at(...).try_send()` 主动推送（按第 7.6 节模板渲染）。
4. 检查 `SendEnqueueStatus`：`Accepted` 为成功；`HostUnavailable/BotNotFound/BotDisabled/QueueFull/HostShuttingDown` 记录并有限次退避重试，不压满队列。

### 8.2 出站：人工介入（Phase 2 落地范围）

1. 命令回调解析参数 → 读取管理员档案 + 字段策略（第 6 节）→ 按第 7.5 节合并 → 构造出站事件信封。
2. 入队到有界 mpsc；回调立即用 `CommandResponse::text` 回「已受理」。
3. worker 用同步 HTTP 客户端签名并 POST 到 `outbound.url`，超时 + 有限重试 + 指数退避。
4. 结果回执：成功/失败用 `BotApi::for_account` 主动发回来源群（含工单号、客户可见正文预览与错误摘要；预览中不含 `internal_note` 敏感字段）。

**为什么不能直接 HTTP**：动态回调是同步 FFI，阻塞会拖垮消息处理并吃熔断预算。outbound 只能跑在受控 worker 线程（`#[init]` 启动、`#[shutdown]` 停止并 join）。同步 HTTP 客户端建议 `ureq`（rustls），避免把 Tokio runtime 拉进 cdylib。

---

## 9. 命令清单与参数规范

统一入口：根命令 `ticket`（aliases `工单,tk`），`role = "admin"`，`scope = "group"`，第一个参数为子命令。

| 子命令 | 语法 | 出站事件 | 说明 |
|---|---|---|---|
| `list` | `/ticket list [状态]` | — | 读投影缓存，列出待处理工单 |
| `detail` | `/ticket detail <id>` | — | 单条工单详情 |
| `accept` | `/ticket accept <id>` | `ticket.assign` | 接单，合并档案 |
| `transfer` | `/ticket transfer <id> @目标` | `ticket.transfer` | 转移给 @ 的用户 |
| `reply` | `/ticket reply <id> <正文…>` | `ticket.reply` | 合并客户可见正文 + 落款后推送 |
| `close` | `/ticket close <id> [说明]` | `ticket.close` | 关闭工单（可附说明，进 `close_note`） |
| `profile` | `/ticket profile [set\|unset\|clear …]` | — | 管理本人档案（受字段策略） |

解析规则：

- `args` 是空格拼接字符串；子命令 = 第 1 个 token，`<id>` = 第 2 个 token，**剩余全部**作为 `reply`/`close` 说明正文（不做二次切分）。
- `profile set` 用 `key=value`；`value` 含空格时以「该 key 后到下一个 key 或行尾」切分（实现期细化，需覆盖中文与空格场景）。
- 参数缺失/非法：返回用法提示，不 `unwrap`，不 panic。
- 权限由宿主 `role = "admin"` 门禁；`transfer`/`close` 建议 `role = "owner"` 或独立开关（见第 11 节）。

---

## 10. 跨协议兼容（OneBot 11 + 官方 QQ Bot）

硬约束（违反会直接踩坑）：

1. **所有 ID 用字符串**。官方 QQ 是 OpenID / `group_openid` / 频道 / guild / 字符串消息 ID，不能转 `i64`。`CommandRequest.group_id` 在 C2C/频道/DMS 下为空字符串，不能当「官方 QQ 解码失败」。
2. **主动发送按稳定账号选 Bot**：`BotApi::for_account(account_id)` 优先，`account_id` 来自配置（入站）或 `qimen_context`（命令）。不用会变的 `bot_instance` 当账号主键。
3. **被动回复走 `CommandResponse`**：运行时按来信场景自动路由到群/C2C/频道/DMS，插件不猜目标类型、不删平台 @ 标签、不硬编码 `/` 前缀。
4. **群推送目标要按协议分别配置**：OneBot 群号与官方 QQ `group_openid` 是两套字符串，`notify.targets` 用 `account_id + group_id` 成对描述，避免把 OneBot 群号发给官方 QQ Bot。
5. **富文本降级**：正文以 `text`/`at` 段为基线；官方 QQ 的 Markdown/Keyboard/Ark/Embed 属平台扩展，`<br>`/`<font>` 等 HTML 标签不按浏览器行为推断，落地后需在真实群聊场景实测。
6. **媒体**：本地图片等用 Base64 通用段交宿主上传；插件不读取 Bot 凭据、不自实现分片上传、不把 Base64 写日志。
7. **官方 QQ 前置依赖**：群消息/群 @ 需对应 Intent 与平台权限（`GROUP_MESSAGE_CREATE` / `GROUP_AT_MESSAGE_CREATE`），插件代码无法凭空让消息进入宿主。
8. **管理员档案键**：`sender_id` 在官方 QQ 是 OpenID，跨 Bot 不保证相同；档案按 `account_id` 分区即天然隔离。

---

## 11. 配置设计（API 0.6 Schema 草案）

配置来自 `config/plugins/mofang-ticket.toml`（宿主转 JSON 传 `init`），在线面板由 Schema 渲染。`config_apply = "reload"`（因 outbound worker 需重建，reload 的 shutdown/init 天然覆盖线程重启）。

```toml
# config/plugins/mofang-ticket.toml（语义示意，字段以最终 Schema 为准）

[inbound]
enabled = true
secret = "<入站 HMAC 密钥>"          # writeOnly + x-qimen-secret
timestamp_tolerance_secs = 300
nonce_cache_size = 4096

[notify]
# 每个目标：稳定账号 + 协议原生群号，成对出现；可加 events/priority 过滤
targets = [
  { account_id = "2733944636", group_id = "123456789", events = ["ticket.created", "ticket.replied"] },
]

[notify.templates]                   # 群推送排版覆盖（可选，缺省用内置模板）
"ticket.created" = "新工单 {ticket.id}\n主题：{ticket.subject}\n客户：{ticket.customer.name}"

[templates]                          # 出站合并模板覆盖（可选）
reply.body = "{content}"
reply.signature = "—— {admin.name}"
reply.internal_note = "{admin.name} · {admin.title}"
assign_note = "由 {admin.name}（{admin.title}）接单：{ticket.id} {ticket.subject}"
transfer_note = "由 {admin.name} 把工单 {ticket.id} 转移给 {target.name}"
close_note = "由 {admin.name}（{admin.title}）关闭：{ticket.id} {ticket.subject}"

[outbound]
url = "https://mofang.example.com/webhook/ticket"
secret = "<出站 HMAC 密钥>"           # writeOnly + x-qimen-secret
timeout_secs = 10
max_retries = 3
backoff_secs = 2

[admin_profiles]
enabled = true
allow_self_edit = true               # 管理员可否改自己的档案（总开关）

[admin_profiles.fields.name]         # 字段策略：editable + scope
editable = true
scope = "customer"
[admin_profiles.fields.title]
editable = true
scope = "internal"
[admin_profiles.fields.phone]
editable = true
scope = "internal"
[admin_profiles.fields.email]
editable = true
scope = "internal"
[admin_profiles.fields.signature]
editable = true
scope = "internal"

[commands]
transfer_requires_owner = false
close_enabled = true
close_requires_owner = true

[storage]
enabled = false                      # Phase 3：投影/映射持久化（默认用 data_dir）
```

Schema 要点（按 online-configuration.md 规范）：

- 根节点显式 `type: "object"`；`additionalProperties: false`；本地 `$ref`；单文件 ≤ 256 KiB。
- 密钥字段统一 `writeOnly: true` + `x-qimen-secret: true`（或 `format: "password"`），GET 不回传明文，保存走 `secret_updates` 通道。
- `account_id` / `bot_id` 用 `oneOf` 互斥（参考模板 `background_push` 的写法），`inbound.enabled` 为 true 时二者必填其一。
- `notify.targets` 用对象数组 + `itemTitle`/`itemLabel`，`events` 用枚举多选；`notify.templates` / `templates` 用 `textarea` 控件展示占位符模板。
- `admin_profiles.fields` 用对象枚举 `scope ∈ {none, internal, customer}` 的 `select` 控件 + `editable` 开关；模板字段带 `placeholder` 提示占位符表。
- `validate_config` 只做业务校验（不启动线程、不做长请求），并**校验模板上下文越权引用**：`customer` 模板只能引用 `scope=customer` 字段。
- `init` 必须为「无配置文件 / 旧配置」提供默认值（含默认字段策略与模板）。首次安装无配置文件时以 `inbound.enabled = false` 的**禁用态**成功加载（安全默认，参考 ai-news 模式），空配置/缺 `secret`（或不足 16 字符）不阻断 init；`enabled = true` 但 `secret` 未配置或不足 16 字符时插件仍加载，Webhook 返回 503 提示，避免安装事务“初始化失败”回滚。

---

## 12. 生命周期与线程模型

```text
扫描 plugin_bin_dir → 读描述符(API 0.6) → 读 config → #[init] → 注册命令/Webhook
```

- `#[init]`：解析配置 → 校验 → 启动 outbound worker（含停止信号 + `JoinHandle`，Phase 2）→ 初始化投影缓存、nonce LRU、档案目录、字段策略与模板引擎。必须可重复调用（reload 语义）。
- `#[shutdown]`：置停止位 → `unpark` → `join` worker → 释放缓存/文件。**动态库卸载后仍在跑的线程会调用失效代码导致崩溃，这是硬边界。**
- Webhook / 命令回调：同步、短小、不 `async`、不阻塞；长任务一律入队。
- 状态键 = `{plugin_id}/{account_id}/{string_id}`，不绑定可变的 `bot_instance`。

---

## 13. 安全设计

- 入站：HMAC 验签 + 时间戳窗口 + nonce 去重 + `id` 幂等（三重防线）。
- 出站：同构签名，远端可验。
- 密钥：只走 API 0.6 密钥通道；不进默认值、日志、README、错误信息。
- 管理员档案：`phone`/`email` 只进出站 `actor`/`internal_note` 与私聊回执，不进群广播/客户正文/日志；他人档案的敏感字段仅本人/owner 可见。
- 字段策略是数据最小化的执法点：`customer` 上下文只放行 `scope=customer` 字段，从机制上防止个人信息外泄给客户。
- 网关层：`[official_host.webhook]` 生产必开 `access_token`、回环监听 + 反向代理 TLS；`max_body_bytes`、`request_timeout_ms`、`max_in_flight` 设上界。
- 动态回调受 `dynamic_plugin_timeout_secs` 保护；`pre_handle`（若用）失败 fail-open，不能当唯一鉴权边界。
- 模板渲染：占位符不二次展开、字段值转义，防注入；不记录 `raw_event_json` 全文、OpenID、媒体 URL、签名密钥到日志（仅受控 `qimen_raw_message=debug` 短开）。

---

## 14. 分阶段落地计划

| 阶段 | 内容 | 依赖 |
|---|---|---|
| **Phase 1（本次落地目标）** | 统一信封 + `POST /events` 入站验签/去重 + 规范化 + 群推送（`notify.targets` + 内置排版模板）+ 最小 Schema | API 0.6、Webhook Gateway |
| Phase 2a | 管理员档案（`profile` 子命令 + 文件存储 + 字段策略） | Phase 1 |
| Phase 2b | 命令 `list/detail/accept/transfer/reply/close` + 出站合并模板（客户/内部双上下文）+ outbound worker + 出站签名 + 回执 | Phase 2a + outbound.url |
| Phase 3 | 投影/映射持久化、模板在线自定义完善、富文本/卡片增强、商城发布 | Phase 2 |

> 逐条可勾选、带依赖与验收锚点的实现清单见 [TODO.md](TODO.md)，实现期间以它为准勾选推进。

---

## 15. 构建与部署

```bash
cargo new --lib qimen-dynamic-plugin-mofang-ticket
# Cargo.toml: crate-type=["cdylib"]，依赖 abi-stable-host-api 0.1.13 + qimen-dynamic-plugin-derive 0.1.13
cargo fmt --check && cargo check --locked && cargo test --locked
cargo build --release
# 产物复制到宿主 plugin_bin_dir（默认 plugins/bin/），Web 插件页“重新扫描”
```

- target 必须匹配宿主：Windows `x86_64-pc-windows-msvc`；Linux GNU `x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu`；**musl 宿主不支持动态加载**。
- 宿主需启用 `[official_host.webhook]`，插件配置放 `config/plugins/mofang-ticket.toml`。
- 管理员档案放 `data_dir/profiles/`，与配置、状态文件分离。
- 不提交 `plugins/bin/`、`config/plugins/*.toml`、档案、数据库、日志。

---

## 16. 验收清单（落地后逐项核对）

- [ ] 插件类型/ID/ABI/crate 版本已说明，未混用 `0.1.12` 或浮动分支。
- [ ] `POST /events` 全链路：验签通过/失败、时间戳越界、nonce 重放、重复 `id` 幂等。
- [ ] 每种 `event` 都在配置目标群按第 7.4/7.6 节模板正确推送，`SendEnqueueStatus` 各失败态有处理。
- [ ] 管理员档案：本人 set/unset/clear 生效；`editable=false` 字段被拒；他人不可改；重启后保留。
- [ ] 字段策略：`scope=none/internal/customer` 在群展示、出站 `actor`、客户正文三处一致执法；`customer` 模板引用 `internal` 字段被 `validate_config` 拒绝。
- [ ] 出站合并：reply 的 `content`（客户可见）只含 `scope=customer` 字段、`internal_note`/`actor` 含内部字段；assign/transfer/close 的 `changes` 与说明符合第 7.5 节；占位符回退链正确；无占位符注入。
- [ ] `close` 操作：`close_enabled`/`close_requires_owner` 生效；关闭说明并入 `close_note`；魔方确认后入站 `ticket.closed` 正确回填投影。
- [ ] 命令 `list/detail/accept/transfer/reply/close/profile` 参数缺失/非法不 panic；权限与作用域正确。
- [ ] 出站事件信封、签名、重试退避、回执正确；魔方远端可达性验证。
- [ ] OneBot（数字字符串 ID）与官方 QQ（OpenID/group_openid）分别实测；C2C/频道空 `group_id` 不误判。
- [ ] 密钥无明文回传/日志泄露；Schema 根类型、默认值、旧配置兼容、revision 冲突通过。
- [ ] 热重载：shutdown 停止并 join worker，无悬空线程/崩溃。

---

## 17. 待确认事项（实现前需拍板）

1. 魔方系统入站推送的**原始字段格式**（届时做「原始 → 统一信封」的适配层，不影响本文档契约）。
2. 管理员/接单人身份在魔方侧的映射：是否直接用 QQ 字符串 ID 作为 `actor.id` / `assignee.id`。
3. `transfer` 目标「@ 用户」到魔方用户的解析规则；档案中的 `phone`/`email` 是否作为魔方侧匹配辅助。
4. 群推送目标是否按工单分类/优先级路由到多个群（当前按 `notify.targets` 的 `events/priority` 过滤支持）。
5. 客户可见落款默认只含 `{admin.name}`；是否需要在某些工单场景给客户展示职称（届时把 `title` 的 `scope` 调成 `customer` 即可，模板无需改）。

---

## 18. 示例（端到端）

> 本节用一个贯穿场景把前文规则串起来，便于实现时对照。所有示例为设计期示意值。

### 18.0 贯穿场景假设

- 工单：`TK-20240101-0001`《账单支付失败》，客户「王五」，优先级 `high`
- 管理员「张三」：QQ 字符串 ID `10001`，档案如下
- 字段策略：`name → customer`，`title / phone / email / signature → internal`（即「只给客户展示用户名」）

```json
// 张三的档案：{data_dir}/profiles/{account_id}/10001.json
{
  "schema_version": 1,
  "name": "张三",
  "title": "售后一组",
  "phone": "13800000000",
  "email": "zhangsan@example.com",
  "signature": "张三 · 售后一组",
  "updated_at": 1700000000
}
```

### 18.1 入站事件信封示例

**新建工单 `ticket.created`**：

```json
{
  "version": 1,
  "event": "ticket.created",
  "id": "evt_01HW0001",
  "ts": 1700000000,
  "ticket": {
    "id": "TK-20240101-0001",
    "subject": "账单支付失败",
    "content": "客户反馈订单 20240101-88 支付后未到账",
    "status": "open",
    "priority": "high",
    "customer": { "id": "cus_100", "name": "王五", "contact": "wangwu@example.com" },
    "assignee": null,
    "created_at": "2024-01-01T12:00:00Z",
    "updated_at": "2024-01-01T12:00:00Z"
  }
}
```

**接单确认 `ticket.assigned`**（魔方把「指派给张三」回推）：

```json
{
  "version": 1,
  "event": "ticket.assigned",
  "id": "evt_01HW0011",
  "ts": 1700000120,
  "ticket": {
    "id": "TK-20240101-0001",
    "subject": "账单支付失败",
    "status": "processing",
    "priority": "high",
    "customer": { "id": "cus_100", "name": "王五", "contact": "wangwu@example.com" },
    "assignee": { "id": "10001", "name": "张三" },
    "updated_at": "2024-01-01T12:02:00Z"
  },
  "changes": { "field": "assignee", "from": null, "to": { "id": "10001", "name": "张三" } }
}
```

**关闭确认 `ticket.closed`**：

```json
{
  "version": 1,
  "event": "ticket.closed",
  "id": "evt_01HW0099",
  "ts": 1700000900,
  "ticket": {
    "id": "TK-20240101-0001",
    "subject": "账单支付失败",
    "status": "closed",
    "priority": "high",
    "customer": { "id": "cus_100", "name": "王五", "contact": "wangwu@example.com" },
    "assignee": { "id": "10001", "name": "张三" },
    "updated_at": "2024-01-01T12:15:00Z"
  },
  "changes": { "field": "status", "from": "processing", "to": "closed" }
}
```

### 18.2 签名计算示例（可复算）

设：

- 密钥 `secret = mofang-demo-secret-0123456789`
- `X-Mofang-Timestamp = 1700000000`
- `X-Mofang-Nonce = n1a2b3c4d5`
- 原始 body（UTF-8，按收到字节原样）：

```text
{"version":1,"event":"ticket.created","id":"evt_01HW0001","ts":1700000000,"ticket":{"id":"TK-20240101-0001"}}
```

规范串 = `{ts}.{nonce}.{raw_body}`：

```text
1700000000.n1a2b3c4d5.{"version":1,"event":"ticket.created","id":"evt_01HW0001","ts":1700000000,"ticket":{"id":"TK-20240101-0001"}}
```

签名 = `hex( HMAC-SHA256(secret, 规范串) )`，可复算结果为：

```text
X-Mofang-Signature: f99c067409979d168cdf84fc217d525341533c017bd4c8d7a3f3b4ba30cbb9f8
```

实现要点：对**收到的原始 body 字节**做 HMAC，不要先 JSON 反序列化再重排键序；密钥与 body 不得写入日志。

### 18.3 出站事件信封示例（人工介入）

**回复 `ticket.reply`**（命令 `/ticket reply TK-20240101-0001 您好，已为您加急处理，预计 2 小时内到账`）：

```json
{
  "version": 1,
  "event": "ticket.reply",
  "id": "evt_01HW0050",
  "ts": 1700000300,
  "ticket": { "id": "TK-20240101-0001", "subject": "账单支付失败" },
  "actor": {
    "id": "10001",
    "name": "张三",
    "title": "售后一组",
    "phone": "13800000000",
    "email": "zhangsan@example.com",
    "signature": "张三 · 售后一组"
  },
  "content": "您好，已为您加急处理，预计 2 小时内到账\n\n—— 张三",
  "internal_note": "张三 · 售后一组"
}
```

> 关键点：`content`（客户可见）里只有 `name=张三`；`phone/email/title` 只出现在 `actor` 与 `internal_note`，没有泄露给客户。

**接单 `ticket.assign`**：

```json
{
  "version": 1,
  "event": "ticket.assign",
  "id": "evt_01HW0060",
  "ts": 1700000400,
  "ticket": { "id": "TK-20240101-0001", "subject": "账单支付失败" },
  "actor": { "id": "10001", "name": "张三", "title": "售后一组", "phone": "13800000000" },
  "internal_note": "由 张三（售后一组）接单：TK-20240101-0001 账单支付失败",
  "changes": { "field": "assignee", "from": null, "to": { "id": "10001", "name": "张三" } }
}
```

**转移 `ticket.transfer`**（`/ticket transfer TK-20240101-0001 @李四`）：

```json
{
  "version": 1,
  "event": "ticket.transfer",
  "id": "evt_01HW0070",
  "ts": 1700000500,
  "ticket": { "id": "TK-20240101-0001", "subject": "账单支付失败" },
  "actor": { "id": "10001", "name": "张三", "title": "售后一组" },
  "internal_note": "由 张三 把工单 TK-20240101-0001 转移给 李四",
  "changes": { "field": "assignee", "from": { "id": "10001", "name": "张三" }, "to": { "id": "10002", "name": "李四" } }
}
```

**关闭 `ticket.close`**（`/ticket close TK-20240101-0001 问题已解决`）：

```json
{
  "version": 1,
  "event": "ticket.close",
  "id": "evt_01HW0080",
  "ts": 1700000600,
  "ticket": { "id": "TK-20240101-0001", "subject": "账单支付失败" },
  "actor": { "id": "10001", "name": "张三", "title": "售后一组" },
  "internal_note": "由 张三（售后一组）关闭：TK-20240101-0001 账单支付失败（问题已解决）",
  "changes": { "field": "status", "from": "processing", "to": "closed" }
}
```

### 18.4 模板渲染走查（客户 vs 内部）

沿用 18.0 的档案与字段策略，模板：

```text
templates.reply.body          = "{content}"
templates.reply.signature     = "—— {admin.name}"
templates.reply.internal_note = "{admin.name} · {admin.title}"
```

命令输入正文 `content = 您好，已为您加急处理`，渲染结果：

| 输出 | 上下文 | 结果 |
|---|---|---|
| 客户可见正文 | customer | `您好，已为您加急处理\n\n—— 张三` |
| 内部备注 | internal | `张三 · 售后一组` |
| `actor` 元数据 | internal | 含 `name/title/phone/email/signature` 全量（见 18.3） |

**变体**：若把 `title` 的 `scope` 改为 `customer`，并把 `signature` 模板改为 `—— {admin.name}（{admin.title}）`，客户可见正文变为：

```text
您好，已为您加急处理

—— 张三（售后一组）
```

而 `phone/email` 仍因 `scope=internal` 进不了客户正文。

### 18.5 群推送渲染示例

`ticket.created` 入站（`notify.templates.ticket.created` 默认模板），按 `account_id + group_id` 目标主动发送：

- **文本段**（`text`）：

```text
新工单 TK-20240101-0001
主题：账单支付失败
客户：王五
优先级：high
请及时接单
```

- **@ 段**（`at`）：`at_all()` 或 `at(值班管理员ID)`，视配置。

`ticket.assigned` 入站：

- 文本段：`工单 TK-20240101-0001 已接单\n处理人：张三`
- @ 段：`at("10001")`（接单人 QQ 字符串 ID）

跨协议提醒：`group_id` 在 OneBot 是群号字符串（如 `"123456789"`），官方 QQ 是 `group_openid`（如 `"ABC...XYZ"`）；`at` 的 `qq` 值在 OneBot 是数字字符串、官方 QQ 是 OpenID。两者都按字符串传给 `SendBuilder`，不转数字。

### 18.6 命令交互实录（群聊）

```text
[管理员] /ticket list
[Bot]    待处理 2 单：
         ① TK-20240101-0001 账单支付失败 · high · 未指派
         ② TK-20240101-0002 发票抬头错误 · normal · 未指派

[管理员] /ticket accept TK-20240101-0001
[Bot]    ⏳ 已受理接单 TK-20240101-0001，正在同步…
[Bot]    ✅ 接单成功：TK-20240101-0001（张三 · 售后一组）

[管理员] /ticket reply TK-20240101-0001 您好，已为您加急处理，预计 2 小时内到账
[Bot]    ⏳ 已受理回复 TK-20240101-0001，正在同步…
[Bot]    ✅ 已回复 TK-20240101-0001：
         您好，已为您加急处理，预计 2 小时内到账
         —— 张三

[管理员] /ticket transfer TK-20240101-0001 @李四
[Bot]    ✅ 已转移 TK-20240101-0001 给 李四

[管理员] /ticket close TK-20240101-0001 问题已解决
[Bot]    ✅ 已关闭 TK-20240101-0001

[管理员] /ticket profile
[Bot]    您的档案：
         姓名：张三
         职称：售后一组（内部可见）
         电话：13800000000（仅出站/私聊可见）
         邮箱：zhangsan@example.com（仅出站/私聊可见）

[管理员] /ticket profile set phone=13900000000 title=售后二组
[Bot]    ✅ 档案已更新：title、phone

[管理员] /ticket profile set name=张三丰
[Bot]    ✅ 档案已更新：name
```

> 回执里的「客户可见正文预览」不显示 `phone/email`。命令回执的 `⏳` 由 `CommandResponse::text` 即时返回，最终 `✅` 结果由 outbound worker 完成后经 `BotApi::for_account` 主动发回。
