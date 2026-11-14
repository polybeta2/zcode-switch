// pool.rs — 哪个账号可以现在发请求。
//
// 这件事的唯一实测事实是：一个套餐账号同时只接一个请求，快一点也就 1.2 秒。
// 所以这里的形状是「每账号一张座位表」，而不是一个计数器：座位空着才发，
// 发出去立刻占座，请求结束（Lease 掉 Drop）才放座。
//
// 第二个实测事实（2026-11-14，10 个 start plan 账号）：并发上限不在账号上，在**出口
// IP** 上——同一秒里 9 个不同账号一起拿到 429「user concurrency limit exceeded」，
// 而单独一条永远是 200。所以真正的并发闸门是整个进程的（global_inflight，默认 1），
// 账号再多也只是分摊额度，不是分摊并发。这条 429 不能当「这个账号忙」处理：那样 5 次
// 之后所有账号一起休 180 秒，并行两个任务就整机 503 三分钟。
//
// 第三件事：选号按「最久没用的先用」轮转。老代码取最近用过的优先（MRU），静默的串行
// 流量会一直黏在同一个账号上，10 个账号只有 1~2 个真的在耗额度。
//
// 另外三件事也在这里，因为它们的粒度只能是账号或整个进程：
// - 起跑间隔：同一账号两次起步至少隔 800ms。实测 1.2 秒交错时正好一个进一个退，
//   挨得太近就是 3008。
// - 冷却：刚被 3008 拒过的账号先别问，1.2 秒后再问。厂商那边放得比客户端想得快。
// - 封路：一个账号的 3012（风控）或 405（edge 拒整个 IP）是同一出口上的所有账号
//   共享的，所以这两个是进程级的暂停，不是这个账号的。
//
// 队列是有时间的：排队超过 queue_ms（默认 120 秒）就答 429，而不是一直挂着。
//
// 预算分两本账，因为「被拒」和「跑过模型」在厂商那边不是一回事：
// - 工作预算 rpm：**每个账号**一分钟里真正被收下的请求（整机的账按可用账号数放大）。
//   被 3008 拒掉的尝试从这里退回去——
//   它没有跑模型，占着预算只会让后面排队的请求空等到超时（实测 6 并发时 21 次
//   拒绝把一整分钟的预算烧光，三个请求全在 120 秒上超时）。
// - 到达上限 arrival_rpm：每一次发出去的请求都算，包括被拒的。它防的是 edge 按
//   到达速率封 IP（405），所以只能调高不能取消；默认取工作预算的三倍。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::gateway::accounts::Target;

