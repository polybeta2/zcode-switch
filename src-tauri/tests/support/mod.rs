
//! 测试专用轻量 mock 上游：std TcpListener 手写最小 HTTP/1.1，零外部依赖。
//! 按路径前缀路由，响应按请求序消费（最后一条无限重复），实现故障序列注入。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// 一条被 mock 收到的请求：用于「请求规格」断言（header 是否成套、body 是否只含 plan_id、
/// 有没有把凭证塞进 body 等）。仅测试使用。
#[derive(Clone, Debug)]
pub struct Recorded {
    pub path: String,
    /// 键统一小写，便于大小写无关断言
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

pub struct Route {
    pub path: &'static str,
    /// (status, extra headers, body)，按请求序消费，最后一条无限重复
    pub responses: Vec<(u16, Vec<(&'static str, &'static str)>, String)>,
    /// 只写这么多字节就关连接，但 Content-Length 报的是完整长度——模拟「200 之后立刻
    /// 断流」：start plan 被挤的时候就是这个样子。
    pub cut: Option<usize>,
}

impl Route {
    pub fn json(path: &'static str, responses: Vec<(u16, &str)>) -> Route {
        Route {
            path,
            responses: responses
                .into_iter()
                .map(|(s, b)| (s, Vec::new(), b.to_string()))
                .collect(),
            cut: None,
        }
    }
    pub fn with_headers(
        path: &'static str,
        responses: Vec<(u16, Vec<(&'static str, &'static str)>, &str)>,
    ) -> Route {
        Route {
            path,
            responses: responses
                .into_iter()
                .map(|(s, h, b)| (s, h, b.to_string()))
                .collect(),
            cut: None,
        }
    }
    /// sse 是一次完整的流式回答；sse_cut 是开了口就断（只写 keep 个字节，长度照报）。
    pub fn sse(path: &'static str, body: &str) -> Route {
        Route {
            path,
            responses: vec![(200, vec![("Content-Type", "text/event-stream")], body.to_string())],
            cut: None,
        }
    }
    pub fn sse_cut(path: &'static str, body: &str, keep: usize) -> Route {
        Route {
            path,
            responses: vec![(200, vec![("Content-Type", "text/event-stream")], body.to_string())],
            cut: Some(keep),
        }
    }
}

pub struct MockUpstream {
    pub base: String,
    counters: Arc<Vec<AtomicUsize>>,
    recorder: Arc<Mutex<Vec<Recorded>>>,
    _shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockUpstream {
    pub fn start(routes: Vec<Route>) -> MockUpstream {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
        let port = listener.local_addr().unwrap().port();
        // 非阻塞 accept + 轮询 shutdown：否则 Drop 的 join 会永远等不到阻塞中的 accept
        listener.set_nonblocking(true).ok();
        let counters: Arc<Vec<AtomicUsize>> = Arc::new(routes.iter().map(|_| AtomicUsize::new(0)).collect());
        let routes = Arc::new(routes);
        let serve_routes = routes.clone();
        let serve_counters = counters.clone();
        let recorder: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
        let serve_recorder = recorder.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let handle = std::thread::spawn(move || {
            loop {
                if sd.load(Ordering::Relaxed) {
                    return;
                }
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(15));
                        continue;
                    }
                    Err(_) => return,
                };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let _ = stream.set_nonblocking(false);
                let req = read_request(&mut stream);
                let Some((path, headers, body)) = req else { continue };
                if let Ok(mut v) = serve_recorder.lock() {
                    v.push(Recorded { path: path.clone(), headers, body });
                }
                // 找第一条前缀匹配路由
                let idx = serve_routes.iter().position(|r| path.starts_with(r.path));
                let resp = match idx {
                    Some(i) => {
                        let route = &serve_routes[i];
                        let n = route.responses.len();
                        let c = serve_counters[i].fetch_add(1, Ordering::SeqCst);
                        let k = c.min(n - 1);
                        let (status, headers, body) = &route.responses[k];
                        match route.cut {
                            Some(keep) => format_response_cut(*status, headers, body, keep),
                            None => format_response(*status, headers, body),
                        }
                    }
                    None => format_response(404, &Vec::new(), r#"{"code":404,"msg":"mock no route"}"#),
                };
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        MockUpstream {
            base: format!("http://127.0.0.1:{port}"),
            counters,
            recorder,
            _shutdown: shutdown,
            handle: Some(handle),
        }
    }

    /// 某路由已被请求的次数（TTL 断言用）
    pub fn hits(&self, route_idx: usize) -> usize {
        self.counters[route_idx].load(Ordering::SeqCst)
    }

    /// 已收到的全部请求（用于断言请求头/请求体规格、以及「只读命令不产生写请求」）
    pub fn requests(&self) -> Vec<Recorded> {
        self.recorder.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        self._shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<(String, BTreeMap<String, String>, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    // 读到 header 结束
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4).unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let request_line = head.lines().next()?.to_string();
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for line in head.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    // POST：按 Content-Length 收齐 body（同时保留下来供测试断言）
    let content_length = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[header_end.min(buf.len())..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    body.truncate(content_length);
    Some((
        // 去掉 query
        path.split('?').next().unwrap_or(&path).to_string(),
        headers,
        String::from_utf8_lossy(&body).to_string(),
    ))
}

fn format_response(status: u16, headers: &[(&'static str, &'static str)], body: &str) -> String {
    // 路由自己给了 Content-Type（比如 text/event-stream）就照它的来，别再加一个默认的
    // ——两个同名头会让客户端只看第一个，流式用例会误判成 JSON。
    let named = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    let ctype = if named { "" } else { "Content-Type: application/json\r\n" };
    let mut out = format!("HTTP/1.1 {status} MOCK\r\n{ctype}Content-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(body);
    out
}

/// 同样的头，但正文只写 keep 个字节就关连接（Content-Length 仍是完整长度）。
/// 客户端读到一半连接就没了，reqwest 会报 "error decoding response body"——正是用户在
/// DSH 里看到的那句。
fn format_response_cut(status: u16, headers: &[(&'static str, &'static str)], body: &str, keep: usize) -> String {
    let mut out = format_response(status, headers, body);
    let keep = keep.min(body.len());
    let head_len = out.len() - body.len();
    out.truncate(head_len + keep);
    out
}

/// 环境变量测试互斥：env 是进程全局，env 相关用例统一串行。
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn set_env(k: &str, v: &str) {
    std::env::set_var(k, v);
}
pub fn del_env(k: &str) {
    std::env::remove_var(k);
}

/// 隔离的假 home（凭证/缓存/风控文件都写进去，测试结束清理）
pub struct TempHome(std::path::PathBuf);
impl TempHome {
    pub fn new(tag: &str) -> TempHome {
        use std::sync::atomic::AtomicU32;
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("zsw-mock-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempHome(p)
    }
    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn mock_creds() -> serde_json::Value {
    // 明文 JWT（>20 字符即通过 claim_token 的直通分支），不含真实凭证
    serde_json::json!({ "zcodejwttoken": "mock.jwt.token-for-tests-only-000000" })
}
