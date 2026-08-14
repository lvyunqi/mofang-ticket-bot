//! 魔方财务工单 · QimenBot 动态插件（Phase 1：Webhook 接收与鉴权）。
//!
//! 对外路由：POST `{base_path}/mofang-ticket/events`（默认 `/webhooks/mofang-ticket/events`）。
//!
//! 鉴权（DESIGN.md §4.3）：`HMAC-SHA256(secret, "{ts}.{nonce}.{raw_body}")`
//! + 时间戳容差窗口 + nonce 防重放；业务幂等用 `event.id` 去重（§4.1）。
//!
//! 当前阶段：接收 + 鉴权 + 去重后，把原始 body 原文透传到配置的 `notify.targets` 群。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use abi_stable_host_api::{
    BotApi, PluginInitConfig, PluginInitResult, WebhookRequest, WebhookResponse,
};
use hmac::{Hmac, Mac};
use qimen_dynamic_plugin_derive::dynamic_plugin;
use serde::Deserialize;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// `event.id` 幂等去重保留时长（秒）。相对 nonce 窗口更长，保证业务重试不重复处理。
const EVENT_ID_TTL_SECS: i64 = 86_400;

/// `config.schema.json` 中 `inbound.secret` 的最低长度（按 trim 后 Unicode 字符计数）。
const MIN_SECRET_CHARS: usize = 16;

// ── 统一信封模型（DESIGN.md §4.1）───────────────────────────────

// Phase 1.6 群推送渲染会消费 ticket/changes/actor/content/internal_note 等字段；
// 当前阶段仅做接收与鉴权，先允许未读。
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Envelope {
    version: u32,
    event: String,
    id: String,
    ts: i64,
    #[serde(default)]
    ticket: Option<Ticket>,
    #[serde(default)]
    changes: Option<serde_json::Value>,
    #[serde(default)]
    actor: Option<serde_json::Value>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    internal_note: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct Ticket {
    #[serde(default)]
    id: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    customer: Option<serde_json::Value>,
    #[serde(default)]
    assignee: Option<serde_json::Value>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

// ── 配置 ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct InboundConfig {
    enabled: bool,
    secret: String,
    timestamp_tolerance_secs: i64,
    nonce_cache_size: usize,
}

impl Default for InboundConfig {
    fn default() -> Self {
        Self {
            // 安全默认：首次安装/空配置以禁用态加载（ai-news 模式），
            // 避免“缺 secret 导致 init 失败、安装事务回滚”。
            enabled: false,
            secret: String::new(),
            timestamp_tolerance_secs: 300,
            nonce_cache_size: 4096,
        }
    }
}

/// 主动推送目标：稳定账号 + 协议原生群号（DESIGN.md §10 成对配置）。
#[derive(Debug, Clone, PartialEq)]
struct NotifyTarget {
    account_id: String,
    group_id: String,
}

#[derive(Debug, Clone)]
struct Config {
    inbound: InboundConfig,
    notify_targets: Vec<NotifyTarget>,
}

fn parse_config(config_json: &str) -> Result<Config, String> {
    let root: serde_json::Value = if config_json.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(config_json).map_err(|e| format!("配置 JSON 无效：{e}"))?
    };

    let inbound = root
        .get("inbound")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let mut ib = InboundConfig::default();
    if let Some(v) = inbound.get("enabled").and_then(serde_json::Value::as_bool) {
        ib.enabled = v;
    }
    if let Some(v) = inbound.get("secret").and_then(serde_json::Value::as_str) {
        ib.secret = v.trim().to_string();
    }
    if let Some(v) = inbound
        .get("timestamp_tolerance_secs")
        .and_then(serde_json::Value::as_i64)
    {
        ib.timestamp_tolerance_secs = v.clamp(1, 3600);
    }
    if let Some(v) = inbound
        .get("nonce_cache_size")
        .and_then(serde_json::Value::as_u64)
    {
        ib.nonce_cache_size = (v as usize).clamp(1, 1_000_000);
    }

    let mut notify_targets = Vec::new();
    if let Some(arr) = root
        .get("notify")
        .and_then(|v| v.get("targets"))
        .and_then(serde_json::Value::as_array)
    {
        for item in arr {
            let account_id = item
                .get("account_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            let group_id = item
                .get("group_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            if account_id.is_empty() || group_id.is_empty() {
                eprintln!(
                    "[mofang-ticket] 忽略无效的 notify.targets 条目（account_id/group_id 为空）"
                );
                continue;
            }
            notify_targets.push(NotifyTarget {
                account_id,
                group_id,
            });
        }
    }

    Ok(Config {
        inbound: ib,
        notify_targets,
    })
}

/// `init` 的纯函数准备阶段：解析配置并收集警告，不触碰全局状态（可单测，避免并行竞态）。
struct InitOutcome {
    config: Config,
    warnings: Vec<String>,
}

/// 解析配置并汇总加载警告。缺 secret 不是错误：`enabled = true` 但 secret 未配置或不足16字符时
/// 插件仍可加载（Webhook 返回 503 提示），保证首次安装/空配置不阻断安装事务。
fn prepare_init(config_json: &str) -> Result<InitOutcome, String> {
    let config = parse_config(config_json)?;
    let mut warnings = Vec::new();
    if config.inbound.enabled && !secret_ready(&config.inbound.secret) {
        warnings.push(
            "inbound.enabled 为 true 但 inbound.secret 未配置或不足 16 字符：Webhook 将返回 503，请填写 ≥16 字符的 secret 后重载"
                .to_string(),
        );
    }
    if config.inbound.enabled && config.notify_targets.is_empty() {
        warnings
            .push("inbound 已启用但 notify.targets 为空，收到工单将不会推送到任何群".to_string());
    }
    Ok(InitOutcome { config, warnings })
}

// ── 全局状态（init 时重置，支持 reload 重复初始化）─────────────

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);
static NONCE_CACHE: Mutex<VecDeque<(String, i64)>> = Mutex::new(VecDeque::new());
static EVENT_CACHE: Mutex<VecDeque<(String, i64)>> = Mutex::new(VecDeque::new());

