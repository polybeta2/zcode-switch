// prompt.rs — ZCode 自己的提示词原文，从 prompt.json 读入。
//
// prompt.json 由 gateway/prompt.mjs 生成（scripts/gen-prompt-json.mjs 可重放），
// 不是手抄：Start Plan 的风控会拿请求体跟官方客户端逐字比对，改一个标点就是一次
// 被拒。Node 网关与这里共用同一份文本，所以两边 dress 出来的请求一致。
use serde::Deserialize;
use std::sync::OnceLock;

const RAW: &str = include_str!("prompt.json");

// 这些字段是 ZCode 提示词契约的一部分：有的这一版用不上（工作目录与 git 状态网关
// 不知道），但结构必须与 prompt.json 对齐，缺一个就解析失败。
#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub heading: String,
    pub invoked_line: String,
    pub cwd_label: String,
    pub git_label: String,
    pub git_yes: String,
    pub git_no: String,
    pub platform_label: String,
    pub shell_label: String,
    pub os_version_label: String,
    pub untrusted_note: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    pub current_date_line: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompt {
    pub cli_prefix: String,
    pub stable_sections: Vec<String>,
    pub dynamic_behavior: String,
    pub session_guidance: String,
    pub context_management: String,
    pub powered_by: String,
    pub environment: Environment,
    pub context: Context,
}

pub fn prompt() -> &'static Prompt {
    static P: OnceLock<Prompt> = OnceLock::new();
    P.get_or_init(|| serde_json::from_str(RAW).expect("gateway/prompt.json 必须是 gateway/prompt.mjs 的原文"))
}
