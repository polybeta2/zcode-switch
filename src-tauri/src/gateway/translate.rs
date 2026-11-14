// translate.rs — 这里说的 OpenAI 形状，和厂商说的 Anthropic 形状。
//
// ZCode 的套餐只收 Messages 请求，所以一个说 OpenAI 的客户端（chat/completions，
// 流式或不是）进来要翻一次，出去再翻一次。翻译刻意薄：OpenAI 有而 Anthropic 没有
// 的字段丢掉，能表达却含义不同的（n > 1）直接拒绝，而不是悄悄只答一个。
//
// 这里全是纯函数：不碰 socket，不读时钟（除了 created_at），调用之间没有状态，
// 所以测试不用网络就能跑。
use serde_json::{json, Map, Value};

fn stop_reason(stop: &str) -> &'static str {
    match stop {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => "stop",
    }
}

pub fn finish_reason(stop: Option<&str>) -> String {
    stop.map(stop_reason).unwrap_or("stop").to_string()
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|p| match p {
                Value::String(s) => s.clone(),
                other => other
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
            .collect(),
        _ => String::new(),
    }
}

fn image_block(url: &str) -> Option<Value> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (mt, b64) = rest.split_once(";base64,")?;
        return Some(json!({ "type": "image", "source": { "type": "base64", "media_type": mt, "data": b64 } }));
    }
    Some(json!({ "type": "image", "source": { "type": "url", "url": url } }))
}

/// parts_to_anthropic 把 OpenAI 的内容数组变成 Anthropic 的 block。图片是两边都有
/// 的那一种多模态：data: URL 转成 base64 source，普通 URL 转成 url source。别的
/// 都丢掉——发一个厂商不认识的 block 是整条请求 400。
fn parts_to_anthropic(content: &[Value], warn: &mut Vec<String>) -> Vec<Value> {
    let mut out = Vec::new();
    for p in content {
        match p {
            Value::String(s) => {
                if !s.is_empty() {
                    out.push(json!({ "type": "text", "text": s }));
                }
            }
            Value::Object(o) => {
                let kind = o.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if kind == "text" {
                    if let Some(t) = o.get("text").and_then(|t| t.as_str()) {
                        if !t.is_empty() {
                            out.push(json!({ "type": "text", "text": t }));
                        }
                    }
                } else if kind == "image_url" {
                    let url = match o.get("image_url") {
                        Some(Value::String(s)) => Some(s.clone()),
                        Some(Value::Object(i)) => i.get("url").and_then(|u| u.as_str()).map(String::from),
                        _ => None,
                    };
                    if let Some(u) = url.filter(|u| !u.is_empty()) {
                        if let Some(b) = image_block(&u) {
                            out.push(b);
                        }
                    }
                } else if kind == "input_audio" || kind == "file" {
                    warn.push(format!("dropped an {kind} part: the vendor takes text and images only"));
                }
            }
            _ => {}
        }
    }
    out
}

