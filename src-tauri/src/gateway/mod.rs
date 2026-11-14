// gateway.rs — 把网关装进应用里：一个开关、一个端口、一个 token。
//
// 这里只管生命周期和那两行配置文件；真正干活的是 server.rs。之所以要一个 token：
// 这个端口对整台机器开放，任何本地进程都能连；一个留在磁盘上的随机串让「谁在用」
// 是明确的，而不是「谁先连上算谁的」。
//
// token 存在 ~/.zcode-switch/gateway.json（造一次就不再变）；端口固定 127.0.0.1:8794，
// 客户端把这条 base URL 写死才安全 —— 见下面 bind() 的注释，端口不能自己漂。
pub mod accounts;
pub mod dress;
pub mod pool;
pub mod prompt;
pub mod server;
pub mod translate;
pub mod upstream;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::gateway::pool::{Opts, Pool};
use crate::store::Paths;

pub const DEFAULT_PORT: u16 = 8794;

struct Running {
    port: u16,
    stop: Option<oneshot::Sender<()>>,
    started: Instant,
    /// 留着它，是为了能热改并发与每分钟预算：改了立刻生效，不用停一次网关。
    pool: Arc<Pool>,
}

fn slot() -> &'static Mutex<Option<Running>> {
    static SLOT: OnceLock<Mutex<Option<Running>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn config_file(paths: &Paths) -> PathBuf {
    paths.store_dir().join("gateway.json")
}

fn read_config(paths: &Paths) -> Value {
    std::fs::read_to_string(config_file(paths))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn write_config(paths: &Paths, v: &Value) {
    let _ = std::fs::create_dir_all(paths.store_dir());
    if let Ok(s) = serde_json::to_string_pretty(v) {
        let _ = std::fs::write(config_file(paths), s);
    }
}

fn mint_token() -> String {
    format!(
        "zsg_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// token 是这份安装自己的凭据。没有就造一个并写下来：造一次，别每次启动换个新的，
/// 否则客户端的配置每重启一次就失效。
pub fn token_for(paths: &Paths) -> String {
    let mut cfg = read_config(paths);
    let existing = cfg
        .get("token")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|t| t.len() >= 12)
        .map(String::from);
    match existing {
        Some(t) => t,
        None => {
            let t = mint_token();
            if let Some(o) = cfg.as_object_mut() {
                o.insert("token".into(), Value::String(t.clone()));
            }
            write_config(paths, &cfg);
            t
        }
    }
}

/// opts_from 读 gateway.json 里那两个能调的旋钮。其余节流参数是实测出来的常数：
/// 起跑间隔 800ms、繁忙冷却 1.2s、风控/edge 休息 3 分钟——这些改了就得重新测，
/// 所以不开放。能被调的只有「一个账号同时几个」和「一分钟起几个」。
fn opts_from(paths: &Paths) -> Opts {
    let cfg = read_config(paths);
    let mut opts = Opts::default();
    if let Some(n) = cfg.get("inflight").and_then(|v| v.as_u64()) {
        opts.inflight = (n as usize).clamp(1, 8);
    }
    if let Some(n) = cfg.get("rpm").and_then(|v| v.as_u64()) {
        opts.rpm = (n as usize).clamp(1, 120);
    }
    opts
}

/// 固定端口：客户端配置里写的是这个地址，所以它不能跟着「上回用了哪个」变。
/// 旧版本会把实际端口写进 gateway.json，端口一漂就把客户端甩在原地 —— 这里不再读它。
fn fixed_port() -> u16 {
    DEFAULT_PORT
}

fn bind(port: u16) -> Result<TcpListener, String> {
    // 端口就是客户端配置里写死的那个 base URL：占了就明说，不偷偷往后挪。
    // 曾经这里是「往后挪 10 个」，实测代价是：本地留了一个测试实例占着 8795，
    // 界面一开网关就静悄悄坐到 8796 并把 8796 写回 gateway.json，
    // 而 DSH 那边还连着 127.0.0.1:8794 —— 表现为「网关明明开着，却 connect-refused」。
    TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!(
                "端口 {port} 已被占用：先关掉占用它的程序；或用 --port 换一个端口，并把客户端的 base URL 一起改掉"
            )
        } else {
            format!("端口 {port} 无法绑定：{e}")
        }
    })
}

