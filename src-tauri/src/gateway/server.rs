// server.rs — 网关对本地客户端说的协议。
//
// 两个入口，两份说同一种方言的答案：
//   POST /v1/chat/completions   OpenAI 形状，转发前翻成 Messages，回来再翻回去；
//   POST /v1/messages           Anthropic 形状，只穿衣服不翻译（ZCode 自己的客户端走这条）。
// 两条都指向同一批账号、同一张座位表，所以并发限制是共享的。
//
// 错误码的选择：客户端的 429 会让它退避重试（3008 就是这个意思），402 会让它告诉人
// 「没额度了」（1005 就是这个意思）。所以 402 只在所有套餐都真的拒过之后才发。
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

use crate::gateway::accounts::{self, Target};
use crate::gateway::dress;
use crate::gateway::pool::{Pool, Ready};
use crate::gateway::translate;
use crate::gateway::upstream;
use crate::store::Paths;

const CAP: usize = 1024 * 1024;

pub struct Ctx {
    pub paths: Paths,
    pub pool: Arc<Pool>,
    pub token: String,
    pub default_max_tokens: u32,
    pub app_version: String,
    pub include_live: bool,
    cache: Mutex<Option<(Instant, Vec<Target>)>>,
}

impl Ctx {
    pub fn new(paths: Paths, pool: Arc<Pool>, token: String, include_live: bool) -> Arc<Self> {
        Arc::new(Self {
            paths,
            pool,
            token,
            default_max_tokens: 16_384,
            app_version: dress::APP_VERSION.to_string(),
            include_live,
            cache: Mutex::new(None),
        })
    }

    /// targets 每次调用都算一遍，但 3 秒内复用：新存的账号立刻能用，又不至于每条请求
    /// 都读一遍磁盘。
    pub fn targets(&self) -> Vec<Target> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, list)) = cache.as_ref() {
            if at.elapsed() < Duration::from_secs(3) {
                return list.clone();
            }
        }
        let list = accounts::load_targets(&self.paths, self.include_live, None, None);
        self.pool.touch(&list);
        *cache = Some((Instant::now(), list.clone()));
        list
    }

    /// models 是这个网关对外报的模型名。套餐自己声明过的优先；套餐都没声明时
    /// （比如只剩 start-plan，它的 models 是空的、任何名字都接）报计划里的那一组。
    pub fn models(&self) -> Vec<String> {
        accounts::served_models(&self.targets())
    }
}

pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/v1/health", get(healthz))
        .route("/v1/models", get(models))
        .route("/v1/models/{id}", get(model_one))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/messages", post(messages))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .with_state(ctx)
}

// ---------------------------------------------------------------- 认门

fn digest(s: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().into()
}

fn bearer(headers: &HeaderMap) -> String {
    if let Some(v) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let t = v.trim();
        if let Some(rest) = t.strip_prefix("Bearer ").or_else(|| t.strip_prefix("bearer ")) {
            return rest.trim().to_string();
        }
    }
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return v.trim().to_string();
    }
    String::new()
}

fn authorized(ctx: &Ctx, headers: &HeaderMap) -> bool {
    if ctx.token.is_empty() {
        return true;
    }
    let given = bearer(headers);
    given.len() == ctx.token.len() && digest(&given) == digest(&ctx.token)
}

// ---------------------------------------------------------------- 分类

fn classify(status: u16, text: &str) -> &'static str {
    let low = text.to_ascii_lowercase();
    if status == 405 || low.contains("aliyun") || low.contains("has been blocked") {
        return "edge";
    }
    match upstream::code_of(text) {
        Some(3008) | Some(3009) | Some(3010) => return "busy",
        Some(1113) | Some(1005) => return "spent",
        Some(3012) => return "risk",
        Some(1302) | Some(1303) | Some(3002) => return "rate",
        Some(c) if c != 0 => return if status >= 400 { "other" } else { "other" },
        _ => {}
    }
    if status == 429 {
        return "rate";
    }
    if status == 401 || status == 403 {
        return "auth";
    }
    if status >= 400 {
        return "other";
    }
    "ok"
}