// ── 鉴权（纯函数，可单测）──────────────────────────────────────

/// 校验 HMAC 签名 + 时间窗，返回解析后的 `ts`。
///
/// 签名规范串使用**原始 header 字符串** `ts_str`（而非重格式化的整数），
/// 对原始 body 字节做 HMAC，避免键序重排或整数格式差异导致字节不一致。
fn verify_signature(
    secret: &str,
    signature_hex: &str,
    ts_str: &str,
    nonce: &str,
    body: &[u8],
    tolerance: i64,
    now: i64,
) -> Result<i64, (u16, &'static str)> {
    let ts: i64 = ts_str
        .parse()
        .map_err(|_| (400, "X-Mofang-Timestamp 不是整数"))?;
    if (now - ts).abs() > tolerance {
        return Err((400, "时间戳超出容差窗口"));
    }

    let mut canonical = Vec::with_capacity(ts_str.len() + nonce.len() + body.len() + 2);
    canonical.extend_from_slice(ts_str.as_bytes());
    canonical.push(b'.');
    canonical.extend_from_slice(nonce.as_bytes());
    canonical.push(b'.');
    canonical.extend_from_slice(body);

    let expected = hex::decode(signature_hex).map_err(|_| (400, "签名不是合法 hex"))?;
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| (500, "HMAC 密钥非法"))?;
    mac.update(&canonical);
    mac.verify_slice(&expected)
        .map_err(|_| (401, "签名不匹配"))?;

    Ok(ts)
}

// ── nonce 防重放 / event.id 幂等去重（有界，按时间过期）──────────

fn prune_expired(cache: &mut VecDeque<(String, i64)>, now: i64) {
    while let Some((_, expiry)) = cache.front() {
        if *expiry < now {
            cache.pop_front();
        } else {
            break;
        }
    }
}

fn nonce_replayed(nonce: &str, now: i64) -> bool {
    let mut cache = NONCE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    prune_expired(&mut cache, now);
    cache.iter().any(|(n, _)| n == nonce)
}

fn nonce_record(nonce: &str, now: i64, tolerance: i64, cap: usize) {
    let mut cache = NONCE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    prune_expired(&mut cache, now);
    cache.push_back((nonce.to_string(), now + tolerance));
    while cache.len() > cap {
        cache.pop_front();
    }
}

