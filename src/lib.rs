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
            enabled: true,
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

    if !cfg.inbound.enabled {
        return json_response(
            503,
            serde_json::json!({"ok": false, "error": "inbound 已停用"}),
        );
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
    version = "0.1.0",
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
        let cfg = match parse_config(config.config_json.as_str()) {
            Ok(c) => c,
            Err(e) => return PluginInitResult::err(&e),
        };
        if cfg.inbound.enabled && cfg.inbound.secret.is_empty() {
            return PluginInitResult::err("inbound.enabled 为 true 时 inbound.secret 不能为空");
        }
        if cfg.inbound.enabled && cfg.notify_targets.is_empty() {
            eprintln!(
                "[mofang-ticket] inbound 已启用但 notify.targets 为空，收到工单将不会推送到任何群"
            );
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
        *CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = Some(cfg);
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
        assert!(cfg.inbound.enabled);
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
}
