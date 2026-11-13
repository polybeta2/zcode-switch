// dress.rs — 一条 Messages 请求按 ZCode 客户端的样子穿好，以及它带的头。
//
// 从 gateway/dress.mjs 逐条移植（它本身移植自 magpie zcode 插件）。两条规则由厂商
// 强制，都是上游用事故换来的：
//   - 系统块恰好三个，前两个逐字是 ZCode 的：第四个会被 405/3012 拒（plugins#343）；
//   - metadata.user_id 里写着这台设备的 id，否则套餐的提示词缓存基本落空，每一轮都
//     按全价再付一次（plugins#387）。
use crate::gateway::prompt::prompt;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub const APP_VERSION: &str = "3.17.2";
pub const ZCODE_ORIGIN: &str = "https://zcode.z.ai";
pub const START_BASE: &str = "https://zcode.z.ai/api/v1/zcode-plan/anthropic";

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn sha_hex(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

/// 由种子生成一个 v4 形状的 UUID：同一个种子每次都得到同一个 id，会话因此能跨重启。
pub fn uuid_of(seed: &str) -> String {
    let h = sha_hex(seed);
    let variant = (u8::from_str_radix(&h[16..17], 16).unwrap_or(0) & 3) | 8;
    format!(
        "{}-{}-4{}-{:x}{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        variant,
        &h[17..20],
        &h[20..32]
    )
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, c) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *c != b'-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

/// 一条请求带的 x-session-id：纯粹的 UUID，同一轮对话里每次都是同一个。
pub fn session_of(id: &str, device: &str) -> String {
    let own = id.trim();
    if is_uuid(own) {
        return own.to_lowercase();
    }
    if own.is_empty() {
        uuid_of(&format!("zcode-session-device\0{device}"))
    } else {
        uuid_of(&format!("zcode-session\0{own}"))
    }
}

/// 一条请求的 x-query-id：用户这一问的 UUID，同一轮的每条请求相同，换一轮就换一个。
/// 读不出 Messages 结构的请求体给空串，也就是不发这个头。
pub fn query_of(text: &str, session: &str) -> String {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let msgs = match v.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return String::new(),
    };
    let mut n = 0usize;
    for m in msgs {
        if m.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let counts = match m.get("content") {
            Some(Value::String(s)) => !s.trim().is_empty(),
            Some(Value::Array(items)) => items
                .iter()
                .any(|x| x.get("type").and_then(|t| t.as_str()) != Some("tool_result")),
            _ => false,
        };
        if counts {
            n += 1;
        }
    }
    if n == 0 {
        String::new()
    } else {
        uuid_of(&format!("zcode-query\0{session}\0{n}"))
    }
}

/// metadata.user_id 按 ZCode 的写法：设备 id、空的账号 uuid、所属会话。
pub fn user_id_of(device: &str, session: &str) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    format!(
        "{{\"device_id\":{},\"account_uuid\":\"\",\"session_id\":{}}}",
        q(device),
        q(session)
    )
}

fn platform_tag() -> String {
    crate::zcrypto::node_platform_for(std::env::consts::OS).to_string()
}

fn os_category() -> &'static str {
    match platform_tag().as_str() {
        "darwin" => "macos",
        "win32" => "windows",
        _ => "linux",
    }
}

fn arch_tag() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// environment 是 ZCode 的环境段。工作目录和 git 只在调用方知道时才写：
/// 写错一个路径比不写更糟（plugins#86），而网关不知道客户端在哪。
fn environment() -> String {
    let e = &prompt().environment;
    let platform = platform_tag();
    let ver = crate::quota::os_version().unwrap_or_else(|| "unknown".into());
    [
        e.heading.clone(),
        e.invoked_line.clone(),
        format!(" - {}: {}", e.platform_label, platform),
        format!(" - {}: {} {} {}", e.os_version_label, platform, ver, arch_tag()),
        format!(" - {}", e.untrusted_note),
    ]
    .join("\n")
}

fn context_prefix() -> Value {
    let line = prompt().context.current_date_line.replace("{date}", &today());
    json!({
        "role": "user",
        "content": [{ "type": "text", "text": format!("<system-reminder>{line}</system-reminder>") }]
    })
}

fn cached(t: &str) -> Value {
    json!({ "type": "text", "text": t, "cache_control": { "type": "ephemeral" } })
}