fn is_tool_results(m: &Value) -> bool {
    m.get("role").and_then(|r| r.as_str()) == Some("user")
        && m
            .get("content")
            .and_then(|c| c.as_array())
            .map(|items| items.iter().all(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result")))
            .unwrap_or(false)
}

fn tool_result_block(m: &Value) -> Value {
    let id = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
    match m.get("content") {
        Some(Value::Array(items)) => {
            let mut sink = Vec::new();
            let parts = parts_to_anthropic(items, &mut sink);
            let content = if parts.is_empty() { json!("") } else { Value::Array(parts) };
            json!({ "type": "tool_result", "tool_use_id": id, "content": content })
        }
        other => json!({
            "type": "tool_result",
            "tool_use_id": id,
            "content": other.map(text_of).unwrap_or_default(),
        }),
    }
}

fn rnd_suffix(n: usize) -> String {
    uuid::Uuid::new_v4().simple().to_string().chars().take(n).collect()
}

/// to_anthropic 把一条 OpenAI chat.completions 请求变成 Messages 请求体。
///
/// 顺序很重要：OpenAI 的 system 轮可以在任何位置，Anthropic 只有一个 system 字符串；
/// OpenAI 的 assistant tool_calls 和随后的 tool 结果是两条消息，Anthropic 的 tool_use
/// 与 tool_result 是普通轮里的 block。弄错这一点的表现是：一次工具调用被 400 回来说
/// 缺少 tool_result。
/// effort_request 取客户端说的思考强度。OpenAI 形状的客户端用 reasoning_effort，
/// Anthropic 形状的客户端把档位塞在 thinking.effort（或 level）里。取值统一小写。
fn effort_request(obj: &Map<String, Value>) -> Option<String> {
    let read = |v: &Value| {
        v.as_str()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
    };
    if let Some(s) = obj.get("reasoning_effort").and_then(read) {
        return Some(s);
    }
    let t = obj.get("thinking")?;
    t.get("effort")
        .and_then(read)
        .or_else(|| t.get("level").and_then(read))
}

/// normalize_effort 把各家各户的写法收成上游认的三档。上游只认 low / high / max
///（发别的值会被 1210 拒：This model always engages in thinking; please use low, high, or max），
/// 中间那档没有 medium，所以 medium 落到 high，xhigh 落到 max。
fn normalize_effort(raw: &str) -> Option<&'static str> {
    match raw {
        "minimal" | "low" => Some("low"),
        "medium" | "high" | "auto" | "default" => Some("high"),
        "xhigh" | "max" => Some("max"),
        // off / none / 不认识的值：上游关不掉思考，只能不发这个字段
        _ => None,
    }
}

pub fn to_anthropic(body: &Value, default_max_tokens: u32) -> Result<(Value, Vec<String>), String> {
    let mut warn: Vec<String> = Vec::new();
    let obj = body
        .as_object()
        .ok_or_else(|| "a request body is required".to_string())?;
    let model = obj
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "model is required".to_string())?;
    let messages = match obj.get("messages") {
        Some(Value::Array(a)) if !a.is_empty() => a.clone(),
        _ => return Err("messages must not be empty".into()),
    };
    if let Some(n) = obj.get("n").and_then(|n| n.as_f64()) {
        if n > 1.0 {
            return Err("n > 1 is not supported: ZCode's plans answer one choice per request".into());
        }
    }

    let mut system: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for m in messages.iter() {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "system" || role == "developer" {
            let t = text_of(m.get("content").unwrap_or(&Value::Null));
            if !t.trim().is_empty() {
                system.push(t);
            }
            continue;
        }
        if role == "tool" {
            // 连续的 tool 结果是一轮 user，Anthropic 要这样
            let block = tool_result_block(m);
            let merge = out
                .last_mut()
                .filter(|prev| is_tool_results(prev))
                .and_then(|prev| prev.get_mut("content").and_then(|c| c.as_array_mut()));
            match merge {
                Some(items) => items.push(block),
                None => out.push(json!({ "role": "user", "content": [block] })),
            }
            continue;
        }
        if role == "assistant" {
            let mut blocks: Vec<Value> = Vec::new();
            let t = text_of(m.get("content").unwrap_or(&Value::Null));
            if !t.is_empty() {
                blocks.push(json!({ "type": "text", "text": t }));
            }
            if let Some(calls) = m.get("tool_calls").and_then(|c| c.as_array()) {
                for c in calls {
                    let f = match c.get("function") {
                        Some(f) => f,
                        None => continue,
                    };
                    let name = match f.get("name").and_then(|n| n.as_str()) {
                        Some(n) => n,
                        None => continue,
                    };
                    let args = f.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
                    let input = if args.is_empty() {
                        json!({})
                    } else {
                        match serde_json::from_str::<Value>(args) {
                            Ok(v) => v,
                            Err(_) => {
                                // 解析不了的参数是客户端的错，但在这里抛出去会丢掉整轮：
                                // 发它想要的东西
                                warn.push(format!(
                                    "tool call {name} had arguments that are not JSON; sent as an empty object"
                                ));
                                json!({})
                            }
                        }
                    };
                    let id = c
                        .get("id")
                        .and_then(|i| i.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| format!("call_{}", rnd_suffix(8)));
                    blocks.push(json!({ "type": "tool_use", "id": id, "name": name, "input": input }));
                }
            }
            if blocks.is_empty() {
                continue;
            }
            let content = if blocks.len() == 1 && blocks[0].get("type").and_then(|t| t.as_str()) == Some("text") {
                blocks[0].get("text").cloned().unwrap_or(json!(""))
            } else {
                Value::Array(blocks)
            };
            out.push(json!({ "role": "assistant", "content": content }));
            continue;
        }
        if role == "user" {
            let parts = match m.get("content") {
                Some(Value::Array(items)) => parts_to_anthropic(items, &mut warn),
                Some(Value::String(s)) if !s.is_empty() => vec![json!({ "type": "text", "text": s })],
                _ => Vec::new(),
            };
            if parts.is_empty() {
                continue;
            }
            // 紧跟在 tool 结果后面说的话属于同一轮：Anthropic 把一轮读成一个 block 列表，
            // 拆开会让模型把工具结果和问题分开答
            if is_tool_results(out.last().unwrap_or(&Value::Null)) {
                let items = out
                    .last_mut()
                    .and_then(|p| p.get_mut("content"))
                    .and_then(|c| c.as_array_mut());
                if let Some(items) = items {
                    items.extend(parts);
                }
                continue;
            }
            let content = if parts.len() == 1 && parts[0].get("type").and_then(|t| t.as_str()) == Some("text") {
                parts[0].get("text").cloned().unwrap_or(json!(""))
            } else {
                Value::Array(parts)
            };
            out.push(json!({ "role": "user", "content": content }));
            continue;
        }
        warn.push(format!("dropped a message with role {role}"));
    }
    if out.is_empty() {
        return Err("messages must carry at least one user or assistant turn".into());
    }
    let sys = system.join("\n\n");

    let mut req = Map::new();
    req.insert("model".into(), Value::String(model));
    let max_tokens = obj
        .get("max_tokens")
        .and_then(|m| m.as_f64())
        .filter(|m| *m > 0.0)
        .map(|m| m.floor() as u64)
        .unwrap_or(default_max_tokens as u64);
    req.insert("max_tokens".into(), json!(max_tokens));
    req.insert("messages".into(), Value::Array(out));
    req.insert(
        "stream".into(),
        json!(obj.get("stream").and_then(|s| s.as_bool()).unwrap_or(false)),
    );
    if !sys.is_empty() {
        req.insert("system".into(), Value::String(sys));
    }
    if let Some(t) = obj.get("temperature").and_then(|t| t.as_f64()) {
        // OpenAI 允许 0..2，Anthropic 0..1：截断比 400 少损失一点
        if t > 1.0 {
            warn.push(format!("temperature {t} clamped to 1"));
        }
        req.insert("temperature".into(), json!(t.clamp(0.0, 1.0)));
    }
    if let Some(p) = obj.get("top_p").and_then(|p| p.as_f64()) {
        req.insert("top_p".into(), json!(p.clamp(0.0, 1.0)));
    }
    match obj.get("stop") {
        Some(Value::String(s)) if !s.is_empty() => {
            req.insert("stop_sequences".into(), json!([s]));
        }
        Some(Value::Array(items)) => {
            let seq: Vec<Value> = items
                .iter()
                .filter_map(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .take(8)
                .map(|s| Value::String(s.to_string()))
                .collect();
            if !seq.is_empty() {
                if items.len() > 8 {
                    warn.push("kept the first 8 stop sequences".into());
                }
                req.insert("stop_sequences".into(), Value::Array(seq));
            }
        }
        _ => {}
    }
    if let Some(tools) = obj.get("tools").and_then(|t| t.as_array()) {
        let mut kept: Vec<Value> = Vec::new();
        for t in tools {
            let f = match t.get("function") {
                Some(f) if t.get("type").and_then(|x| x.as_str()) == Some("function") => Some(f),
                _ => None,
            };
            let f = match f {
                Some(f) => f,
                None => {
                    warn.push("dropped a tool that is not a function".into());
                    continue;
                }
            };
            let name = match f.get("name").and_then(|n| n.as_str()) {
                Some(n) => n,
                None => {
                    warn.push("dropped a tool that is not a function".into());
                    continue;
                }
            };
            let mut tool = Map::new();
            tool.insert("name".into(), Value::String(name.to_string()));
            if let Some(d) = f.get("description").and_then(|d| d.as_str()) {
                if !d.is_empty() {
                    tool.insert("description".into(), Value::String(d.to_string()));
                }
            }
            let schema = f
                .get("parameters")
                .filter(|p| p.is_object())
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            tool.insert("input_schema".into(), schema);
            kept.push(Value::Object(tool));
        }
        if !kept.is_empty() {
            req.insert("tools".into(), Value::Array(kept));
            match obj.get("tool_choice") {
                Some(Value::String(c)) if c == "auto" => {
                    req.insert("tool_choice".into(), json!({ "type": "auto" }));
                }
                Some(Value::String(c)) if c == "required" => {
                    req.insert("tool_choice".into(), json!({ "type": "any" }));
                }
                Some(Value::String(c)) if c == "none" => {
                    req.remove("tools");
                }
                Some(Value::Object(c)) => {
                    if let Some(name) = c.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
                        req.insert("tool_choice".into(), json!({ "type": "tool", "name": name }));
                    }
                }
                _ => {}
            }
        }
    }
    // 思考强度：上游认的是 output_config.effort（Anthropic 形状的字段），OpenAI 形状的
    // 客户端说的是 reasoning_effort。不翻这一下，客户端选的 Low / High / Max 到不了模型，
    // 一律按套餐默认档跑。
    if let Some(raw) = effort_request(obj) {
        match normalize_effort(&raw) {
            Some(level) => {
                req.insert("output_config".into(), json!({ "effort": level }));
            }
            None => warn.push(format!(
                "reasoning effort {raw} has no ZCode equivalent (the plans only offer low/high/max); left the plan default"
            )),
        }
    }
    Ok((Value::Object(req), warn))
}

