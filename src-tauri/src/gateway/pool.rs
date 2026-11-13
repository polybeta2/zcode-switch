// pool.rs — 哪个账号可以现在发请求。
//
// 这件事的唯一实测事实是：一个套餐账号同时只接一个请求，快一点也就 1.2 秒。
// 所以这里的形状是「每账号一张座位表」，而不是一个计数器：座位空着才发，
// 发出去立刻占座，请求结束（Lease 掉 Drop）才放座。
//
// 另外三件事也在这里，因为它们的粒度只能是账号或整个进程：
// - 起跑间隔：同一账号两次起步至少隔 800ms。实测 1.2 秒交错时正好一个进一个退，
//   挨得太近就是 3008。
// - 冷却：刚被 3008 拒过的账号先别问，1.2 秒后再问。厂商那边放得比客户端想得快。
// - 封路：一个账号的 3012（风控）或 405（edge 拒整个 IP）是同一出口上的所有账号
//   共享的，所以这两个是进程级的暂停，不是这个账号的。
//
// 队列是有时间的：排队超过 queue_ms（默认 120 秒）就答 429，而不是一直挂着。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
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
    pub busy_waits: Vec<u64>,
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
            busy_waits: vec![1_000, 2_000],
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
    /// 最近两次被 3008 拒的时间，用来判断「一直在被拒」而不是「偶尔被拒」。
    recent: Vec<Instant>,
}

struct Inner {
    slots: HashMap<String, Slot>,
    /// 进程级封路：405 或 3012 之后所有账号都不发。
    edge_until: Option<Instant>,
    edge_why: String,
    starts: Vec<Instant>,
    served: usize,
    refused: usize,
    since: Instant,
}

pub struct Pool {
    pub opts: Opts,
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

impl Pool {
    pub fn new(opts: Opts) -> Arc<Self> {
        Arc::new(Self {
            opts,
            inner: Mutex::new(Inner {
                slots: HashMap::new(),
                edge_until: None,
                edge_why: String::new(),
                starts: Vec::new(),
                served: 0,
                refused: 0,
                since: Instant::now(),
            }),
            ready: Notify::new(),
        })
    }

    /// touch 给每个目标建座位表，让 /healthz 一上来就能报全量。
    pub fn touch(&self, targets: &[Target]) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let limit = self.opts.inflight.max(1);
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
        // 每分钟的总量：超了就让等，而不是硬发。
        let window = Duration::from_secs(60);
        inner.starts.retain(|t| now.duration_since(*t) < window);
        let gate = if inner.starts.len() >= self.opts.rpm.max(1) {
            inner
                .starts
                .first()
                .map(|t| window.saturating_sub(now.duration_since(*t)))
                .unwrap_or(Duration::from_secs(1))
        } else {
            Duration::ZERO
        };

        let limit = self.opts.inflight.max(1);
        let mut wake = Duration::from_secs(3600);
        let mut rested = 0usize;
        let mut reason = String::new();
        let mut best: Option<String> = None;
        let mut best_score: (u8, usize, Duration) = (u8::MAX, usize::MAX, Duration::MAX);
        for t in targets {
            if exclude.contains(&t.key) {
                continue;
            }
            let slot = inner.slots.entry(t.key.clone()).or_insert_with(|| Slot {
                limit,
                ..Default::default()
            });
            // 一个被挤窄的座位到期恢复
            if let Some(until) = slot.narrow_until {
                if until <= now {
                    slot.narrow_until = None;
                    slot.limit = limit;
                }
            }
            if let Some(until) = slot.rest_until {
                if until > now {
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
            let score = (t.rank, slot.inflight, slot.last_start.map(|l| now.duration_since(l)).unwrap_or(Duration::MAX));
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
            }
            inner.starts.push(now);
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
        let slot = inner.slots.entry(key.to_string()).or_insert_with(|| Slot {
            limit: o.inflight.max(1),
            ..Default::default()
        });
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
            "risk" | "edge" => {
                slot.rest_until = Some(now + Duration::from_millis(o.risk_ms));
                slot.rest_kind = "risk";
            }
            _ => {}
        }
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
