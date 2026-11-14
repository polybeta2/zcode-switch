// upstream.rs — 一个请求怎么发出去，以及怎么把回话读回来。
//
// 全项目只有这里碰厂商的 socket。之所以单独一个文件，是因为这块是唯一需要
// 流式的地方：套餐的答案是 SSE，客户端要边收边看，所以不能像 ureq 那样把整条
// 响应体收进内存再说。
//
// 已知的坑：厂商前面有一层 WAF 看 TLS 指纹（quota.rs 里那条注释、PR#43）。
// reqwest 走 native-tls（Windows 上是 Schannel），指纹跟浏览器/CLI 都不完全一样。
// 现在还活着，说明这层只卡 IP 频率、不卡 TLS；哪天开始 405 了，先怀疑这里。

use std::time::Duration;

use serde_json::Value;

/// 一条非流式回话：已经按容量截断的响应体。
pub struct Reply {
    pub text: String,
}

fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // 这里刻意不设总超时：reqwest 的 timeout 连读响应体一起算，流式回答一旦说满
            // 五分钟就会被我们自己掐断——客户端那边看到的就是 "error decoding response body"
            // （DSH 里这么报过）。read_timeout 只在「连续这么多秒一个字节都没有」时断，
            // 正常的长回答不受影响；真要卡死，也还有它兜着。
            .read_timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(20))
            .user_agent("zcode-switch-gateway/0.1")
            .build()
            .expect("reqwest client")
    })
}

/// kind_of 把 reqwest 的错误折成调用方能分类的两种：超时和断网。两者在排队逻辑
/// 里待遇一样（换账号，不罚账号），但在界面上不一样。
pub fn kind_of(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else {
        "net"
    }
}

/// post 发一条 JSON 请求，不读响应体。要不要流、要不要截断由调用方决定。
pub async fn post(url: &str, headers: &[(String, String)], body: &Value) -> Result<reqwest::Response, reqwest::Error> {
    let mut req = client().post(url);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req.json(body).send().await
}

/// read_text 读完整条响应体，最多 cap 字节。超过就截断——这不是正常的路径，
/// 正常的错误响应都很短，截断只是防止一个坏上游把内存吃光。
pub async fn read_text(resp: reqwest::Response, cap: usize) -> Result<Reply, reqwest::Error> {
    let bytes = resp.bytes().await?;
    let take = bytes.len().min(cap);
    let text = String::from_utf8_lossy(&bytes[..take]).to_string();
    Ok(Reply { text })
}

/// 厂商的错误体是 {"code":3008,"msg":"..."} 或者网关的 {"error":{...}}。
/// 两种都认，认不出就返回原文前 300 个字符，别把一整页 HTML 塞进答案。
pub fn message_of(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(m) = v.get("msg").and_then(|m| m.as_str()) {
            if !m.is_empty() {
                return m.to_string();
            }
        }
        if let Some(m) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            if !m.is_empty() {
                return m.to_string();
            }
        }
        if let Some(m) = v.get("message").and_then(|m| m.as_str()) {
            if !m.is_empty() {
                return m.to_string();
            }
        }
    }
    let flat = text.trim().replace('\n', " ");
    flat.chars().take(300).collect()
}

/// code_of 取厂商业务码。HTTP 200 里带一个业务码是常态（1005 没套餐、3008 并发）。
pub fn code_of(text: &str) -> Option<u32> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_u64()))
        .map(|c| c as u32)
}
