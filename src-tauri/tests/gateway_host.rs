//! 监听地址：默认只听回环（本机客户端写死的就是它）；写 0.0.0.0 才让 WSL / 局域网
//! 连进来。WSL2 是独立子网，从里面连云主机的 127.0.0.1 是连不到的。

mod support;

use serde_json::Value;
use support::TempHome;
use zcode_switch_lib::gateway;

fn stored_host(home: &std::path::Path) -> Value {
    let p = home.join(".zcode-switch").join("gateway.json");
    let s = std::fs::read_to_string(p).expect("gateway.json written");
    serde_json::from_str::<Value>(&s).expect("valid json")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_zero_point_zero_listens_beyond_loopback_and_is_remembered() {
    let home = TempHome::new("gw-host-any");
    let paths = gateway::paths_for_tests(home.path().to_path_buf());

    let st = gateway::start(paths.clone(), Some(0), Some("0.0.0.0".into()))
        .await
        .expect("starts on every interface");
    assert_eq!(st["host"], "0.0.0.0");
    let port = st["port"].as_u64().expect("port");
    // 回环照样通：DSH 那边写死的是 127.0.0.1
    let body = reqwest::get(format!("http://127.0.0.1:{port}/healthz"))
        .await
        .expect("answers")
        .text()
        .await
        .unwrap_or_default();
    assert!(body.contains("\"ok\":true"), "unexpected body: {body}");
    gateway::stop(&paths);

    // 写进 gateway.json：下次界面里一开（不带 --host）也照这个跑
    assert_eq!(stored_host(home.path())["host"], "0.0.0.0");
    assert_eq!(gateway::status_of(&paths)["host"], "0.0.0.0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_that_is_not_an_ip_is_refused_instead_of_opened_up() {
    let home = TempHome::new("gw-host-bad");
    let paths = gateway::paths_for_tests(home.path().to_path_buf());
    let e = gateway::start(paths.clone(), Some(0), Some("0.0.0.0:8794".into()))
        .await
        .expect_err("refused");
    assert!(e.contains("不是合法的 IP"), "unexpected error: {e}");
    let st = gateway::status_of(&paths);
    assert_eq!(st["running"], false);
    // 打错的地址不许落盘，更不许被当成「对外开放」
    assert_eq!(st["host"], gateway::DEFAULT_HOST);
}
