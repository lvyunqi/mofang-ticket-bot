# mofang-ticket

对接魔方财务系统工单的 [QimenBot](https://github.com/lvyunqi/QimenBot) 动态插件。

它让 QimenBot 成为「魔方财务系统工单」与 QQ 群之间的双向消息网关：接收魔方系统推送的工单事件，鉴权、去重后主动推送到指定群。当前阶段先打通 **Webhook 接收 → 鉴权 → 主动群推送** 一条链路，接单 / 转移 / 回复等人工介入能力按路线图逐步补齐。

- 插件 ID：`mofang-ticket`
- 动态 ABI API：`0.6`
- 兼容协议：OneBot 11 与官方 QQ Bot

## 当前能力（Phase 1）

已实现并通过单元测试：

- 统一信封校验（`version` / `event` / `id` / `ts`）
- Webhook 鉴权：HMAC-SHA256 签名 + 时间戳容差窗口 + nonce 防重放
- 幂等去重：按 `event.id` 去重，重复投递返回 `dup:true`
- 群推送：把原始 body 原文透传到配置的 `notify.targets` 群

尚未实现（见 [TODO.md](TODO.md)）：模板化排版、管理员档案、接单/转移/回复命令、出站推送、持久化等。

## 技术基线

| 项 | 取值 |
|---|---|
| QimenBot 宿主 | `v0.1.18` 及以上 |
| `abi-stable-host-api` | `0.1.13` |
| `qimen-dynamic-plugin-derive` | `0.1.13` |
| Rust | edition 2024，rust-version `1.89` |

> 两个配套 crate 必须同版本；不要回退 `0.1.12`，也不要依赖浮动 Git 分支或本地 path。

## 目录结构

```text
.
├── Cargo.toml            # cdylib 依赖声明
├── config.schema.json    # API 0.6 配置 Schema
├── config.ui.json        # API 0.6 配置 UI Schema
├── src/lib.rs            # 插件实现（信封/鉴权/去重/推送）
├── DESIGN.md             # 完整设计规范
├── TODO.md               # 实现清单
└── LICENSE               # MIT
```

## 构建

```bash
cargo fmt --check
cargo test
cargo build --release
```

产物（以 Windows x64 为例）：`target/release/qimen_dynamic_plugin_mofang_ticket.dll`。

产物必须匹配宿主的 OS / CPU / C 运行时；Linux 用 `x86_64-unknown-linux-gnu` 或 `aarch64-unknown-linux-gnu`，musl 宿主不支持动态加载。

## 部署与加载

1. 复制产物到宿主 `plugin_bin_dir`（默认 `plugins/bin/`）。
2. 在宿主配置启用 Webhook Gateway：

   ```toml
   [official_host.webhook]
   enabled = true
   bind = "127.0.0.1:8088"     # 生产建议回环监听 + 反向代理 TLS
   base_path = "/webhooks"
   access_token = "..."        # 网关 Bearer token
   ```

3. 在 Web 插件页「重新扫描」；对外路由为：

   ```text
   POST /webhooks/mofang-ticket/events
   ```

## 配置

配置文件：`config/plugins/mofang-ticket.toml`（宿主转 JSON 传给插件）。

```toml
[inbound]
enabled = true
secret = "mofang-demo-secret-0123456789"   # 与魔方侧约定的 HMAC 密钥（启用时必填，≥16 字符）
timestamp_tolerance_secs = 300             # 时间戳容差窗口（秒）
nonce_cache_size = 4096                    # nonce/event.id 缓存容量

[[notify.targets]]
account_id = "2733944636"   # 宿主 [[bots]].account_id；OneBot 通常为 self_id
group_id = "123456789"      # 协议原生群号：OneBot 为数字字符串，官方 QQ 为 group_openid

# 可配置多个目标群
# [[notify.targets]]
# account_id = "..."
# group_id = "..."
```

> `secret` 在 Web 管理面板按 `writeOnly` 密钥处理，不回传明文；但手工 TOML 里是明文，注意不要提交到 Git（`config/plugins/` 已在 `.gitignore`）。

> **首次安装安全默认**：没有 `config/plugins/mofang-ticket.toml` 时，插件以**禁用态**加载（`inbound.enabled = false`），初始化不会失败。请先填写 `inbound.secret`（与魔方侧约定的 HMAC 密钥，**至少 16 字符**）和 `notify.targets`，再把 `enabled` 改为 `true` 并重载。若手工配置为 `enabled = true` 但 secret 未配置或不足 16 字符，插件仍会加载，但 Webhook 会返回 `503` 提示。

## Webhook 契约

### 信封

```json
{
  "version": 1,
  "event": "ticket.created",
  "id": "evt_01HW0001",
  "ts": 1700000000,
  "ticket": {
    "id": "TK-20240101-0001",
    "subject": "账单支付失败",
    "status": "open",
    "priority": "high"
  }
}
```

- `version`：当前只接受 `1`，未知版本返回 400。
- `id`：事件幂等 ID，插件据此去重。
- `event`：`ticket.created` / `ticket.updated` / `ticket.replied` / `ticket.assigned` / `ticket.transferred` / `ticket.closed`。

### 鉴权头

| Header | 含义 |
|---|---|
| `X-Mofang-Timestamp` | Unix 秒 |
| `X-Mofang-Nonce` | 随机字符串，防重放 |
| `X-Mofang-Signature` | `hex( HMAC-SHA256(secret, "{ts}.{nonce}.{raw_body}") )` |

签名对**原始 body 字节**计算，`{ts}` 用 header 里的原始字符串。

### 响应码

| 状态码 | 含义 |
|---|---|
| `200` | 已受理；重复投递时带 `"dup": true` |
| `400` | 信封非法 / 时间戳越界 / 签名非 hex |
| `401` | 鉴权失败（缺头 / 签名不匹配 / nonce 重放） |
| `503` | `inbound.enabled = false`，或 `enabled = true` 但 `inbound.secret` 未配置或不足 16 字符 |

## 示例

### 签名计算（可复算）

设 `secret = mofang-demo-secret-0123456789`、`ts = 1700000000`、`nonce = n1a2b3c4d5`，原始 body：

```text
{"version":1,"event":"ticket.created","id":"evt_01HW0001","ts":1700000000,"ticket":{"id":"TK-20240101-0001"}}
```

规范串 = `{ts}.{nonce}.{raw_body}`，HMAC-SHA256 后 hex 编码：

```text
X-Mofang-Signature: f99c067409979d168cdf84fc217d525341533c017bd4c8d7a3f3b4ba30cbb9f8
```

### curl 推送

```bash
curl -X POST 'http://127.0.0.1:8088/webhooks/mofang-ticket/events' \
  -H 'Content-Type: application/json' \
  -H 'X-Mofang-Timestamp: 1700000000' \
  -H 'X-Mofang-Nonce: n1a2b3c4d5' \
  -H 'X-Mofang-Signature: f99c067409979d168cdf84fc217d525341533c017bd4c8d7a3f3b4ba30cbb9f8' \
  -d '{"version":1,"event":"ticket.created","id":"evt_01HW0001","ts":1700000000,"ticket":{"id":"TK-20240101-0001"}}'
```

> 示例里的 `ts` 是固定值，真实请求需用当前时间（与接收方时间差在 `timestamp_tolerance_secs` 内），并据此重新计算签名。

## 路线图

完整设计与实现清单见 [DESIGN.md](DESIGN.md) 与 [TODO.md](TODO.md)。

## 许可证

[MIT](LICENSE)
