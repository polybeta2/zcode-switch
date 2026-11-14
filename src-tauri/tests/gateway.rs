// gateway 的纯逻辑：翻译、穿衣服、座位表。不碰网络，所以跑得快，也不费额度。
use serde_json::{json, Value};
use std::collections::HashSet;
use zcode_switch_lib::gateway;

#[test]
fn an_openai_turn_becomes_one_anthropic_turn() {
    let body = json!({
        "model": "glm-5.3-flash",
        "max_tokens": 64,
        "messages": [
            { "role": "system", "content": "be brief" },
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": null, "tool_calls": [
                { "id": "call_1", "type": "function", "function": { "name": "read", "arguments": "{\"path\":\"a.txt\"}" } }
            ]},
            { "role": "tool", "tool_call_id": "call_1", "content": "file body" }
        ],
        "tools": [{ "type": "function", "function": { "name": "read", "parameters": { "type": "object" } } }]
    });
    let (req, warn) = gateway::translate::to_anthropic(&body, 4096).expect("translates");
    assert!(warn.is_empty(), "unexpected warnings: {warn:?}");
    assert_eq!(req["system"], "be brief");
    assert_eq!(req["max_tokens"], 64);
    let msgs = req["messages"].as_array().unwrap();
    // user, assistant(tool_use), user(tool_result) —— 三块，系统的那个已经被抬到顶层
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
    assert_eq!(msgs[1]["content"][0]["input"]["path"], "a.txt");
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call_1");
    assert_eq!(req["tools"][0]["input_schema"]["type"], "object");
}