fn status_for(kind: &str) -> StatusCode {
    match kind {
        "spent" => StatusCode::PAYMENT_REQUIRED,
        "busy" | "rate" => StatusCode::TOO_MANY_REQUESTS,
        "risk" => StatusCode::FORBIDDEN,
        "edge" => StatusCode::SERVICE_UNAVAILABLE,
        "auth" => StatusCode::UNAUTHORIZED,
        "timeout" => StatusCode::GATEWAY_TIMEOUT,
        "none" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    }
}

fn fail(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = json!({ "error": { "message": message, "type": kind, "code": kind } });
    (status, axum::Json(body)).into_response()
}

fn fail_kind(kind: &str, message: &str) -> Response {
    fail(status_for(kind), kind, message)
}

// ---------------------------------------------------------------- 模型

/// model_for 是「这个套餐用哪个拼写答这个名字」。套餐没声明模型时按客户端说的发，
/// 声明了就必须用它的拼写：换个名字是 3006 model not allowed。
fn model_for(t: &Target, want: &str) -> Option<String> {
    if t.models.is_empty() {
        return Some(want.to_string());
    }
    t.models
        .iter()
        .find(|m| m.eq_ignore_ascii_case(want))
        .cloned()
}

// ---------------------------------------------------------------- 路由

async fn root(State(ctx): State<Arc<Ctx>>) -> Response {
    let models = ctx.models();
    axum::Json(json!({
        "gateway": "zcode-switch",
        "version": env!("CARGO_PKG_VERSION"),
        "endpoints": ["/v1/chat/completions", "/v1/messages", "/v1/models", "/healthz"],
        "models": models,
    }))
    .into_response()
}

async fn healthz(State(ctx): State<Arc<Ctx>>) -> Response {
    let targets = ctx.targets();
    let snap: HashMap<String, (usize, usize, Option<Duration>)> = ctx
        .pool
        .snapshot()
        .into_iter()
        .map(|(k, a, b, c)| (k, (a, b, c)))
        .collect();
    let rows: Vec<Value> = targets
        .iter()
        .map(|t| {
            let (inflight, sent, rest) = snap.get(&t.key).cloned().unwrap_or((0, 0, None));
            json!({
                "account": t.account_id,
                "label": t.label,
                "provider": t.provider,
                "plan": t.plan,
                "models": t.models,
                "aliases": t.aliases,
                "inflight": inflight,
                "sent": sent,
                "rest_s": rest.map(|d| d.as_secs()),
            })
        })
        .collect();
    let (served, refused, uptime) = ctx.pool.stats();
    axum::Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_s": uptime.as_secs(),
        "served": served,
        "refused": refused,
        "models": ctx.models(),
        "limits": {
            "inflight": ctx.pool.limits().0,
            "rpm": ctx.pool.limits().1,
        },
        "targets": rows,
        "resting": ctx.pool.resting().map(|(d, why)| json!({ "for_s": d.as_secs(), "why": why })),
    }))
    .into_response()
}

async fn models(State(ctx): State<Arc<Ctx>>) -> Response {
    let list: Vec<Value> = ctx.models()
        .into_iter()
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "created": translate::now_secs(),
                "owned_by": "zcode",
            })
        })
        .collect();
    axum::Json(json!({ "object": "list", "data": list })).into_response()
}

async fn model_one(State(ctx): State<Arc<Ctx>>, axum::extract::Path(id): axum::extract::Path<String>) -> Response {
    match ctx.models()
        .into_iter()
        .find(|m| m.eq_ignore_ascii_case(&id))
    {
        Some(found) => axum::Json(json!({
            "id": found,
            "object": "model",
            "created": translate::now_secs(),
            "owned_by": "zcode",
        }))
        .into_response(),
        None => fail(StatusCode::NOT_FOUND, "model_not_found", &format!("no such model: {id}")),
    }
}

