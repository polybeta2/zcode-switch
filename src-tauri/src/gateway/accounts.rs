// accounts.rs — 本应用已经存下的账号，变成网关能发出去的目标。
//
// 每个账号文件里存着保存那一刻的 ZCode config.json：套餐的 key 和它的 baseURL
// 就在里面（provider["builtin:zai-start-plan"].options.apiKey），旁边是这台账号
// 假装成的设备（virtual_device_mid）。这里不写、不解密：credentials.json 的值由
// 操作系统封着，config 快照是明文，读的就是它。
use crate::gateway::dress::START_BASE;
use crate::store::{list_accounts, Paths};
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug)]
struct Plan {
    plan: &'static str,
    site: &'static str,
    dress: bool,
    rank: u8,
}

/// PLANS 说明 ZCode 的每个 provider 是什么，也说明一条请求该先找谁：
/// Start Plan 是厂商盯得最紧的那条，coding plan 是普通订阅，API key 是付费的。
fn plan_of(provider: &str) -> Option<Plan> {
    Some(match provider {
        "builtin:zai-start-plan" => Plan { plan: "start", site: "zai", dress: true, rank: 0 },
        "builtin:bigmodel-start-plan" => Plan { plan: "start", site: "bigmodel", dress: true, rank: 0 },
        "builtin:bigmodel-coding-plan" => Plan { plan: "coding", site: "bigmodel", dress: false, rank: 1 },
        "builtin:zai-coding-plan" => Plan { plan: "coding", site: "zai", dress: false, rank: 1 },
        "builtin:bigmodel" => Plan { plan: "api", site: "bigmodel", dress: false, rank: 2 },
        "builtin:zai" => Plan { plan: "api", site: "zai", dress: false, rank: 2 },
        _ => return None,
    })
}

/// 一个可以发请求的账号套餐。
#[derive(Clone, Debug)]
pub struct Target {
    pub account_id: String,
    pub account_name: String,
    pub provider: String,
    pub plan: &'static str,
    pub site: &'static str,
    pub dress: bool,
    pub rank: u8,
    pub enabled: bool,
    pub key: String,
    pub base: String,
    pub models: Vec<String>,
    pub subject: String,
    pub device: String,
    pub label: String,
    pub aliases: Vec<String>,
}

impl Target {
    pub fn endpoint(&self) -> String {
        let base = if self.base.trim().is_empty() {
            START_BASE.to_string()
        } else {
            self.base.trim().to_string()
        };
        format!("{}/v1/messages", base.trim_end_matches('/'))
    }
}

/// usable_key 是这里能发的 key：非空，而且不是 credentials.json 里那种被封起来的值
/// （enc:v1: 是本应用的，enc:v2: 是操作系统的）——发出去只会换回一个空 body 的 401。
fn usable_key(k: &str) -> bool {
    let t = k.trim();
    t.len() > 20 && !t.starts_with("enc:v1") && !t.starts_with("enc:v2")
}