#[derive(Clone, Debug)]
pub struct Opts {
    /// 一个账号同时能飞几个请求。实测是 1；插件的 maxConcurrency: 3 是它自己的客户端闸门。
    pub inflight: usize,
    pub start_gap_ms: u64,
    pub busy_cooldown_ms: u64,
    pub queue_ms: u64,
    /// 整个进程每分钟最多起多少个请求。默认 12：插件的日志里 ~18/分钟就开始出事。
    pub rpm: usize,
    pub narrow_ms: u64,
    pub double_refusals: usize,
    pub double_window_ms: u64,
    pub hold_ms: u64,
    pub edge_ms: u64,
    pub risk_ms: u64,
    pub auth_ms: u64,
    /// 上游开了口又立刻断流之后的冷却。断流多半是 start plan 被挤的，不是账号的错，
    /// 一会儿就好；但连着重连同一个账号通常还是断，所以让它歇一下，换别人先顶上。
    /// 这是「软休息」：所有人都这样时 acquire 会等它，而不是报「都在休息」。
    pub broken_ms: u64,
    pub busy_waits: Vec<u64>,
    /// 整个进程同时在飞的请求数。厂商对「同一个用户」限并发——实测压测时几个不同账号
    /// 同时发也会一起拿到 429 `user concurrency limit exceeded`，说明这个「用户」是按出口
    /// IP 算的，不是按账号。所以这里默认 1：真正的串行，排队发生在本地，而不是撞到
    /// 厂商的 429 上（撞一次的代价是接下来几十秒全是 429）。
    pub global_inflight: usize,
    /// 撞到上面的并发限制之后，整个进程停多久再发。厂商那边是滑动窗口，短促重试只会
    /// 让窗口一直不滑过去。
    pub congest_ms: u64,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            inflight: 1,
            start_gap_ms: 800,
            busy_cooldown_ms: 1200,
            queue_ms: 120_000,
            rpm: 12,
            narrow_ms: 300_000,
            double_refusals: 5,
            double_window_ms: 60_000,
            hold_ms: 180_000,
            edge_ms: 180_000,
            risk_ms: 300_000,
            auth_ms: 600_000,
            broken_ms: 1_500,
            busy_waits: vec![1_000, 2_000],
            global_inflight: 1,
            congest_ms: 800,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct Slot {
    limit: usize,
    inflight: usize,
    last_start: Option<Instant>,
    busy_until: Option<Instant>,
    /// 被挤窄之后的可回到时间；到期恢复 limit。
    narrow_until: Option<Instant>,
    /// 这个账号暂停到什么时候，以及为什么（spent / auth / risk / 一层层叠加）。
    rest_until: Option<Instant>,
    rest_kind: &'static str,
    sent: usize,
    refusals: usize,
    /// 已经发出去、还没有结局的起步时间。被拒时要凭它把这一笔从工作预算里退掉。
    pending: Vec<Instant>,
    /// 最近两次被 3008 拒的时间，用来判断「一直在被拒」而不是「偶尔被拒」。
    recent: Vec<Instant>,
}

struct Inner {
    slots: HashMap<String, Slot>,
    /// 整个进程现在有几个请求在飞。见 Opts::global_inflight。
    flying: usize,
    /// 撞到厂商的按 IP 并发限制之后，整个进程停到什么时候。
    congest_until: Option<Instant>,
    /// 进程级封路：405 或 3012 之后所有账号都不发。
    edge_until: Option<Instant>,
    edge_why: String,
    /// 工作预算：被收下的请求（被拒的会被退掉）。
    starts: Vec<Instant>,
    /// 到达预算：所有发出去的请求，只增不减。
    arrivals: Vec<Instant>,
    served: usize,
    refused: usize,
    since: Instant,
}

/// Live 是两个可以热改的数：并发和每分钟预算。其余节流参数是实测出来的常数，
/// 改了就要重新测，所以它们留在 Opts 里不动。
#[derive(Clone, Copy, Debug)]
struct Live {
    inflight: usize,
    rpm: usize,
}

impl Live {
    fn from(opts: &Opts) -> Self {
        Self { inflight: opts.inflight.max(1), rpm: opts.rpm.max(1) }
    }

    /// 到达上限：被拒的请求不跑模型，但仍然到达一次厂商的 edge。
    fn arrival_rpm(&self) -> usize {
        self.rpm.saturating_mul(3).max(self.rpm)
    }
}

pub struct Pool {
    pub opts: Opts,
    live: RwLock<Live>,
    inner: Mutex<Inner>,
    ready: Notify,
}

/// Ready 是 acquire 的三种回答：可以发、等一会儿、或者现在没得发。
pub enum Ready {
    Lease(Lease),
    Wait(Duration),
    /// 所有账号都在休息，或者是整个进程被封了。带着原因，好让调用方把它变成答案。
    Rest(String),
}

/// Lease 是占到的座位。它掉 drop 就放座，所以一个请求中途 return / ? 都不会把账号
/// 锁死——这是唯一一处需要 RAII 的地方。
pub struct Lease {
    pool: Arc<Pool>,
    key: String,
    released: bool,
}

impl Lease {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.pool.release(&self.key);
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.release();
    }
}

/// window_gate 说「这本账满了没有，满了还要等多久」。两本账用同一套算法。
fn window_gate(ledger: &[Instant], cap: usize, now: Instant, window: Duration) -> Duration {
    if ledger.len() < cap.max(1) {
        return Duration::ZERO;
    }
    ledger
        .first()
        .map(|t| window.saturating_sub(now.duration_since(*t)))
        .unwrap_or(Duration::from_secs(1))
}