async fn not_found() -> Response {
    fail(StatusCode::NOT_FOUND, "not_found", "the gateway serves /v1/chat/completions and /v1/messages")
}

async fn chat(State(ctx): State<Arc<Ctx>>, headers: HeaderMap, axum::Json(body): axum::Json<Value>) -> Response {
    if !authorized(&ctx, &headers) {
        return fail(StatusCode::UNAUTHORIZED, "unauthorized", "this gateway wants its own token: Authorization: Bearer <token>");
    }
    let (req, warnings) = match translate::to_anthropic(&body, ctx.default_max_tokens) {
        Ok(v) => v,
        Err(e) => return fail(StatusCode::BAD_REQUEST, "invalid_request", &e),
    };
    let mut resp = turn(&ctx, req, true).await;
    if !warnings.is_empty() {
        if let Ok(v) = warnings.join("; ").parse() {
            resp.headers_mut().insert("x-gateway-warnings", v);
        }
    }
    resp
}

async fn messages(State(ctx): State<Arc<Ctx>>, headers: HeaderMap, axum::Json(body): axum::Json<Value>) -> Response {
    if !authorized(&ctx, &headers) {
        return fail(StatusCode::UNAUTHORIZED, "unauthorized", "this gateway wants its own token: Authorization: Bearer <token>");
    }
    turn(&ctx, body, false).await
}

// ---------------------------------------------------------------- 一条请求