/// jwt_subject 是这把 key 属于哪个 ZCode 用户。同一次登录会被存成好几个名字，每个名字
/// 都有自己的新 token，所以 key 字符串分不出它们是同一个账号；token 的 subject 可以。
/// 一个账号不能当成好几个发出去：厂商的并发限制按账号算，防火墙的按地址算。
pub fn jwt_subject(key: &str) -> String {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.len() != 3 {
        return String::new();
    }
    let raw = match URL_SAFE_NO_PAD.decode(parts[1]) {
        Ok(v) => v,
        Err(_) => match URL_SAFE.decode(parts[1]) {
            Ok(v) => v,
            Err(_) => return String::new(),
        },
    };
    let v: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    for k in ["sub", "user_id"] {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// targets_from_config 按应用自己的读法读一份 ZCode config.json：每个带 key 的
/// provider 条目都是一个目标，条目自己的 baseURL 就是它去哪。
pub fn targets_from_config(
    config: Option<&Value>,
    account_id: &str,
    account_name: &str,
    device: &str,
) -> Vec<Target> {
    let providers = match config.and_then(|c| c.get("provider")).and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for (provider_id, entry) in providers {
        let known = match plan_of(provider_id) {
            Some(k) => k,
            None => continue,
        };
        let options = match entry.get("options").and_then(|o| o.as_object()) {
            Some(o) => o,
            None => continue,
        };
        let key = match options.get("apiKey").and_then(|k| k.as_str()) {
            Some(k) if usable_key(k) => k.trim().to_string(),
            _ => continue,
        };
        let base = options
            .get("baseURL")
            .and_then(|b| b.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        // 条目声明了这个套餐服务的模型，拼写就是厂商要的那个：换一个名字会被 3006 拒
        let models: Vec<String> = entry
            .get("models")
            .and_then(|m| m.as_object())
            .map(|m| m.keys().filter(|k| !k.trim().is_empty()).cloned().collect())
            .unwrap_or_default();
        let subject = jwt_subject(&key);
        let label = if known.plan == "start" {
            format!("{account_name} · {}", known.plan)
        } else {
            format!(
                "{account_name} · {} ({})",
                known.plan,
                provider_id.trim_start_matches("builtin:")
            )
        };
        out.push(Target {
            account_id: account_id.to_string(),
            account_name: account_name.to_string(),
            provider: provider_id.clone(),
            plan: known.plan,
            site: known.site,
            dress: known.dress,
            rank: known.rank,
            enabled: entry.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true),
            key,
            base,
            models,
            subject,
            device: device.to_string(),
            label,
            aliases: Vec::new(),
        });
    }
    out
}

/// load_targets 是这些选项允许的全部目标，最好的在前，一个 key 只有一个。
///
/// 先找 Start Plan 是对意图的猜测，不是对计费的断言：想要订阅的人会说（plans），
/// 只想要一个账号的人也会说（accounts：id 或名字）。
pub fn load_targets(
    paths: &Paths,
    include_live: bool,
    plans: Option<&[String]>,
    accounts: Option<&[String]>,
) -> Vec<Target> {
    let mut all: Vec<Target> = Vec::new();
    let list = list_accounts(paths).unwrap_or_default();
    for a in list.iter() {
        if let Some(want) = accounts {
            if !want.iter().any(|x| x == &a.id || x == &a.name) {
                continue;
            }
        }
        let device = a.virtual_device_mid.clone().unwrap_or_default();
        all.extend(targets_from_config(a.config.as_ref(), &a.id, &a.name, &device));
    }
    if include_live {
        if let Some(cfg) = crate::store::read_live_config(paths) {
            let device = std::fs::read_to_string(paths.live_telemetry())
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v.get("deviceMid").and_then(|m| m.as_str()).map(String::from))
                .unwrap_or_default();
            let extra = targets_from_config(Some(&cfg), "live", "本机 ZCode", &device);
            // 账号文件里已经有的 key 是同一个套餐、同一台设备：再发一次只是把一份额度
            // 当成两份用
            let seen: HashSet<String> = all.iter().map(|t| t.key.clone()).collect();
            for t in extra {
                if !seen.contains(&t.key) {
                    all.push(t);
                }
            }
        }
    }
    // 停用的套餐不发：配置里写着 enabled: false 的条目，网关就不该再拿它去花额度。
    // 以前这个字段只用来排序，于是被停掉的 coding plan 照样吃到真请求，回 1113。
    all.retain(|t| t.enabled);
    if let Some(want) = plans {
        let set: HashSet<&str> = want.iter().map(|s| s.as_str()).collect();
        all.retain(|t| set.contains(t.plan));
    }
    all.sort_by(|x, y| {
        x.rank
            .cmp(&y.rank)
            .then(y.enabled.cmp(&x.enabled))
            .then_with(|| x.account_name.to_lowercase().cmp(&y.account_name.to_lowercase()))
    });
    // 一个账号一个目标：同一个登录取的不同名字共享 subject，第一个占位，其余折进去
    let mut by_identity: Vec<(String, Target)> = Vec::new();
    for t in all {
        let identity = if t.subject.is_empty() {
            format!("key:{}", t.key)
        } else {
            format!("sub:{}", t.subject)
        };
        if let Some(first) = by_identity.iter_mut().find(|(k, _)| *k == identity) {
            first.1.aliases.push(t.account_name.clone());
            continue;
        }
        by_identity.push((identity, t));
    }
    by_identity.into_iter().map(|(_, t)| t).collect()
}

/// models_of 是这里有某个套餐能服务的全部模型，用套餐自己的拼写。
pub fn models_of(targets: &[Target]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for t in targets {
        for m in &t.models {
            if !seen.contains(m) {
                seen.push(m.clone());
            }
        }
    }
    seen
}

/// served_models 是网关对外报的模型名：套餐声明过的优先；一个都没声明时（比如只剩
/// start-plan，它的 models 是空的、任何名字都接）退到 ZCode 认识的那些，免得空着。
pub fn served_models(paths: &Paths, targets: &[Target]) -> Vec<String> {
    let declared = models_of(targets);
    if declared.is_empty() {
        read_models(paths)
    } else {
        declared
    }
}

const FALLBACK_MODELS: &[&str] = &["glm-4.6", "glm-4.5", "glm-4.5-air"];

/// read_models 是 ZCode 自己的 provider_config.json 知道的模型，作为没有套餐时的兜底。
pub fn read_models(paths: &Paths) -> Vec<String> {
    let cfg: Option<Value> = std::fs::read_to_string(paths.zcode_dir().join("v2").join("provider_config.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let mut ids: Vec<String> = Vec::new();
    let mut add = |v: Option<&Value>| {
        if let Some(s) = v.and_then(|x| x.as_str()) {
            let s = s.trim();
            if !s.is_empty() && !ids.contains(&s.to_string()) {
                ids.push(s.to_string());
            }
        }
    };
    if let Some(rules) = cfg.as_ref().and_then(|c| c.get("config")).and_then(|c| c.get("modelConfigRules")) {
        for key in ["providerModelRules", "manualProviderModelRules"] {
            if let Some(arr) = rules.get(key).and_then(|a| a.as_array()) {
                for r in arr {
                    add(r.get("modelId"));
                }
            }
        }
    }
    if ids.is_empty() {
        ids = FALLBACK_MODELS.iter().map(|s| s.to_string()).collect();
    }
    ids
}