impl Pool {
    pub fn new(opts: Opts) -> Arc<Self> {
        let mut opts = opts;
        // 压测用：整机并发上限可以直接用环境变量改，不用重新编译。
        if let Ok(v) = std::env::var("ZCODE_SWITCH_GW_CONCURRENCY") {
            if let Ok(n) = v.trim().parse::<usize>() {
                opts.global_inflight = n.max(1);
            }
        }
        let live = Live::from(&opts);
        Arc::new(Self {
            opts,
            live: RwLock::new(live),
            inner: Mutex::new(Inner {
                slots: HashMap::new(),
                flying: 0,
                congest_until: None,
                edge_until: None,
                edge_why: String::new(),
                starts: Vec::new(),
                arrivals: Vec::new(),
                served: 0,
                refused: 0,
                since: Instant::now(),
            }),
            ready: Notify::new(),
        })
    }

    /// global 是整机同时在飞的请求上限。
    fn global(&self) -> usize {
        self.opts.global_inflight.max(1)
    }

    /// touch 给每个目标建座位表，让 /healthz 一上来就能报全量。
    pub fn touch(&self, targets: &[Target]) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let limit = self.live.read().unwrap_or_else(|e| e.into_inner()).inflight;
        for t in targets {
            inner.slots.entry(t.key.clone()).or_insert_with(|| Slot {
                limit,
                ..Default::default()
            });
        }
    }

    /// acquire 找一个现在就能发的账号。priority 收到的目标已经是排好序的。
    pub fn acquire(self: &Arc<Self>, targets: &[Target], exclude: &HashSet<String>) -> Ready {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if let Some(until) = inner.edge_until {
            if until > now {
                return Ready::Rest(inner.edge_why.clone());
            }
            inner.edge_until = None;
            inner.edge_why.clear();
        }
        // 刚被厂商按出口 IP 的并发限制拒过：整机停一小会儿。这时候给某个账号记过是
        // 没意义的——问题不在账号上。
        if let Some(until) = inner.congest_until {
            if until > now {
                return Ready::Wait(until.duration_since(now));
            }
            inner.congest_until = None;
        }
        // 整个进程同时在飞的请求数。厂商的「user concurrency limit」是按出口 IP 算的，
        // 不是按账号：多开账号不会让并发变大，只会撞 429。所以真正的并发闸门在这里，
        // 超出的请求在本地排队。ZCODE_SWITCH_GW_CONCURRENCY 可以改这个数。
        if inner.flying >= self.global() {
            return Ready::Wait(Duration::from_millis(50));
        }
        // 每分钟的总量：超了就让等，而不是硬发。工作账和到达账各一道闸。
        // rpm 是「每个账号」的预算，整机的预算按能用的账号数放大——否则 10 个账号也
        // 只有 12 次/分钟，等于只养活了半个账号。
        let live = *self.live.read().unwrap_or_else(|e| e.into_inner());
        let seats = targets.len().max(1);
        let window = Duration::from_secs(60);
        inner.starts.retain(|t| now.duration_since(*t) < window);
        inner.arrivals.retain(|t| now.duration_since(*t) < window);
        let gate = window_gate(&inner.starts, live.rpm.saturating_mul(seats), now, window)
            .max(window_gate(&inner.arrivals, live.arrival_rpm().saturating_mul(seats), now, window));

        let limit = live.inflight;
        let mut wake = Duration::from_secs(3600);
        let mut rested = 0usize;
        let mut reason = String::new();
        let mut best: Option<String> = None;
        // 第三位是「距上次起步多久」，包成 Reverse 让最久没用的排最前：轮着用，
        // 而不是一直喂同一个账号。老代码取最小，等于最近用过的优先（MRU），所以
        // 10 个账号只有 1-2 个真的在耗额度。
        let mut best_score: (u8, usize, std::cmp::Reverse<Duration>) =
            (u8::MAX, usize::MAX, std::cmp::Reverse(Duration::MAX));
        for t in targets {
            if exclude.contains(&t.key) {
                continue;
            }
            let slot = inner.slots.entry(t.key.clone()).or_insert_with(|| Slot {
                limit,
                ..Default::default()
            });
            // 没有结局的起步（客户端断了连接）不能一直挂在待定队列上
            slot.pending.retain(|t| now.duration_since(*t) < window);
            // 一个被挤窄的座位到期恢复
            if let Some(until) = slot.narrow_until {
                if until <= now {
                    slot.narrow_until = None;
                    slot.limit = limit;
                }
            }
            if let Some(until) = slot.rest_until {
                if until > now {
                    // broken 是软休息：断流几秒就过去了，把它算进「全体都歇着」会让只有
                    // 一个账号的机器直接拿到 Rest 而报错。这里只记进 wake，等它到期。
                    if slot.rest_kind == "broken" {
                        wake = wake.min(until.duration_since(now));
                        continue;
                    }
                    rested += 1;
                    if reason.is_empty() {
                        reason = slot.rest_kind.to_string();
                    }
                    wake = wake.min(until.duration_since(now));
                    continue;
                }
                slot.rest_until = None;
                slot.rest_kind = "";
            }
            if let Some(until) = slot.busy_until {
                if until > now {
                    wake = wake.min(until.duration_since(now));
                    continue;
                }
                slot.busy_until = None;
            }
            if let Some(last) = slot.last_start {
                let gap = Duration::from_millis(self.opts.start_gap_ms);
                let passed = now.duration_since(last);
                if passed < gap {
                    wake = wake.min(gap.saturating_sub(passed));
                    continue;
                }
            }
            if gate > Duration::ZERO {
                wake = wake.min(gate);
                continue;
            }
            if slot.inflight >= slot.limit.max(1) {
                wake = wake.min(Duration::from_millis(120));
                continue;
            }
            let age = slot.last_start.map(|l| now.duration_since(l)).unwrap_or(Duration::MAX);
            let score = (t.rank, slot.inflight, std::cmp::Reverse(age));
            if score < best_score {
                best_score = score;
                best = Some(t.key.clone());
            }
        }
        if let Some(key) = best {
            if let Some(slot) = inner.slots.get_mut(&key) {
                slot.inflight += 1;
                slot.last_start = Some(now);
                slot.sent += 1;
                slot.pending.push(now);
            }
            inner.flying += 1;
            inner.starts.push(now);
            inner.arrivals.push(now);
            return Ready::Lease(Lease {
                pool: Arc::clone(self),
                key,
                released: false,
            });
        }
        if rested > 0 && rested >= targets.iter().filter(|t| !exclude.contains(&t.key)).count() {
            return Ready::Rest(if reason.is_empty() { "exhausted".into() } else { reason });
        }
        Ready::Wait(wake.min(Duration::from_millis(2_000)).max(Duration::from_millis(20)))
    }

    /// pending 还有几个账号没被排除。用来判断「再等也没人接」。
    pub fn pending(&self, targets: &[Target], exclude: &HashSet<String>) -> usize {
        targets.iter().filter(|t| !exclude.contains(&t.key)).count()
    }

    /// 等一个座位。释放时 notify，或者到点自己醒。返回 false 表示 pool 已经没人
    /// 会在短时间内放座（调用方不必真的睡满）。
    pub async fn wait(&self, d: Duration) {
        let sleeper = tokio::time::sleep(d);
        tokio::pin!(sleeper);
        tokio::select! {
            _ = self.ready.notified() => {}
            _ = &mut sleeper => {}
        }
    }

    fn release(&self, key: &str) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = inner.slots.get_mut(key) {
                slot.inflight = slot.inflight.saturating_sub(1);
            }
            inner.flying = inner.flying.saturating_sub(1);
        }
        self.ready.notify_waiters();
    }

    /// note 记下一次请求的结局。kind 是 server 判出来的类别。
    pub fn note(&self, key: &str, kind: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let o = self.opts.clone();
        match kind {
            "ok" => inner.served += 1,
            _ => inner.refused += 1,
        }
        // 风控和 405 落在出口上，不在账号上：整个进程停。先做这一步，因为下面要独占
        // 借这个账号的座位。
        if kind == "risk" || kind == "edge" {
            let ms = if kind == "edge" { o.edge_ms } else { o.risk_ms };
            inner.edge_until = Some(now + Duration::from_millis(ms));
            inner.edge_why = if kind == "edge" {
                "the vendor's edge is refusing this address".to_string()
            } else {
                "a risk code came back; the whole address is resting".to_string()
            };
        }
        let live = *self.live.read().unwrap_or_else(|e| e.into_inner());
        let slot = inner.slots.entry(key.to_string()).or_insert_with(|| Slot {
            limit: live.inflight,
            ..Default::default()
        });
        // 这一笔的起步时间：结局回来了，就从待定队列里取最老的一条
        let stamp = if slot.pending.is_empty() {
            None
        } else {
            Some(slot.pending.remove(0))
        };
        match kind {
            "busy" => {
                slot.refusals += 1;
                slot.busy_until = Some(now + Duration::from_millis(o.busy_cooldown_ms));
                // 同一个账号在别人还在飞的时候被再拒一次：压它的座位一档。这条规则来自
                // 插件（plugins#438 见过账号一次只服务 1 个），保留是因为它只是更保守。
                if slot.inflight > 1 {
                    let next = slot.limit.saturating_sub(1).max(1);
                    if next < slot.limit {
                        slot.limit = next;
                        slot.narrow_until = Some(now + Duration::from_millis(o.narrow_ms));
                    }
                }
                slot.recent.push(now);
                let window = Duration::from_millis(o.double_window_ms);
                slot.recent.retain(|t| now.duration_since(*t) < window);
                if slot.recent.len() >= o.double_refusals.max(2) {
                    slot.recent.clear();
                    slot.rest_until = Some(now + Duration::from_millis(o.hold_ms));
                    slot.rest_kind = "busy";
                }
            }
            "spent" => {
                slot.rest_until = Some(now + Duration::from_millis(o.hold_ms));
                slot.rest_kind = "spent";
            }
            "auth" => {
                slot.rest_until = Some(now + Duration::from_millis(o.auth_ms));
                slot.rest_kind = "auth";
            }
            "congested" => {
                // 厂商按出口 IP 限并发。这不是这个账号的问题：给账号记过会让全部账号
                // 在几秒内一起进休息，然后整台机器三分钟发不出任何东西——这正是
                // 「两个任务并行就全断」的现场。停整机一小会儿，让窗口滑过去。
                inner.congest_until = Some(now + Duration::from_millis(o.congest_ms));
            }
            "broken" => {
                slot.rest_until = Some(now + Duration::from_millis(o.broken_ms));
                slot.rest_kind = "broken";
            }
            "risk" | "edge" => {
                slot.rest_until = Some(now + Duration::from_millis(o.risk_ms));
                slot.rest_kind = "risk";
            }
            _ => {}
        }
        // 被拒的尝试没有跑模型：把它的起步时间从工作预算里退回去，后面的请求才排得动。
        // 到达账不退——edge 看到的是「到达了几次」，不是「收下了几个」。
        if kind == "busy" || kind == "congested" {
            if let Some(t) = stamp {
                inner.starts.retain(|s| *s != t);
            }
        }
        drop(inner);
        self.ready.notify_waiters();
    }

    /// retune 改两个能热改的数。已经在飞的请求不受影响；没被挤窄的座位立刻按新的
    /// 并发上限放行。
    pub fn retune(&self, inflight: Option<usize>, rpm: Option<usize>) {
        {
            let mut live = self.live.write().unwrap_or_else(|e| e.into_inner());
            if let Some(n) = inflight {
                live.inflight = n.clamp(1, 8);
            }
            if let Some(n) = rpm {
                live.rpm = n.clamp(1, 120);
            }
        }
        let now_limit = self.live.read().unwrap_or_else(|e| e.into_inner()).inflight;
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            for slot in inner.slots.values_mut() {
                if slot.narrow_until.is_none() {
                    slot.limit = now_limit;
                }
            }
        }
        self.ready.notify_waiters();
    }

    /// limits 是当前生效的那一对数：给 /healthz、设置页和 CLI 看。
    pub fn limits(&self) -> (usize, usize) {
        let live = *self.live.read().unwrap_or_else(|e| e.into_inner());
        (live.inflight, live.rpm)
    }

    pub fn stats(&self) -> (usize, usize, Duration) {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        (inner.served, inner.refused, inner.since.elapsed())
    }

    pub fn resting(&self) -> Option<(Duration, String)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        inner
            .edge_until
            .filter(|u| *u > now)
            .map(|u| (u.duration_since(now), inner.edge_why.clone()))
    }

    /// 每个账号的一行状态，给 /healthz 和界面用。
    pub fn snapshot(&self) -> Vec<(String, usize, usize, Option<Duration>)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        inner
            .slots
            .iter()
            .map(|(k, s)| {
                let rest = [s.rest_until, s.busy_until]
                    .into_iter()
                    .flatten()
                    .filter(|u| *u > now)
                    .map(|u| u.duration_since(now))
                    .max();
                (k.clone(), s.inflight, s.sent, rest)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str) -> Target {
        Target {
            account_id: name.to_string(),
            account_name: name.to_string(),
            provider: "builtin:zai-start-plan".into(),
            plan: "start",
            site: "zai",
            dress: false,
            rank: 0,
            enabled: true,
            key: format!("key-{name}"),
            base: String::new(),
            models: Vec::new(),
            subject: name.to_string(),
            device: String::new(),
            label: name.to_string(),
            aliases: Vec::new(),
        }
    }

    fn pool(targets: &[Target]) -> Arc<Pool> {
        // 起跑间隔是实测出来的节流，测轮转时把它去掉，否则每个账号都得等 800ms。
        let opts = Opts {
            start_gap_ms: 0,
            ..Opts::default()
        };
        let pool = Pool::new(opts);
        pool.touch(targets);
        pool
    }

    fn pick(pool: &Arc<Pool>, targets: &[Target]) -> String {
        match pool.acquire(targets, &HashSet::new()) {
            Ready::Lease(l) => l.key().to_string(),
            Ready::Wait(d) => panic!("expected a seat, waited {d:?}"),
            Ready::Rest(why) => panic!("expected a seat, resting: {why}"),
        }
    }

    /// 一轮请求应该把账号挨个用一遍，而不是黏在最近用过的那个上（老实现是 MRU，
    /// 10 个账号只有 1~2 个真的在耗额度）。
    #[test]
    fn rotation_prefers_the_least_recently_used() {
        let targets: Vec<Target> = ["a", "b", "c"].iter().map(|n| target(n)).collect();
        let pool = pool(&targets);
        let mut order = Vec::new();
        for _ in 0..6 {
            let key = pick(&pool, &targets);
            order.push(key.clone());
            // 放座：Lease 掉 Drop 就放，这里手动来一次更直观
            pool.release(&key);
        }
        assert_eq!(order, ["key-a", "key-b", "key-c", "key-a", "key-b", "key-c"]);
    }

    /// 整机同时只发一条：厂商按出口 IP 限并发，第二条必须排队，而不是再开一个账号。
    #[test]
    fn only_one_request_flies_at_a_time() {
        let targets: Vec<Target> = ["a", "b"].iter().map(|n| target(n)).collect();
        let pool = pool(&targets);
        let lease = match pool.acquire(&targets, &HashSet::new()) {
            Ready::Lease(l) => l,
            _ => panic!("first acquire should get a seat"),
        };
        match pool.acquire(&targets, &HashSet::new()) {
            Ready::Wait(_) => {}
            Ready::Lease(_) => panic!("a second request must not fly while the first is out"),
            Ready::Rest(why) => panic!("it should queue, not answer: {why}"),
        }
        drop(lease);
        let _second = match pool.acquire(&targets, &HashSet::new()) {
            Ready::Lease(l) => l,
            _ => panic!("the seat is free again after release"),
        };
    }

    /// 出口 IP 的并发拒绝不是账号的问题：不许给账号记过，只停整机一小会儿。
    #[test]
    fn concurrency_refusal_does_not_punish_the_account() {
        let targets: Vec<Target> = ["a", "b"].iter().map(|n| target(n)).collect();
        let pool = pool(&targets);
        let key = pick(&pool, &targets);
        pool.release(&key);
        pool.note(&key, "congested");
        let snapshot = pool.snapshot();
        for (_, inflight, _sent, rest) in snapshot.iter() {
            assert_eq!(*inflight, 0);
            assert!(rest.is_none(), "no account may go to rest because of an IP-wide refusal");
        }
    }
}