/// turn 是两条入口共用的那一半：挑账号、穿衣服、发送、把拒绝变成重试或答案。
/// speak_openai 决定回来的东西要不要翻译。
async fn turn(ctx: &Arc<Ctx>, req: Value, speak_openai: bool) -> Response {
    let targets = ctx.targets();
    let want = req.get("model").and_then(|m| m.as_str()).unwrap_or("").trim().to_string();
    if want.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "invalid_request", "model is required");
    }
    let stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let serving: Vec<Target> = targets.iter().filter(|t| model_for(t, &want).is_some()).cloned().collect();
    if serving.is_empty() {
        let known = ctx.models();
        return fail(
            StatusCode::BAD_REQUEST,
            "model_not_found",
            &format!("no plan here serves {want}; this gateway serves: {}", known.join(", ")),
        );
    }

    let deadline = Instant::now() + Duration::from_millis(ctx.pool.opts.queue_ms);
    // 两本「先别问它」的名单。dead 是这一趟里它不会好了（没额度、鉴权、网络不通）；
    // broken 是上游开了口又立刻断——同一批账号过一会儿可能就好了，所以它会被清空重来。
    let mut dead: HashSet<String> = HashSet::new();
    let mut broken: HashSet<String> = HashSet::new();
    let mut saw_spent = false;
    let mut last: (&'static str, String) = ("none", "no account answered".to_string());
    let mut attempt = 0usize;
    let mut busy_round = 0usize;

    while attempt < 32 && Instant::now() < deadline {
        let mut exclude: HashSet<String> = dead.clone();
        exclude.extend(broken.iter().cloned());
        let pick = match ctx.pool.acquire(&serving, &exclude) {
            Ready::Lease(l) => l,
            Ready::Wait(d) => {
                ctx.pool.wait(d).await;
                continue;
            }
            Ready::Rest(reason) => {
                last = if saw_spent { ("spent", reason) } else { ("edge", reason) };
                break;
            }
        };
        let target = match serving.iter().find(|t| t.key == pick.key()) {
            Some(t) => t.clone(),
            None => {
                drop(pick);
                break;
            }
        };
        attempt += 1;
        let (resp, spelling) = match open_once(&req, &want, &target, &ctx.app_version).await {
            Ok(v) => v,
            Err((kind, message)) => {
                ctx.pool.note(&target.key, kind);
                last = (kind, message);
                if kind == "timeout" || kind == "net" {
                    dead.insert(target.key.clone());
                }
                drop(pick);
                continue;
            }
        };
        let status = resp.status().as_u16();
        let streamed = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("event-stream"))
            .unwrap_or(false);

        if status == 200 && stream && streamed {
            let tr = if speak_openai {
                Some(translate::StreamTranslator::new(&spelling, None, translate::now_secs()))
            } else {
                None
            };
            // 空着收尾也算断流：客户端一个字节都还没收到，换账号它看不见。
            let rolled = match preroll(resp, tr).await {
                Ok(head) if head.carried_payload() => Ok(head),
                Ok(_) => Err("the vendor closed the stream before any content".to_string()),
                Err(e) => Err(e),
            };
            match rolled {
                Ok(head) => {
                    ctx.pool.note(&target.key, "ok");
                    return stream_of(head, &target, pick);
                }
                Err(why) => {
                    // 一个字节都还没交给客户端：换账号重来，它那边什么都看不见。
                    ctx.pool.note(&target.key, "broken");
                    last = ("broken", why);
                    broken.insert(target.key.clone());
                    drop(pick);
                    // 全都断过一遍？等一会儿把名单清掉再来一遍，直到排队的期限用完。
                    // 这就是「静默重试」：客户端只会觉得这一轮慢。
                    if serving.iter().all(|t| dead.contains(&t.key) || broken.contains(&t.key)) {
                        let wait = ctx
                            .pool
                            .opts
                            .busy_waits
                            .get(busy_round.min(ctx.pool.opts.busy_waits.len().saturating_sub(1)))
                            .copied()
                            .unwrap_or(1_000);
                        busy_round += 1;
                        if busy_round > 3 || ctx.pool.pending(&serving, &dead) == 0 {
                            break;
                        }
                        broken.clear();
                        ctx.pool.wait(Duration::from_millis(wait)).await;
                    }
                    continue;
                }
            }
        }
        let reply = match upstream::read_text(resp, CAP).await {
            Ok(r) => r,
            Err(e) => {
                let kind = upstream::kind_of(&e);
                ctx.pool.note(&target.key, kind);
                last = (kind, format!("{e}"));
                dead.insert(target.key.clone());
                drop(pick);
                continue;
            }
        };
        let kind = classify(status, &reply.text);
        ctx.pool.note(&target.key, kind);
        drop(pick);

        if kind == "ok" {
            let parsed: Value = match serde_json::from_str(&reply.text) {
                Ok(v) => v,
                Err(_) => {
                    return fail(
                        StatusCode::BAD_GATEWAY,
                        "bad_upstream",
                        &format!("the vendor did not answer JSON: {}", upstream::message_of(&reply.text)),
                    )
                }
            };
            let mut answer = if speak_openai {
                translate::to_openai(&parsed, &want)
            } else {
                parsed
            };
            if speak_openai {
                // 客户端要的是它自己写的名字；翻译层只知道套餐的拼写
                answer["model"] = Value::String(want.clone());
            }
            let mut r = axum::Json(answer).into_response();
            // 头只能是 ASCII：账号名里有中文，所以这里放的是账号文件 id，不是显示名
            if let Ok(v) = target.account_id.parse() {
                r.headers_mut().insert("x-zcode-account", v);
            }
            if let Ok(v) = target.plan.parse() {
                r.headers_mut().insert("x-zcode-plan", v);
            }
            return r;
        }

        let message = upstream::message_of(&reply.text);
        last = (kind, message);
        match kind {
            "busy" | "rate" => {
                // 这个账号没有「用掉」：等一会儿再问它，别把它排除掉
                let wait = ctx
                    .pool
                    .opts
                    .busy_waits
                    .get(busy_round.min(ctx.pool.opts.busy_waits.len().saturating_sub(1)))
                    .copied()
                    .unwrap_or(1_000);
                busy_round += 1;
                if ctx.pool.pending(&serving, &dead) == 0 {
                    break;
                }
                ctx.pool.wait(Duration::from_millis(wait)).await;
            }
            "spent" => {
                saw_spent = true;
                dead.insert(target.key.clone());
            }
            _ => {
                dead.insert(target.key.clone());
            }
        }
        if ctx.pool.pending(&serving, &dead) == 0 {
            break;
        }
    }

    let (kind, message) = if saw_spent && matches!(last.0, "busy" | "rate" | "none" | "broken") {
        ("spent", message_of_spent(&last.1))
    } else {
        last
    };
    fail_kind(kind, &message)
}