fn event_seen(id: &str, now: i64) -> bool {
    let mut cache = EVENT_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    prune_expired(&mut cache, now);
    cache.iter().any(|(n, _)| n == id)
}

fn event_record(id: &str, now: i64, cap: usize) {
    let mut cache = EVENT_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    prune_expired(&mut cache, now);
    cache.push_back((id.to_string(), now + EVENT_ID_TTL_SECS));
    while cache.len() > cap {
        cache.pop_front();
    }
}

// ── 工具 ───────────────────────────────────────────────────────

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_headers(json: &str) -> Result<HashMap<String, String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("headers_json 不是合法 JSON：{e}"))?;
    let obj = value.as_object().ok_or("headers_json 不是对象")?;
    let mut map = HashMap::with_capacity(obj.len());
    for (k, v) in obj {
        let s = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(arr) => arr
                .first()
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            other => other.to_string(),
        };
        map.insert(k.to_ascii_lowercase(), s);
    }
    Ok(map)
}

fn json_response(code: u16, payload: serde_json::Value) -> WebhookResponse {
    WebhookResponse::text(code, &payload.to_string())
        .with_headers_json(r#"{"content-type":"application/json; charset=utf-8"}"#)
}

// ── 主动推送（Phase 1.6：原始内容透传）─────────────────────────

/// 把文本推送到所有已配置目标群，逐条检查 `SendEnqueueStatus`。
/// 失败仅记录到 stderr，不阻断 webhook 的 200 响应；重试与退避留待后续阶段。
fn push_to_targets(targets: &[NotifyTarget], text: &str) {
    for target in targets {
        let status = BotApi::for_account(&target.account_id).send_group_msg(&target.group_id, text);
        if !status.is_accepted() {
            eprintln!(
                "[mofang-ticket] 群推送未受理 account={} group={} status={status:?}",
                target.account_id, target.group_id
            );
        }
    }
}

// ── inbound 就绪检查（纯函数，可单测）─────────────────────────

/// inbound 是否可继续处理：`None` 表示就绪；`Some(reason)` 表示不可用原因
/// （停用 / 未配置 secret）。供 `handle_webhook` 返回 503 使用。
/// secret 是否达到 schema 的最低强度：trim 后 Unicode 字符数 ≥ `MIN_SECRET_CHARS`。
fn secret_ready(secret: &str) -> bool {
    secret.trim().chars().count() >= MIN_SECRET_CHARS
}

fn inbound_unavailable_reason(inbound: &InboundConfig) -> Option<&'static str> {
    if !inbound.enabled {
        return Some("inbound 已停用");
    }
    if !secret_ready(&inbound.secret) {
        return Some("inbound 未配置或不足 16 字符的 secret：请在配置中填写 inbound.secret 后重载");
    }
    None
}

// ── Webhook 处理器 ─────────────────────────────────────────────