/// usage_of 是 Anthropic 的用量换算成 OpenAI 的，token 和缓存明细都算上。
pub fn usage_of(u: &Value) -> (Value, Value) {
    // 一律整数：token 没有半个，而客户端按整数算钱
    let n = |k: &str| -> u64 {
        u.get(k)
            .and_then(|v| v.as_f64())
            .map(|f| f.round().max(0.0) as u64)
            .unwrap_or(0)
    };
    let input = n("input_tokens");
    let output = n("output_tokens");
    let cached = n("cache_read_input_tokens");
    let written = n("cache_creation_input_tokens");
    let prompt = input + cached + written;
    let mut openai = Map::new();
    openai.insert("prompt_tokens".into(), json!(prompt));
    openai.insert("completion_tokens".into(), json!(output));
    openai.insert("total_tokens".into(), json!(prompt + output));
    if cached > 0 || written > 0 {
        openai.insert(
            "prompt_tokens_details".into(),
            json!({ "cached_tokens": cached }),
        );
    }
    (
        json!({
            "input_tokens": input,
            "output_tokens": output,
            "cache_read_input_tokens": cached,
            "cache_creation_input_tokens": written,
        }),
        Value::Object(openai),
    )
}

/// to_openai 把一个 Messages 响应变成 chat.completion。
pub fn to_openai(message: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: Vec<Value> = Vec::new();
    if let Some(blocks) = message.get("content").and_then(|c| c.as_array()) {
        for b in blocks {
            match b.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "text" => {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                    }
                }
                "thinking" => {
                    if let Some(t) = b.get("thinking").and_then(|t| t.as_str()) {
                        reasoning.push_str(t);
                    }
                }
                "tool_use" => {
                    let id = b
                        .get("id")
                        .and_then(|i| i.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| format!("call_{}", calls.len()));
                    let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let input = b.get("input").cloned().unwrap_or(json!({}));
                    calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": input.to_string() },
                    }));
                }
                _ => {}
            }
        }
    }
    let usage = usage_of(message.get("usage").unwrap_or(&Value::Null));
    let mut msg = Map::new();
    msg.insert("role".into(), json!("assistant"));
    msg.insert(
        "content".into(),
        if text.is_empty() {
            if calls.is_empty() { json!("") } else { Value::Null }
        } else {
            Value::String(text)
        },
    );
    if !reasoning.is_empty() {
        msg.insert("reasoning_content".into(), Value::String(reasoning));
    }
    if !calls.is_empty() {
        msg.insert("tool_calls".into(), Value::Array(calls));
    }
    let id = message
        .get("id")
        .and_then(|i| i.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("chatcmpl-{}", rnd_suffix(10)));
    let named = if model.is_empty() {
        message.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string()
    } else {
        model.to_string()
    };
    json!({
        "id": id,
        "object": "chat.completion",
        "created": now_secs(),
        "model": named,
        "choices": [{
            "index": 0,
            "message": Value::Object(msg),
            "finish_reason": finish_reason(message.get("stop_reason").and_then(|s| s.as_str())),
            "logprobs": Value::Null,
        }],
        "usage": usage.1,
    })
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// sse_text 是一个 server-sent event，按 OpenAI 客户端读的样子。
pub fn sse_text(v: &Value) -> String {
    match v {
        Value::String(s) => format!("data: {s}\n\n"),
        other => format!("data: {other}\n\n"),
    }
}