fn strip_cache(v: &mut Value) {
    if let Some(items) = v.as_array_mut() {
        for x in items.iter_mut() {
            if let Some(o) = x.as_object_mut() {
                o.remove("cache_control");
            }
        }
    }
}

/// dress 把一条 Messages 请求体穿成 ZCode 发出的样子：三个带缓存标记的系统块、
/// 第一轮的日期前缀、只有最后一轮带缓存标记、tools 上不带、metadata 写设备。
/// 读不出 Messages 结构的，原样返回。
pub fn dress(text: &str, site: &str, device: &str, session: &str) -> String {
    if !text.contains("messages") {
        return text.to_string();
    }
    let mut b: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return text.to_string(),
    };
    {
        let obj = match b.as_object_mut() {
            Some(o) => o,
            None => return text.to_string(),
        };
        let turns = match obj.get("messages").and_then(|m| m.as_array()) {
            Some(m) => m.clone(),
            None => return text.to_string(),
        };
        let model = obj
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let mut own: Vec<String> = Vec::new();
        match obj.get("system") {
            Some(Value::String(s)) => {
                if !s.trim().is_empty() {
                    own.push(s.clone());
                }
            }
            Some(Value::Array(items)) => {
                for x in items {
                    if x.get("type").and_then(|t| t.as_str()) != Some("text") {
                        continue;
                    }
                    if let Some(t) = x.get("text").and_then(|t| t.as_str()) {
                        if !t.trim().is_empty() {
                            own.push(t.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
        let skill = obj
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|ts| {
                ts.iter()
                    .any(|t| t.get("name").and_then(|n| n.as_str()) == Some("Skill"))
            })
            .unwrap_or(false);
        let provider = if site == "bigmodel" { "bigmodel-api" } else { "zai-api" };
        let p = prompt();
        let mut dynamic: Vec<String> = vec![p.dynamic_behavior.clone()];
        if skill {
            dynamic.push(p.session_guidance.clone());
        }
        dynamic.push(environment());
        if !model.is_empty() {
            dynamic.push(
                p.powered_by
                    .replace("{provider}", provider)
                    .replace("{model}", &model),
            );
        }
        dynamic.push(p.context_management.clone());
        dynamic.extend(own);
        obj.insert(
            "system".into(),
            json!([
                cached(&p.cli_prefix),
                cached(&p.stable_sections.join("\n\n")),
                cached(&format!("\n\n{}", dynamic.join("\n\n"))),
            ]),
        );
        let mut all: Vec<Value> = vec![context_prefix()];
        all.extend(turns);
        let mut last: Option<usize> = None;
        for (i, m) in all.iter_mut().enumerate() {
            if m.get("role").and_then(|r| r.as_str()) == Some("system") {
                continue;
            }
            last = Some(i);
            if let Some(c) = m.get_mut("content") {
                strip_cache(c);
            }
        }
        if let Some(i) = last {
            let m = &mut all[i];
            let mut replaced: Option<Value> = None;
            if let Some(Value::String(s)) = m.get("content") {
                replaced = Some(json!([cached(s)]));
            }
            match replaced {
                Some(v) => {
                    if let Some(o) = m.as_object_mut() {
                        o.insert("content".into(), v);
                    }
                }
                None => {
                    if let Some(items) = m.get_mut("content").and_then(|c| c.as_array_mut()) {
                        if let Some(x) = items.last_mut() {
                            if let Some(o) = x.as_object_mut() {
                                if !o.contains_key("cache_control") {
                                    o.insert("cache_control".into(), json!({ "type": "ephemeral" }));
                                }
                            }
                        }
                    }
                }
            }
        }
        obj.insert("messages".into(), Value::Array(all));
        if let Some(items) = obj.get_mut("tools").and_then(|t| t.as_array_mut()) {
            for t in items.iter_mut() {
                if let Some(o) = t.as_object_mut() {
                    o.remove("cache_control");
                }
            }
        }
        let mut meta: Map<String, Value> = match obj.get("metadata") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        meta.insert("user_id".into(), Value::String(user_id_of(device, session)));
        obj.insert("metadata".into(), Value::Object(meta));
    }
    serde_json::to_string(&b).unwrap_or_else(|_| text.to_string())
}

/// marked 是给 coding plan 用的：那些端点不需要整套 dress，但需要设备，
/// 否则缓存落到别处。
pub fn marked(text: &str, device: &str, session: &str) -> String {
    if device.is_empty() || !text.contains("messages") {
        return text.to_string();
    }
    let mut b: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return text.to_string(),
    };
    {
        let obj = match b.as_object_mut() {
            Some(o) => o,
            None => return text.to_string(),
        };
        if !obj.get("messages").map(|m| m.is_array()).unwrap_or(false) {
            return text.to_string();
        }
        let mut meta: Map<String, Value> = match obj.get("metadata") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        if meta
            .get("user_id")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            return text.to_string();
        }
        meta.insert("user_id".into(), Value::String(user_id_of(device, session)));
        obj.insert("metadata".into(), Value::Object(meta));
    }
    serde_json::to_string(&b).unwrap_or_else(|_| text.to_string())
}

/// Start Plan 一条请求的头：ZCode 自己的那几个，和不属于任何 agent 的那几个。
/// 客户端带来的头一个也不带——anthropic-beta、x-app、x-stainless-* 都是 agent 的，
/// ZCode 在这个端点上不发它们。
pub fn start_headers(jwt: &str, session_id: &str, query_id: &str, app_version: &str) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = vec![
        ("content-type".into(), "application/json".into()),
        ("Authorization".into(), format!("Bearer {jwt}")),
        ("anthropic-version".into(), "2023-06-01".into()),
        ("HTTP-Referer".into(), ZCODE_ORIGIN.into()),
        (
            "User-Agent".into(),
            format!("ZCode/{app_version} ai-sdk/anthropic/3.0.81"),
        ),
        ("X-ZCode-App-Version".into(), app_version.into()),
        ("X-Title".into(), "Z Code@electron".into()),
        ("X-Release-Channel".into(), "production".into()),
        ("X-Client-Language".into(), crate::quota::ZCODE_LANG.into()),
        ("X-Client-Timezone".into(), crate::quota::client_timezone()),
        ("X-ZCode-Agent".into(), "glm".into()),
        ("X-Platform".into(), crate::quota::client_platform()),
        ("X-Os-Category".into(), os_category().into()),
        (
            "X-Os-Version".into(),
            crate::quota::os_version().unwrap_or_else(|| "unknown".into()),
        ),
        ("x-request-id".into(), uuid::Uuid::new_v4().to_string()),
        ("x-zcode-session-type".into(), "main".into()),
        ("x-zcode-trace-id".into(), uuid::Uuid::new_v4().to_string()),
        ("x-zcode-query-source".into(), "main_turn".into()),
    ];
    if !session_id.is_empty() {
        h.push(("x-session-id".into(), session_id.to_string()));
    }
    if !query_id.is_empty() {
        h.push(("x-query-id".into(), query_id.to_string()));
    }
    h
}

/// coding plan / API key 的请求带的是应用自己查额度时那一套指纹
/// （quota.rs: zai_headers_with_version），Start Plan 要的比这多，所以它单独一套。
pub fn app_headers(token: &str, device: &str, app_version: &str) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = vec![
        ("content-type".into(), "application/json".into()),
        ("Authorization".into(), format!("Bearer {token}")),
        ("anthropic-version".into(), "2023-06-01".into()),
        ("User-Agent".into(), format!("ZCode/{app_version}")),
        ("HTTP-Referer".into(), ZCODE_ORIGIN.into()),
        ("X-Title".into(), "Z Code@electron".into()),
        ("X-ZCode-App-Version".into(), app_version.into()),
        ("X-Platform".into(), crate::quota::client_platform()),
        ("X-Release-Channel".into(), "production".into()),
        ("X-Client-Language".into(), crate::quota::ZCODE_LANG.into()),
        ("X-Client-Timezone".into(), crate::quota::client_timezone()),
        ("X-Os-Category".into(), os_category().into()),
        (
            "X-Os-Version".into(),
            crate::quota::os_version().unwrap_or_else(|| "unknown".into()),
        ),
        ("x-request-id".into(), uuid::Uuid::new_v4().to_string()),
    ];
    if !device.is_empty() {
        h.push(("X-Device-Mid".into(), device.to_string()));
    }
    h
}