/// open_once 准备并发出一次请求：把模型名换成套餐的拼写、穿衣服、带上会话 id。
/// 失败时给 (kind, message)，由调用方决定换账号还是放弃。
async fn open_once(
    req: &Value,
    want: &str,
    target: &Target,
    app_version: &str,
) -> Result<(reqwest::Response, String), (&'static str, String)> {
    let spelling = model_for(target, want).unwrap_or_else(|| want.to_string());
    let mut out = req.clone();
    out["model"] = Value::String(spelling.clone());
    let text = out.to_string();

    // 会话 id 由「账号 + 这一段对话」决定：同一段对话里每一轮都相同，提示词缓存才落得住。
    // 它不参与鉴权，只影响厂商那边的缓存命中。
    let session = dress::session_of(&conversation_seed(req, target), &target.device);
    let query = dress::query_of(&text, &session);
    let body = if target.dress {
        dress::dress(&text, target.site, &target.device, &session)
    } else {
        dress::marked(&text, &target.device, &session)
    };
    let payload: Value = serde_json::from_str(&body).unwrap_or(out);
    let headers = if target.plan == "start" {
        dress::start_headers(&target.key, &session, &query, app_version)
    } else {
        dress::app_headers(&target.key, &target.device, app_version)
    };
    upstream::post(&target.endpoint(), &headers, &payload)
        .await
        .map(|r| (r, spelling))
        .map_err(|e| (upstream::kind_of(&e), format!("{e}")))
}

fn message_of_spent(inner: &str) -> String {
    if inner.is_empty() {
        "every plan here is out of quota".to_string()
    } else {
        format!("every plan here is out of quota: {inner}")
    }
}

/// conversation_seed 让同一段对话在同一账号上一直是同一个会话，换账号就换一个：
/// 缓存是按设备+会话算的，混用只会让两边的缓存互相打散。
fn conversation_seed(req: &Value, t: &Target) -> String {
    let mut first = String::new();
    if let Some(msgs) = req.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            if m.get("role").and_then(|r| r.as_str()) != Some("user") {
                continue;
            }
            match m.get("content") {
                Some(Value::String(s)) => first.push_str(s),
                Some(Value::Array(items)) => {
                    for x in items {
                        if let Some(s) = x.get("text").and_then(|t| t.as_str()) {
                            first.push_str(s);
                        }
                    }
                }
                _ => {}
            }
            if !first.is_empty() {
                break;
            }
        }
    }
    let tail: String = first.chars().take(400).collect();
    format!("zcode-gw\0{}\0{}", t.subject, tail)
}

// ---------------------------------------------------------------- 流

fn sse(body: Body, target: &Target) -> Response {
    let mut r = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream; charset=utf-8")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .unwrap_or_else(|_| fail(StatusCode::INTERNAL_SERVER_ERROR, "internal", "could not build the stream"));
    // 流也要说出这些话是哪来的账号、什么套餐（只用 ASCII）
    if let Ok(v) = target.account_id.parse() {
        r.headers_mut().insert("x-zcode-account", v);
    }
    if let Ok(v) = target.plan.parse() {
        r.headers_mut().insert("x-zcode-plan", v);
    }
    r
}

/// Head 是 preroll 的成果：已经翻好的开头、（OpenAI 入口的）翻译器、还没读完的上游响应。
struct Head {
    prologue: Vec<u8>,
    resp: reqwest::Response,
    tr: Option<translate::StreamTranslator>,
    /// 上游在 preroll 期间就收尾了：剩下的只有 prologue。
    finished: bool,
}