pub fn status_of(paths: &Paths) -> Value {
    let guard = slot().lock().unwrap_or_else(|e| e.into_inner());
    let base = match guard.as_ref() {
        Some(r) => format!("http://127.0.0.1:{}/v1", r.port),
        None => format!("http://127.0.0.1:{}/v1", fixed_port()),
    };
    let targets = accounts::load_targets(paths, true, None, None);
    // 跑着的时候报告的是真的在用的那一对；没跑的时候报告下一次会用的那一对
    let (inflight, rpm) = match guard.as_ref() {
        Some(r) => r.pool.limits(),
        None => {
            let o = opts_from(paths);
            (o.inflight, o.rpm)
        }
    };
    json!({
        "running": guard.is_some(),
        "limits": { "inflight": inflight, "rpm": rpm },
        "port": guard.as_ref().map(|r| r.port).unwrap_or_else(fixed_port),
        "base": base,
        "token": token_for(paths),
        "uptime_s": guard.as_ref().map(|r| r.started.elapsed().as_secs()).unwrap_or(0),
        "targets": targets.len(),
        "models": accounts::served_models(&targets),
    })
}

/// start 起一个本地网关：端口只认 `--port` 或默认 8794，占了就报错，不往后挪。
pub async fn start(paths: Paths, port: Option<u16>) -> Result<Value, String> {
    {
        let guard = slot().lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return Ok(status_of(&paths));
        }
    }
    let token = token_for(&paths);
    let want = port.unwrap_or_else(fixed_port);
    let listener = bind(want)?;
    let actual = listener.local_addr().map(|a| a.port()).unwrap_or(want);
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let listener = tokio::net::TcpListener::from_std(listener).map_err(|e| e.to_string())?;

    let pool = Pool::new(opts_from(&paths));
    let ctx = server::Ctx::new(paths.clone(), Arc::clone(&pool), token.clone(), true);
    ctx.targets();
    let app = server::router(ctx);
    let (tx, rx) = oneshot::channel::<()>();
    // 用 tauri 的执行器起这个任务：它是 tokio，而且是应用自己的那个运行时
    tauri::async_runtime::spawn(async move {
        let served = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
        if let Err(e) = served {
            eprintln!("gateway stopped: {e}");
        }
    });

    {
        let mut guard = slot().lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Running {
            port: actual,
            stop: Some(tx),
            started: Instant::now(),
            pool: Arc::clone(&pool),
        });
    }
    let mut cfg = read_config(&paths);
    if let Some(o) = cfg.as_object_mut() {
        // 端口不再落盘：写进去就等于给下一次启动留了个「漂到别处」的理由
        o.remove("port");
        o.insert("token".into(), Value::String(token));
    }
    write_config(&paths, &cfg);
    Ok(status_of(&paths))
}

pub fn stop(paths: &Paths) -> Value {
    {
        let mut guard = slot().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut r) = guard.take() {
            if let Some(tx) = r.stop.take() {
                let _ = tx.send(());
            }
        }
    }
    // 让 graceful shutdown 走完再报状态，界面上的「已停止」才是真的
    std::thread::sleep(Duration::from_millis(150));
    status_of(paths)
}

// ---------------------------------------------------------------- 给界面的口

#[tauri::command]
pub fn gateway_status() -> Value {
    status_of(&Paths::detect())
}

/// set_limits 把两个旋钮写进 gateway.json，并且立刻作用到正在跑的那个网关上：
/// 改并发不该要求先停一次（停一次就断掉正在答的请求）。
pub fn set_limits(paths: &Paths, inflight: Option<u32>, rpm: Option<u32>) -> Value {
    let want_inflight = inflight.map(|n| n.clamp(1, 8));
    let want_rpm = rpm.map(|n| n.clamp(1, 120));
    let mut cfg = read_config(paths);
    if let Some(o) = cfg.as_object_mut() {
        if let Some(n) = want_inflight {
            o.insert("inflight".into(), json!(n));
        }
        if let Some(n) = want_rpm {
            o.insert("rpm".into(), json!(n));
        }
    }
    write_config(paths, &cfg);
    // applied_live 说的是「正在跑的那个网关是不是本进程」：设置页改的永远是本进程，
    // 所以立刻生效；CLI 改的是别的进程起的网关，只能写进配置等它下次启动。
    let applied_live = {
        let guard = slot().lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(r) => {
                r.pool.retune(
                    want_inflight.map(|n| n as usize),
                    want_rpm.map(|n| n as usize),
                );
                true
            }
            None => false,
        }
    };
    let mut out = status_of(paths);
    if let Some(o) = out.as_object_mut() {
        o.insert("applied_live".into(), json!(applied_live));
    }
    out
}

#[tauri::command]
pub fn gateway_set_limits(inflight: Option<u32>, rpm: Option<u32>) -> Value {
    set_limits(&Paths::detect(), inflight, rpm)
}

#[tauri::command]
pub async fn gateway_start(port: Option<u16>) -> Result<Value, String> {
    start(Paths::detect(), port).await
}

#[tauri::command]
pub fn gateway_stop() -> Value {
    stop(&Paths::detect())
}