#[test]
fn a_tool_result_and_the_words_after_it_are_one_turn() {
    let body = json!({
        "model": "m",
        "messages": [
            { "role": "user", "content": "read a.txt" },
            { "role": "assistant", "content": null, "tool_calls": [
                { "id": "c1", "type": "function", "function": { "name": "read", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "c1", "content": "12 bytes" },
            { "role": "user", "content": "now summarise it" }
        ]
    });
    let (req, _) = gateway::translate::to_anthropic(&body, 1024).unwrap();
    let msgs = req["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    let last = msgs[2]["content"].as_array().unwrap();
    assert_eq!(last.len(), 2);
    assert_eq!(last[0]["type"], "tool_result");
    assert_eq!(last[1]["type"], "text");
}

#[test]
fn a_choice_count_above_one_is_refused_rather_than_silently_dropped() {
    let body = json!({ "model": "m", "n": 3, "messages": [{ "role": "user", "content": "hi" }] });
    let err = gateway::translate::to_anthropic(&body, 1024).unwrap_err();
    assert!(err.contains("n > 1"), "{err}");
}

#[test]
fn a_vendor_answer_reads_like_an_openai_answer() {
    let message = json!({
        "id": "msg_1",
        "type": "message",
        "model": "GLM-5.3-Flash",
        "stop_reason": "max_tokens",
        "content": [
            { "type": "thinking", "thinking": "hmm" },
            { "type": "text", "text": "ok" },
            { "type": "tool_use", "id": "toolu_1", "name": "read", "input": { "path": "b" } }
        ],
        "usage": { "input_tokens": 10, "output_tokens": 4, "cache_read_input_tokens": 90 }
    });
    let out = gateway::translate::to_openai(&message, "glm-5.3-flash");
    assert_eq!(out["model"], "glm-5.3-flash");
    assert_eq!(out["choices"][0]["message"]["content"], "ok");
    assert_eq!(out["choices"][0]["message"]["reasoning_content"], "hmm");
    assert_eq!(out["choices"][0]["finish_reason"], "length");
    let call = &out["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["function"]["name"], "read");
    assert_eq!(call["function"]["arguments"], "{\"path\":\"b\"}");
    // 缓存读的那部分也算进 prompt_tokens：客户端按这个数算成本
    assert_eq!(out["usage"]["prompt_tokens"], 100);
    assert_eq!(out["usage"]["completion_tokens"], 4);
}

#[test]
fn a_vendor_stream_becomes_openai_chunks() {
    let mut tr = gateway::translate::StreamTranslator::new("glm-5.3-flash", None, 1_700_000_000);
    let first = tr.feed("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_9\",\"usage\":{\"input_tokens\":7}}}\n\n");
    assert!(first.contains("\"role\":\"assistant\""), "{first}");
    assert!(first.contains("msg_9"));
    let text = tr.feed(
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"he\"}}\n\n",
    );
    assert!(text.contains("\"content\":\"he\""), "{text}");
    // 半条事件不该产出任何东西：分片是按字节来的
    assert_eq!(tr.feed("event: content_block_delta\ndata: {\"type\":\"content_bl"), "");
    let rest = tr.feed("ock_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"y\"}}\n\n");
    assert!(rest.contains("\"content\":\"y\""), "{rest}");
    tr.feed(
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
    );
    let tail = tr.finish();
    assert!(tail.contains("\"finish_reason\":\"stop\""), "{tail}");
    assert!(tail.contains("[DONE]"), "{tail}");
    // 用量落在最后一个 chunk 上：开始时的输入和结束时的输出合并成一个数
    assert!(tail.contains("\"prompt_tokens\":7"), "{tail}");
    assert!(tail.contains("\"completion_tokens\":3"), "{tail}");
}

#[test]
fn the_dressed_request_looks_like_the_client_and_keeps_the_plan_model() {
    let text = json!({
        "model": "GLM-5.3-Flash",
        "max_tokens": 32,
        "messages": [{ "role": "user", "content": "hi" }]
    })
    .to_string();
    let out = gateway::dress::dress(&text, "zai", "device-1", "session-1");
    let v: Value = serde_json::from_str(&out).unwrap();
    let sys = v["system"].as_array().unwrap();
    // 厂商只认三个系统块，第四个会被 405/3012 拒
    assert_eq!(sys.len(), 3);
    assert!(sys[2]["text"].as_str().unwrap().starts_with("\n\n"));
    assert_eq!(v["model"], "GLM-5.3-Flash");
    let msgs = v["messages"].as_array().unwrap();
    // 第一轮前面挂的是日期提醒
    assert!(msgs[0]["content"][0]["text"].as_str().unwrap().starts_with("<system-reminder>"));
    assert_eq!(msgs[1]["content"][0]["type"], "text");
    assert_eq!(msgs[1]["content"][0]["cache_control"]["type"], "ephemeral");
    let uid = v["metadata"]["user_id"].as_str().unwrap();
    assert!(uid.contains("device-1"), "{uid}");
}

fn target(name: &str, key: &str) -> gateway::accounts::Target {
    gateway::accounts::Target {
        account_id: name.into(),
        account_name: name.into(),
        provider: "builtin:zai-start-plan".into(),
        plan: "start",
        site: "zai",
        dress: true,
        rank: 0,
        enabled: true,
        key: key.into(),
        base: "https://example.invalid/api".into(),
        models: vec!["GLM-5.3-Flash".into()],
        subject: name.into(),
        device: "dev".into(),
        label: name.into(),
        aliases: vec![],
    }
}

#[test]
fn one_account_lends_one_seat_at_a_time() {
    use gateway::pool::{Opts, Pool, Ready};
    let pool = Pool::new(Opts { inflight: 1, start_gap_ms: 0, ..Default::default() });
    let targets = vec![target("a", "k1")];
    let none: HashSet<String> = HashSet::new();
    let lease = match pool.acquire(&targets, &none) {
        Ready::Lease(l) => l,
        other => panic!("expected a seat, got {}", match other {
            Ready::Wait(_) => "wait",
            Ready::Rest(_) => "rest",
            Ready::Lease(_) => "lease",
        }),
    };
    // 座位已经占了：第二个请求要么等要么停，不能同时发
    match pool.acquire(&targets, &none) {
        Ready::Wait(_) => {}
        Ready::Rest(r) => assert!(!r.is_empty()),
        Ready::Lease(_) => panic!("two seats for one account"),
    }
    drop(lease);
    assert!(matches!(pool.acquire(&targets, &none), Ready::Lease(_)));
}

#[test]
fn a_busy_refusal_rests_the_account_for_a_moment() {
    use gateway::pool::{Opts, Pool, Ready};
    let pool = Pool::new(Opts {
        inflight: 1,
        start_gap_ms: 0,
        busy_cooldown_ms: 5_000,
        ..Default::default()
    });
    let targets = vec![target("a", "k1")];
    let none: HashSet<String> = HashSet::new();
    {
        let _seat = match pool.acquire(&targets, &none) {
            Ready::Lease(l) => l,
            _ => panic!("expected a seat"),
        };
    }
    pool.note("k1", "busy");
    match pool.acquire(&targets, &none) {
        Ready::Wait(d) => assert!(d.as_millis() > 0),
        _ => panic!("a busy account was lent again immediately"),
    }
}

#[test]
fn an_edge_refusal_rests_the_whole_address() {
    use gateway::pool::{Opts, Pool, Ready};
    let pool = Pool::new(Opts { inflight: 1, start_gap_ms: 0, ..Default::default() });
    let targets = vec![target("a", "k1"), target("b", "k2")];
    let none: HashSet<String> = HashSet::new();
    pool.note("k1", "edge");
    // 405 是 IP 的：另一个账号也不该发
    match pool.acquire(&targets, &none) {
        Ready::Rest(why) => assert!(why.contains("edge"), "{why}"),
        _ => panic!("the address kept sending after an edge refusal"),
    }
}

#[test]
fn a_refusal_gives_the_minute_budget_back() {
    use gateway::pool::{Opts, Pool, Ready};
    let pool = Pool::new(Opts {
        inflight: 1,
        start_gap_ms: 0,
        busy_cooldown_ms: 0,
        rpm: 1,
        ..Default::default()
    });
    let targets = vec![target("a", "k1")];
    let none: HashSet<String> = HashSet::new();
    {
        let seat = match pool.acquire(&targets, &none) {
            Ready::Lease(l) => l,
            _ => panic!("expected a seat"),
        };
        drop(seat);
    }
    // 一分钟只准起一次。被 3008 拒掉的那次没有跑模型，预算要退回来，
    // 否则后面的请求只能空等到 120 秒超时。
    pool.note("k1", "busy");
    match pool.acquire(&targets, &none) {
        Ready::Lease(_) => {}
        _ => panic!("a refused attempt kept the whole minute's budget"),
    }
}

#[test]
fn refused_attempts_still_count_as_arrivals() {
    use gateway::pool::{Opts, Pool, Ready};
    let pool = Pool::new(Opts {
        inflight: 1,
        start_gap_ms: 0,
        busy_cooldown_ms: 0,
        rpm: 1,
        ..Default::default()
    });
    let targets = vec![target("a", "k1")];
    let none: HashSet<String> = HashSet::new();
    // rpm=1 时到达上限是 3：连拒三次之后到达账满了，第四次不能再发。
    // 被拒的请求不跑模型，但仍然到达厂商的 edge——405 是按到达速率封的。
    for _ in 0..3 {
        {
            let seat = match pool.acquire(&targets, &none) {
                Ready::Lease(l) => l,
                _ => panic!("expected a seat"),
            };
            drop(seat);
        }
        pool.note("k1", "busy");
    }
    match pool.acquire(&targets, &none) {
        Ready::Wait(_) => {}
        _ => panic!("refused attempts were treated as free arrivals"),
    }
}

#[test]
fn the_pacing_knobs_can_be_changed_while_it_runs() {
    use gateway::pool::{Opts, Pool, Ready};
    let pool = Pool::new(Opts { inflight: 1, start_gap_ms: 0, rpm: 1, ..Default::default() });
    let targets = vec![target("a", "k1")];
    let none: HashSet<String> = HashSet::new();
    {
        let seat = match pool.acquire(&targets, &none) {
            Ready::Lease(l) => l,
            _ => panic!("expected a seat"),
        };
        drop(seat);
    }
    pool.note("k1", "ok");
    match pool.acquire(&targets, &none) {
        Ready::Wait(_) => {}
        _ => panic!("the minute's budget was not enforced"),
    }
    // 不用重启网关：调大预算之后下一问就该有座
    pool.retune(None, Some(2));
    assert!(matches!(pool.acquire(&targets, &none), Ready::Lease(_)));
    assert_eq!(pool.limits(), (1, 2));
}

#[test]
fn a_plan_is_addressed_by_its_own_spelling() {
    let t = target("a", "k1");
    assert_eq!(gateway::accounts::models_of(&[t.clone()]), vec!["GLM-5.3-Flash".to_string()]);
    // 客户端写小写，套餐写大写：找到的是套餐的拼写
    let want = "glm-5.3-flash";
    assert!(t.models.iter().any(|m| m.eq_ignore_ascii_case(want)));
}

#[test]
fn a_thinking_level_becomes_the_one_effort_field_the_plans_read() {
    let turn = |effort: &str| {
        let body = json!({
            "model": "GLM-5.3-Flash",
            "max_tokens": 4096,
            "reasoning_effort": effort,
            "messages": [{ "role": "user", "content": "hi" }]
        });
        gateway::translate::to_anthropic(&body, 1024).expect("translates")
    };
    let (req, warn) = turn("max");
    assert!(warn.is_empty(), "{warn:?}");
    assert_eq!(req["output_config"]["effort"], "max");
    // 上游只有 low / high / max 三档：medium 落到 high，xhigh 落到 max
    assert_eq!(turn("medium").0["output_config"]["effort"], "high");
    assert_eq!(turn("low").0["output_config"]["effort"], "low");
    assert_eq!(turn("xhigh").0["output_config"]["effort"], "max");
}

#[test]
fn a_thinking_level_the_plans_cannot_honour_is_left_to_the_plan() {
    let body = json!({
        "model": "GLM-5.3-Flash",
        "reasoning_effort": "off",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let (req, warn) = gateway::translate::to_anthropic(&body, 1024).expect("translates");
    assert!(req.get("output_config").is_none(), "sent an effort after all: {req}");
    assert!(warn.iter().any(|w| w.contains("off")), "{warn:?}");

    // Anthropic 形状的客户端把档位塞在 thinking.effort 里，大小写不论
    let body = json!({
        "model": "GLM-5.3-Flash",
        "thinking": { "type": "enabled", "effort": "LOW" },
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let (req, _) = gateway::translate::to_anthropic(&body, 1024).expect("translates");
    assert_eq!(req["output_config"]["effort"], "low");
}

/// 网关靠 saw_content 判断「这个流已经接上了」：只有角色那一帧不算——上游 200 之后
/// 只报了个开场就断掉，换账号完全来得及。
#[test]
fn a_role_frame_alone_is_not_content() {
    let mut tr = gateway::translate::StreamTranslator::new("GLM-5.3-Flash", None, 1_700_000_000);
    tr.feed("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":3}}}\n\n");
    assert!(!tr.saw_content(), "the opening frame counted as content");
    tr.feed("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n");
    assert!(!tr.saw_content(), "an empty block counted as content");
    tr.feed("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n");
    assert!(tr.saw_content(), "a text delta did not count as content");
}

/// 断流是「软休息」：只有这一个账号时 acquire 必须等它，而不是直接回 Rest——否则单账号
/// 的机器一遇到 start plan 被挤就整轮失败，静默重试等于没有。
#[test]
fn a_broken_stream_rests_the_account_softly_enough_to_retry_it() {
    use gateway::pool::{Opts, Pool, Ready};
    let pool = Pool::new(Opts { inflight: 1, start_gap_ms: 0, broken_ms: 30, ..Default::default() });
    let targets = vec![target("a", "k1")];
    let none: HashSet<String> = HashSet::new();
    match pool.acquire(&targets, &none) {
        Ready::Lease(l) => drop(l),
        _ => panic!("the first seat was not handed out"),
    }
    pool.note("k1", "broken");
    match pool.acquire(&targets, &none) {
        Ready::Wait(_) => {}
        Ready::Rest(r) => panic!("a broken stream is a soft rest, got Rest({r})"),
        Ready::Lease(_) => panic!("the account was retried with no pause at all"),
    }
    std::thread::sleep(std::time::Duration::from_millis(60));
    assert!(matches!(pool.acquire(&targets, &none), Ready::Lease(_)), "the account never came back");
}