/// StreamTranslator 把厂商的事件流当场翻成 OpenAI 的 chunk：喂给它手里的字节，
/// 写出它返回的东西，上游结束后调一次 finish。
pub struct StreamTranslator {
    pub model: String,
    named: bool,
    pub id: String,
    created: i64,
    buffer: String,
    block_types: std::collections::HashMap<i64, String>,
    tool_index: std::collections::HashMap<i64, i64>,
    next_tool: i64,
    finish: Option<String>,
    pub usage: Option<Value>,
    started: bool,
}

impl StreamTranslator {
    pub fn new(model: &str, id: Option<String>, created: i64) -> Self {
        Self {
            model: model.to_string(),
            named: !model.is_empty(),
            id: id.unwrap_or_else(|| format!("chatcmpl-{}", rnd_suffix(10))),
            created,
            buffer: String::new(),
            block_types: Default::default(),
            tool_index: Default::default(),
            next_tool: 0,
            finish: None,
            usage: None,
            started: false,
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>, extra: Option<Value>) -> String {
        let mut o = Map::new();
        o.insert("id".into(), json!(self.id));
        o.insert("object".into(), json!("chat.completion.chunk"));
        o.insert("created".into(), json!(self.created));
        o.insert("model".into(), json!(self.model));
        o.insert(
            "choices".into(),
            json!([{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
                "logprobs": Value::Null,
            }]),
        );
        if let Some(Value::Object(e)) = extra {
            for (k, v) in e {
                o.insert(k, v);
            }
        }
        sse_text(&Value::Object(o))
    }

    /// feed 吃下手里的字节，给出它们凑齐的那些事件。
    pub fn feed(&mut self, text: &str) -> String {
        self.buffer.push_str(text);
        let mut out = String::new();
        while let Some(at) = self.buffer.find("\n\n") {
            let raw: String = self.buffer.drain(..at + 2).collect();
            out.push_str(&self.event(&raw[..raw.len() - 2]));
        }
        out
    }

    fn event(&mut self, raw: &str) -> String {
        let mut name = String::new();
        let mut data: Vec<&str> = Vec::new();
        for line in raw.split('\n') {
            if let Some(rest) = line.strip_prefix("event:") {
                name = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data.push(rest.trim());
            }
        }
        if data.is_empty() {
            return String::new();
        }
        let j: Value = match serde_json::from_str(&data.join("\n")) {
            Ok(v) => v,
            Err(_) => return String::new(),
        };
        if name.is_empty() {
            if let Some(t) = j.get("type").and_then(|t| t.as_str()) {
                name = t.to_string();
            }
        }
        match name.as_str() {
            "message_start" => {
                let message = j.get("message").cloned().unwrap_or(json!({}));
                if let Some(id) = message.get("id").and_then(|i| i.as_str()) {
                    self.id = id.to_string();
                }
                if !self.named {
                    if let Some(m) = message.get("model").and_then(|m| m.as_str()) {
                        self.model = m.to_string();
                    }
                }
                if let Some(u) = message.get("usage") {
                    if !u.is_null() {
                        self.usage = Some(u.clone());
                    }
                }
                self.started = true;
                self.chunk(json!({ "role": "assistant", "content": "" }), None, None)
            }
            "content_block_start" => {
                let block = j.get("content_block").cloned().unwrap_or(json!({}));
                let index = j.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                let bt = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                self.block_types.insert(index, bt.to_string());
                if bt == "tool_use" {
                    let idx = self.next_tool;
                    self.next_tool += 1;
                    self.tool_index.insert(index, idx);
                    let id = block
                        .get("id")
                        .and_then(|i| i.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| format!("call_{idx}"));
                    let nm = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    return self.chunk(
                        json!({ "tool_calls": [{ "index": idx, "id": id, "type": "function", "function": { "name": nm, "arguments": "" } }] }),
                        None,
                        None,
                    );
                }
                String::new()
            }
            "content_block_delta" => {
                let d = j.get("delta").cloned().unwrap_or(json!({}));
                match d.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text_delta" => match d.get("text").and_then(|t| t.as_str()) {
                        Some(t) => self.chunk(json!({ "content": t }), None, None),
                        None => String::new(),
                    },
                    "thinking_delta" => match d.get("thinking").and_then(|t| t.as_str()) {
                        Some(t) => self.chunk(json!({ "reasoning_content": t }), None, None),
                        None => String::new(),
                    },
                    "input_json_delta" => {
                        let index = j.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                        let idx = self.tool_index.get(&index).copied();
                        match (idx, d.get("partial_json").and_then(|p| p.as_str())) {
                            (Some(idx), Some(pj)) => self.chunk(
                                json!({ "tool_calls": [{ "index": idx, "function": { "arguments": pj } }] }),
                                None,
                                None,
                            ),
                            _ => String::new(),
                        }
                    }
                    _ => String::new(),
                }
            }
            "message_delta" => {
                if let Some(sr) = j.get("delta").and_then(|d| d.get("stop_reason")).and_then(|s| s.as_str()) {
                    self.finish = Some(finish_reason(Some(sr)));
                }
                if let Some(u) = j.get("usage") {
                    if let Value::Object(o) = u {
                        let mut merged = match self.usage.take() {
                            Some(Value::Object(m)) => m,
                            _ => Map::new(),
                        };
                        for (k, v) in o {
                            merged.insert(k.clone(), v.clone());
                        }
                        self.usage = Some(Value::Object(merged));
                    }
                }
                String::new()
            }
            "error" => {
                let msg = j
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("upstream error");
                let typ = j
                    .get("error")
                    .and_then(|e| e.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("upstream_error");
                sse_text(&json!({ "error": { "message": msg, "type": typ, "code": typ } }))
            }
            _ => String::new(),
        }
    }

    /// finish 收尾：最后一个 chunk 带 finish reason 和 usage，然后是 OpenAI 客户端
    /// 停在的那个哨兵。
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        let extra = self.usage.take().map(|u| json!({ "usage": usage_of(&u).1 }));
        let fr = self.finish.clone().unwrap_or_else(|| "stop".into());
        out.push_str(&self.chunk(json!({}), Some(&fr), extra));
        out.push_str(&sse_text(&json!("[DONE]")));
        out
    }
}