fn handle_webhook(req: &WebhookRequest) -> WebhookResponse {
    let cfg = match CONFIG.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(c) => c.clone(),
        None => {
            return json_response(
                500,
                serde_json::json!({"ok": false, "error": "插件未初始化"}),
            );
        }
    };

    if let Some(reason) = inbound_unavailable_reason(&cfg.inbound) {
        return json_response(503, serde_json::json!({"ok": false, "error": reason}));
    }

    // 1) 解析鉴权头
    let headers = match parse_headers(req.headers_json.as_str()) {
        Ok(h) => h,
        Err(e) => return json_response(400, serde_json::json!({"ok": false, "error": e})),
    };
    let signature = match headers.get("x-mofang-signature") {
        Some(v) => v.clone(),
        None => {
            return json_response(
                401,
                serde_json::json!({"ok": false, "error": "缺少 X-Mofang-Signature"}),
            );
        }
    };
    let ts_str = match headers.get("x-mofang-timestamp") {
        Some(v) => v.clone(),
        None => {
            return json_response(
                401,
                serde_json::json!({"ok": false, "error": "缺少 X-Mofang-Timestamp"}),
            );
        }
    };
    let nonce = match headers.get("x-mofang-nonce") {
        Some(v) => v.clone(),
        None => {
            return json_response(
                401,
                serde_json::json!({"ok": false, "error": "缺少 X-Mofang-Nonce"}),
            );
        }
    };

    let now = now_secs();

    // 2) 鉴权：签名 + 时间窗
    if let Err((code, msg)) = verify_signature(
        &cfg.inbound.secret,
        &signature,
        &ts_str,
        &nonce,
        req.body.as_slice(),
        cfg.inbound.timestamp_tolerance_secs,
        now,
    ) {
        return json_response(code, serde_json::json!({"ok": false, "error": msg}));
    }

    // 3) nonce 防重放（签名通过后才记录，避免被无效请求消耗）
    if nonce_replayed(&nonce, now) {
        return json_response(401, serde_json::json!({"ok": false, "error": "nonce 重放"}));
    }
    nonce_record(
        &nonce,
        now,
        cfg.inbound.timestamp_tolerance_secs,
        cfg.inbound.nonce_cache_size,
    );

    // 4) 解析并校验信封
    let body_str = String::from_utf8_lossy(req.body.as_slice());
    let envelope: Envelope = match serde_json::from_str(&body_str) {
        Ok(e) => e,
        Err(e) => {
            return json_response(
                400,
                serde_json::json!({"ok": false, "error": format!("信封 JSON 无效：{e}")}),
            );
        }
    };
    if envelope.version != 1 {
        return json_response(
            400,
            serde_json::json!({"ok": false, "error": "未识别的 version"}),
        );
    }
    if envelope.event.trim().is_empty() {
        return json_response(
            400,
            serde_json::json!({"ok": false, "error": "event 不能为空"}),
        );
    }
    if envelope.id.trim().is_empty() {
        return json_response(
            400,
            serde_json::json!({"ok": false, "error": "id 不能为空"}),
        );
    }

    // 5) 幂等去重（同一 event.id 的业务重试：返回已处理，不重复处理）
    if event_seen(&envelope.id, now) {
        return json_response(
            200,
            serde_json::json!({"ok": true, "id": envelope.id, "dup": true}),
        );
    }
    event_record(&envelope.id, now, cfg.inbound.nonce_cache_size);

    // 6) 群推送：MVP 阶段把原始 body 原文透传到目标群（模板渲染留待后续）
    push_to_targets(&cfg.notify_targets, body_str.as_ref());

    json_response(
        200,
        serde_json::json!({
            "ok": true,
            "id": envelope.id,
            "event": envelope.event,
            "pushed": cfg.notify_targets.len()
        }),
    )
}

// ── 动态插件入口 ───────────────────────────────────────────────

#[dynamic_plugin(
    id = "mofang-ticket",
    version = "0.1.6",
    api = "0.6",
    config_schema = "../config.schema.json",
    config_ui = "../config.ui.json",
    config_version = 1,
    config_apply = "reload"
)]
mod plugin {
    use super::*;

    #[init]
    fn init(config: PluginInitConfig) -> PluginInitResult {
        let outcome = match prepare_init(config.config_json.as_str()) {
            Ok(o) => o,
            Err(e) => return PluginInitResult::err(&e),
        };
        for w in &outcome.warnings {
            eprintln!("[mofang-ticket] {w}");
        }
        // reload 语义：重置全部内存状态，保证 init 可重复调用
        NONCE_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        EVENT_CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome.config);
        PluginInitResult::ok()
    }

    #[webhook(method = "POST", path = "/events")]
    fn receive_event(req: &WebhookRequest) -> WebhookResponse {
        handle_webhook(req)
    }
}

