//! 流式行为的端到端测试：上游 200 之后立刻断流时，网关必须在客户端一个字节都还没收到
//! 之前换账号重来（start plan 被挤是常态）。全部流量指向 127.0.0.1，不碰真账号。

mod support;

use serde_json::json;
use support::{MockUpstream, Route, TempHome};
use zcode_switch_lib::gateway::{pool, server};

/// 一段完整的 Anthropic 流：只有一句正文，用来证明「接上了」。
fn good_stream(text: &str) -> String {
    format!(
        concat!(
            "event: message_start\n",
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"GLM-5.3-Flash\",\"content\":[],\"usage\":{{\"input_tokens\":5,\"output_tokens\":0}}}}}}\n\n",
            "event: content_block_start\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
            "event: content_block_delta\n",
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n",
            "event: content_block_stop\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "event: message_delta\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":2}}}}\n\n",
            "event: message_stop\n",
            "data: {{\"type\":\"message_stop\"}}\n\n"
        ),
        text = text
    )
}

/// 只报了「开始了」，正文一个字都没发。
fn empty_stream() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_0\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"GLM-5.3-Flash\",\"content\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    )
    .to_string()
}

fn save_account(home: &std::path::Path, id: &str, name: &str, provider: &str, base: &str, key: &str, created: &str) {
    let dir = home.join(".zcode-switch").join("accounts");
    std::fs::create_dir_all(&dir).unwrap();
    let acc = json!({
        "id": id,
        "name": name,
        "created_at": created,
        "updated_at": created,
        "hash": "h",
        "credentials": {},
        "config": { "provider": { provider: { "options": { "apiKey": key, "baseURL": base } } } },
        "virtual_device_mid": "device-0001"
    });
    std::fs::write(dir.join(format!("{id}.json")), acc.to_string()).unwrap();
}

async fn serve(home: std::path::PathBuf) -> String {
    let p = pool::Pool::new(pool::Opts::default());
    let ctx = server::Ctx::new(zcode_switch_lib::gateway::paths_for_tests(home), p, "test-key".into(), false);
    let app = server::router(ctx);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn ask(gateway: &str) -> (u16, String) {
    let body = json!({
        "model": "GLM-5.3-Flash",
        "stream": true,
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let resp = reqwest::Client::new()
        .post(format!("{gateway}/v1/chat/completions"))
        .bearer_auth("test-key")
        .json(&body)
        .send()
        .await
        .expect("gateway answers");
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap_or_default())
}

/// start plan 的账号开口就断：客户端的流式回答最终来自第二个账号，中间那句
/// "the vendor's stream broke" 不该出现在它看到的字节里。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_that_dies_before_any_text_is_retried_on_another_account() {
    let mock = MockUpstream::start(vec![
        Route::sse_cut("/a", &good_stream("never delivered"), 40),
        Route::sse("/b", &good_stream("second account")),
    ]);
    let home = TempHome::new("gw-retry");
    // start plan 排序在前，所以第一个被打到的永远是会断的那个账号
    save_account(home.path(), "acct-a", "A", "builtin:zai-start-plan", &format!("{}/a", mock.base), "sk-mock-key-00000000000000000000000000a", "2026-01-01T00:00:00Z");
    save_account(home.path(), "acct-b", "B", "builtin:zai-coding-plan", &format!("{}/b", mock.base), "sk-mock-key-00000000000000000000000000b", "2026-01-02T00:00:00Z");
    let gateway = serve(home.path().to_path_buf()).await;

    let (status, body) = ask(&gateway).await;
    assert_eq!(status, 200, "got {body}");
    assert!(body.contains("second account"), "the retry did not reach the second account: {body}");
    assert!(!body.contains("stream broke"), "a broken stream leaked to the client: {body}");
    assert_eq!(mock.hits(0), 1, "the broken account was not tried exactly once");
    assert_eq!(mock.hits(1), 1, "the second account was not used");
}

/// 上游干净收尾但正文一个字都没有：这也算没接上，同样换账号。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_that_ends_without_any_content_is_retried_too() {
    let mock = MockUpstream::start(vec![
        Route::sse("/c", &empty_stream()),
        Route::sse("/d", &good_stream("second account")),
    ]);
    let home = TempHome::new("gw-empty");
    save_account(home.path(), "acct-c", "C", "builtin:zai-start-plan", &format!("{}/c", mock.base), "sk-mock-key-00000000000000000000000000c", "2026-01-01T00:00:00Z");
    save_account(home.path(), "acct-d", "D", "builtin:zai-coding-plan", &format!("{}/d", mock.base), "sk-mock-key-00000000000000000000000000d", "2026-01-02T00:00:00Z");
    let gateway = serve(home.path().to_path_buf()).await;

    let (status, body) = ask(&gateway).await;
    assert_eq!(status, 200, "got {body}");
    assert!(body.contains("second account"), "the retry did not reach the second account: {body}");
    assert_eq!(mock.hits(0), 1);
    assert_eq!(mock.hits(1), 1);
}