impl Head {
    /// carried_payload 说这个流的开头里有没有真正的内容。上游 200 之后正文一个字节都
    /// 没给就收了尾，跟断流是一回事——这时客户端还没收到任何东西，换账号完全来得及。
    fn carried_payload(&self) -> bool {
        match self.tr.as_ref() {
            Some(t) => t.saw_content(),
            None => !self.prologue.is_empty(),
        }
    }
}

/// preroll 先替客户端把流的开头读下来，读到正文才交出去。
///
/// 起因是 start plan：它在智谱那边优先度最低，常见的是 200 之后立刻断流——客户端那边
/// 显示 "error decoding response body"，一整轮白跑（DSH 里就是这么报的）。只要客户端
/// 一个字节都还没收到，换账号重来它完全看不见，只会觉得这一轮慢一点。
async fn preroll(mut resp: reqwest::Response, mut tr: Option<translate::StreamTranslator>) -> Result<Head, String> {
    let mut prologue: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => match tr.as_mut() {
                Some(t) => {
                    prologue.extend_from_slice(t.feed(&String::from_utf8_lossy(&chunk)).as_bytes());
                    // 第一段正文（文字、思考或工具参数）到了就算接上了：再往后换账号
                    // 已经来不及——客户端那边已经有东西了。
                    if t.saw_content() {
                        return Ok(Head { prologue, resp, tr, finished: false });
                    }
                }
                // Anthropic 入口是原样透传：第一批字节到了就算接上了
                None => {
                    prologue.extend_from_slice(&chunk);
                    return Ok(Head { prologue, resp, tr, finished: false });
                }
            },
            Ok(None) => {
                if let Some(t) = tr.as_mut() {
                    prologue.extend_from_slice(t.finish().as_bytes());
                }
                return Ok(Head { prologue, resp, tr, finished: true });
            }
            Err(e) => return Err(format!("{e}")),
        }
    }
}

/// stream_of 把 preroll 读下来的开头先交给客户端，后面的跟着上游走。座位（lease）跟着流走，
/// 流结束才放——否则下一个请求会在上一个还在答的时候挤进来。
fn stream_of(head: Head, target: &Target, lease: crate::gateway::pool::Lease) -> Response {
    let state = (head, lease, false);
    let stream = futures_util::stream::unfold(state, |(mut head, lease, mut done)| async move {
        let _hold = &lease;
        let p = std::mem::take(&mut head.prologue);
        if !p.is_empty() {
            return Some((Ok::<Vec<u8>, std::io::Error>(p), (head, lease, done)));
        }
        if done || head.finished {
            return None;
        }
        loop {
            match head.resp.chunk().await {
                Ok(Some(chunk)) => {
                    let text = String::from_utf8_lossy(&chunk);
                    let out = match head.tr.as_mut() {
                        Some(t) => t.feed(&text),
                        None => text.to_string(),
                    };
                    if !out.is_empty() {
                        return Some((Ok(out.into_bytes()), (head, lease, done)));
                    }
                }
                Ok(None) => {
                    let tail = match head.tr.as_mut() {
                        Some(t) => t.finish(),
                        None => String::new(),
                    };
                    done = true;
                    if tail.is_empty() {
                        return None;
                    }
                    return Some((Ok(tail.into_bytes()), (head, lease, done)));
                }
                Err(e) => {
                    // 已经交给客户端了，换不了账号：把断在哪里说清楚。OpenAI 入口用事件说，
                    // Anthropic 入口只能让流停掉。
                    done = true;
                    let msg = match head.tr.as_ref() {
                        Some(_) => translate::sse_text(&json!({
                            "error": { "message": format!("the vendor's stream broke: {e}"), "type": "upstream_error" }
                        })),
                        None => String::new(),
                    };
                    if msg.is_empty() {
                        return None;
                    }
                    return Some((Ok(msg.into_bytes()), (head, lease, done)));
                }
            }
        }
    });
    sse(Body::from_stream(stream), target)
}