// ── 单元测试 ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "mofang-demo-secret-0123456789";

    /// 与生产相同的签名算法，用于测试端独立生成期望签名。
    fn sign(secret: &str, ts: &str, nonce: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        let mut canonical = Vec::new();
        canonical.extend_from_slice(ts.as_bytes());
        canonical.push(b'.');
        canonical.extend_from_slice(nonce.as_bytes());
        canonical.push(b'.');
        canonical.extend_from_slice(body);
        mac.update(&canonical);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn verify_signature_ok() {
        let ts = "1700000000";
        let nonce = "n1a2b3c4d5";
        let body = br#"{"version":1,"event":"ticket.created","id":"evt_1","ts":1700000000}"#;
        let sig = sign(SECRET, ts, nonce, body);
        let r = verify_signature(SECRET, &sig, ts, nonce, body, 300, 1700000000);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), 1700000000);
    }

    #[test]
    fn verify_signature_mismatch() {
        let ts = "1700000000";
        let nonce = "n1a2b3c4d5";
        let sig = sign(SECRET, ts, nonce, br#"{"x":1}"#);
        let r = verify_signature(SECRET, &sig, ts, nonce, br#"{"x":2}"#, 300, 1700000000);
        assert_eq!(r.unwrap_err().0, 401);
    }

    #[test]
    fn verify_signature_tampered_ts() {
        let ts = "1700000000";
        let nonce = "n1a2b3c4d5";
        let body = br#"{"x":1}"#;
        let sig = sign(SECRET, ts, nonce, body);
        // 篡改 ts 字符串但签名未变 → 不匹配
        let r = verify_signature(SECRET, &sig, "1700000001", nonce, body, 300, 1700000000);
        assert_eq!(r.unwrap_err().0, 401);
    }

    #[test]
    fn verify_signature_out_of_window() {
        let ts = "1700000000";
        let nonce = "n1a2b3c4d5";
        let body = br#"{"x":1}"#;
        let sig = sign(SECRET, ts, nonce, body);
        let r = verify_signature(SECRET, &sig, ts, nonce, body, 300, 1700001000);
        assert_eq!(r.unwrap_err().0, 400);
    }

    #[test]
    fn verify_signature_bad_hex() {
        let r = verify_signature(SECRET, "zzzz", "1700000000", "n", br#"{}"#, 300, 1700000000);
        assert_eq!(r.unwrap_err().0, 400);
    }

    #[test]
    fn parse_envelope_ok() {
        let body = r#"{"version":1,"event":"ticket.created","id":"evt_1","ts":1700000000,"ticket":{"id":"TK-1","subject":"账单"}}"#;
        let env: Envelope = serde_json::from_str(body).unwrap();
        assert_eq!(env.version, 1);
        assert_eq!(env.event, "ticket.created");
        assert_eq!(env.id, "evt_1");
        assert_eq!(env.ticket.unwrap().subject, "账单");
    }

    #[test]
    fn parse_envelope_missing_id_fails() {
        let body = r#"{"version":1,"event":"ticket.created","ts":1700000000}"#;
        assert!(serde_json::from_str::<Envelope>(body).is_err());
    }

    #[test]
    fn config_defaults() {
        let cfg = parse_config("").unwrap();
        assert!(!cfg.inbound.enabled, "空配置默认禁用（安全默认）");
        assert!(cfg.inbound.secret.is_empty());
        assert_eq!(cfg.inbound.timestamp_tolerance_secs, 300);
        assert_eq!(cfg.inbound.nonce_cache_size, 4096);
        assert!(cfg.notify_targets.is_empty());
    }

    #[test]
    fn config_parses_inbound() {
        let cfg = parse_config(
            r#"{"inbound":{"secret":"abcdefghijklmnop","timestamp_tolerance_secs":120}}"#,
        )
        .unwrap();
        assert_eq!(cfg.inbound.secret, "abcdefghijklmnop");
        assert_eq!(cfg.inbound.timestamp_tolerance_secs, 120);
    }

    #[test]
    fn config_parses_notify_targets() {
        let cfg = parse_config(
            r#"{"notify":{"targets":[{"account_id":"111","group_id":"222"},{"account_id":"","group_id":"333"}]}}"#,
        )
        .unwrap();
        assert_eq!(cfg.notify_targets.len(), 1);
        assert_eq!(cfg.notify_targets[0].account_id, "111");
        assert_eq!(cfg.notify_targets[0].group_id, "222");
    }

    // ── 首次安装 / 空配置安全默认（回归）────────────────────────

    #[test]
    fn init_empty_config_succeeds_disabled() {
        // 首次安装没有 config/plugins/<id>.toml → config_json 为空串
        let outcome = prepare_init("").unwrap();
        assert!(!outcome.config.inbound.enabled);
        assert!(outcome.config.inbound.secret.is_empty());
        assert!(outcome.warnings.is_empty());
    }

    #[test]
    fn init_enabled_without_secret_is_not_error() {
        // enabled=true 但 secret 为空：不是初始化错误，而是加载警告
        let outcome = prepare_init(r#"{"inbound":{"enabled":true}}"#).unwrap();
        assert!(outcome.config.inbound.enabled);
        assert!(outcome.config.inbound.secret.is_empty());
        assert!(
            outcome.warnings.iter().any(|w| w.contains("secret")),
            "应给出缺 secret 的加载警告：{:?}",
            outcome.warnings
        );
    }

    #[test]
    fn init_enabled_weak_secret_warns() {
        // 15 字符仍低于 16 字符下限 → 加载警告（非错误）
        let outcome =
            prepare_init(r#"{"inbound":{"enabled":true,"secret":"short-secret-12"}}"#).unwrap();
        assert!(outcome.config.inbound.enabled);
        assert!(
            outcome.warnings.iter().any(|w| w.contains("secret")),
            "15 字符 secret 应产生加载警告：{:?}",
            outcome.warnings
        );
    }

    #[test]
    fn init_enabled_min_secret_no_secret_warning() {
        // 恰好 16 字符 → 无缺 secret 警告（可能仍有 notify.targets 为空警告）
        let outcome =
            prepare_init(r#"{"inbound":{"enabled":true,"secret":"1234567890abcdef"}}"#).unwrap();
        assert!(outcome.config.inbound.enabled);
        assert!(
            !outcome.warnings.iter().any(|w| w.contains("secret")),
            "16 字符 secret 不应有缺 secret 警告：{:?}",
            outcome.warnings
        );
    }

    #[test]
    fn init_invalid_json_is_error() {
        // 只有配置 JSON 本身非法才是初始化错误
        assert!(prepare_init("{not json").is_err());
    }

    // ── inbound 就绪检查（三态）────────────────────────────────

    #[test]
    fn inbound_disabled_returns_reason() {
        let ib = InboundConfig {
            enabled: false,
            secret: SECRET.into(),
            ..Default::default()
        };
        assert_eq!(inbound_unavailable_reason(&ib), Some("inbound 已停用"));
    }

    #[test]
    fn inbound_enabled_without_secret_returns_reason() {
        let ib = InboundConfig {
            enabled: true,
            secret: String::new(),
            ..Default::default()
        };
        assert_eq!(
            inbound_unavailable_reason(&ib),
            Some("inbound 未配置或不足 16 字符的 secret：请在配置中填写 inbound.secret 后重载")
        );
    }

    #[test]
    fn inbound_ready_returns_none() {
        let ib = InboundConfig {
            enabled: true,
            secret: SECRET.into(),
            ..Default::default()
        };
        assert_eq!(inbound_unavailable_reason(&ib), None);
    }

    #[test]
    fn inbound_weak_secret_returns_reason() {
        // 15 字符仍不足 16 字符下限
        let ib = InboundConfig {
            enabled: true,
            secret: "short-secret-12".into(),
            ..Default::default()
        };
        assert_eq!(
            inbound_unavailable_reason(&ib),
            Some("inbound 未配置或不足 16 字符的 secret：请在配置中填写 inbound.secret 后重载")
        );
    }

    #[test]
    fn inbound_secret_at_min_length_ready() {
        // 恰好 16 个字符：就绪
        let ib = InboundConfig {
            enabled: true,
            secret: "1234567890abcdef".into(),
            ..Default::default()
        };
        assert_eq!(inbound_unavailable_reason(&ib), None);
    }

    #[test]
    fn config_trims_secret_before_readiness() {
        // parse_config 已 trim；首尾空白不影响就绪判断
        let cfg = parse_config(r#"{"inbound":{"enabled":true,"secret":"  1234567890abcdef  "}}"#)
            .unwrap();
        assert_eq!(cfg.inbound.secret, "1234567890abcdef");
        assert_eq!(inbound_unavailable_reason(&cfg.inbound), None);
    }
}
