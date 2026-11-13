// pool.mjs — how many requests an account is given at once, and when it rests.
//
// The vendor's limits are per account and are enforced by refusing a request
// with a code, not by telling us a number (§ concurrency, measured by the probe
// in ../probe): 3008 is the account's concurrency, 3009 a model's, 3010 a turn
// that has not come yet. Sustained pressure on the Start Plan escalates: after
// a run of double refusals the edge firewall refuses the whole address with 405
// for an hour or two, and every account of that address goes with it. So this
// pool is deliberately timid — a per-account slot count, a queue that gives up,
// a rate ceiling, and holds that outlast the refusal that caused them.
//
// It is a test build's approximation of the plugin's gate (magpie
// packages/zcode/index.mjs: startGate 2096, narrow 2095-2119, busy 2059-2070,
// 2258-2260, edge 2432-2439), not a reimplementation of it: the numbers are the
// same, the bookkeeping is smaller, and a waiter is polled rather than woken.

export const BUSY_CODES = new Set([3008, 3009, 3010])
export const BUSY_WAITS = [1000, 2000]

export class PoolError extends Error {
  constructor(kind, message, retryAfterMs = 0) {
    super(message)
    this.kind = kind // "edge" | "busy" | "resting" | "none" | "aborted"
    this.retryAfterMs = retryAfterMs
  }
}

const now = () => Date.now()

export class Pool {
  constructor(targets, opts = {}) {
    this.opts = {
      // how many requests one account may have in flight: ONE, measured.
      // ../probe fired requests 1.2s apart and recorded the timeline exactly —
      // i=0 ok (1222ms), i=1 ok (1200→4635ms), i=2 and i=3 refused with 3008 in
      // ~150ms while i=1 was still running, i=4 and i=5 ok once it had finished.
      // Nothing overlapped without a 3008. The plugin's maxConcurrency: 3 is a
      // client-side gate, not the vendor's cap: it admits three and then answers
      // each 3008 by waiting a second and asking again, which is why its log
      // reads "up to ~10 in flight against a cap of about three". Spacing the
      // starts is what makes a queue look like three-at-once.
      inflight: 1,
      // the shortest gap between two starts on one account: a second request
      // sent in the same instant as the first is refused in ~150ms
      startGapMs: 800,
      // after a 3008 the account is skipped for this long, then tried again
      busyCooldownMs: 1200,
      // how long a request may wait for a slot before it is refused
      queueMs: 120_000,
      // starts per minute across every account of this address; 0 is no limit,
      // and 12 is the plugin's "sustained ~18/min brings 3008 storms" with room
      rpm: 12,
      // a re-refused account is narrowed for this long (plugin NARROW_FOR)
      narrowMs: 300_000,
      // how many double refusals arm the busy hold, and over what window
      doubleRefusals: 5,
      doubleWindowMs: 60_000,
      // the busy hold, the edge hold and an account's own risk rest
      holdMs: 180_000,
      edgeMs: 180_000,
      riskMs: 300_000,
      // a sign-in the vendor does not accept is not retried soon
      authMs: 600_000,
      ...opts,
    }
    this.targets = targets.map((t) => ({
      ...t,
      inflight: 0,
      limit: this.opts.inflight,
      narrowUntil: 0,
      narrowLimit: 0,
      gapUntil: 0,
      gapNow: 0,
      busyUntil: 0,
      holdUntil: 0,
      holdWhy: "",
      doubles: [],
      lastUsed: 0,
      lastStart: 0,
      served: 0,
      refused: 0,
    }))
    this.edgeUntil = 0
    this.edgeWhy = ""
    this.starts = []
    this.log = this.opts.log || (() => {})
  }

  get size() {
    return this.targets.length
  }

  // edgeFor is how long this address has left before the firewall lets it back.
  edgeFor() {
    return Math.max(0, this.edgeUntil - now())
  }

  // limitOf is one account's current slot count: its own, or the narrowed one
  // after a refusal arrived while the account was busy.
  limitOf(t) {
    if (t.narrowUntil > now() && t.narrowLimit > 0) return Math.min(t.limit, t.narrowLimit)
    return t.limit
  }

  // gapOf is the pacing this account is under right now: the default, or the
  // wider one a burst refusal bought it.
  gapOf(t) {
    return t.gapUntil > now() && t.gapNow > 0 ? t.gapNow : this.opts.startGapMs
  }

  // pending counts the accounts a request could still reach: not excluded, not
  // resting. When it is zero, waiting for a slot would only spend the queue.
  pending(exclude = new Set()) {
    const at = now()
    if (this.edgeUntil > at) return 0
    return this.targets.filter((t) => !exclude.has(t) && t.holdUntil <= at).length
  }

  // pick chooses the account a request should use: the one with the most room
  // relative to its own limit, and the earliest used among equals, so a queue
  // spreads over accounts instead of piling on the first.
  pick(exclude = new Set()) {
    const at = now()
    // while the address rests, no account of it is available: the firewall
    // refuses by address, so a slot here would only earn another 405
    if (this.edgeUntil > at) return null
    let best = null
    let bestScore = Infinity
    for (const t of this.targets) {
      if (exclude.has(t)) continue
      if (t.holdUntil > at) continue
      // a 3008 says "not this instant", not "not this account": wait it out
      if (t.busyUntil > at) continue
      // and leave the account its gap, which is what the burst refusal asked for
      if (at - t.lastStart < this.gapOf(t)) continue
      const limit = this.limitOf(t)
      if (t.inflight >= limit) continue
      const score = (t.inflight + 1) / limit
      if (score < bestScore - 1e-9 || (Math.abs(score - bestScore) < 1e-9 && t.lastUsed < best.lastUsed)) {
        best = t
        bestScore = score
      }
    }
    return best
  }

  // rpmWait waits until a start fits the ceiling over the last minute. It is
  // what keeps a client's parallel turn from spending an evening's budget in an
  // afternoon.
  async rpmWait(signal) {
    const rpm = this.opts.rpm
    if (!rpm) return
    for (;;) {
      const at = now()
      this.starts = this.starts.filter((s) => at - s < 60_000)
      if (this.starts.length < rpm) return
      const wait = Math.max(50, 60_000 - (at - this.starts[0]) + 5)
      await sleep(wait, signal)
    }
  }

  // acquire waits for a slot and returns the account it took, with the release
  // the caller must call (once) when the response is done.
  async acquire({ exclude = new Set(), signal } = {}) {
    const deadline = now() + this.opts.queueMs
    for (;;) {
      if (this.edgeFor() > 0) {
        throw new PoolError("edge", "the address is resting: the firewall refused it (" + this.edgeWhy + ")", this.edgeFor())
      }
      const t = this.pick(exclude)
      if (t) {
        await this.rpmWait(signal)
        // another request may have taken the slot while this one waited
        if (t.inflight >= this.limitOf(t) || t.holdUntil > now()) continue
        t.inflight++
        t.lastUsed = now()
        t.lastStart = now()
        this.starts.push(now())
        let released = false
        return {
          target: t,
          release: () => {
            if (released) return
            released = true
            t.inflight = Math.max(0, t.inflight - 1)
          },
        }
      }
      if (!this.targets.length) throw new PoolError("none", "no account has a key this gateway can send")
      const left = deadline - now()
      if (left <= 0) throw new PoolError("busy", "every account is at its limit and the queue waited " + Math.round(this.opts.queueMs / 1000) + "s")
      await sleep(Math.min(500, left), signal)
    }
  }

  // note is what a response taught the pool. It is the whole safety story: the
  // vendor never says "slow down", it says "no" with a code, and every code has
  // a hold that is longer than the refusal it answers.
  note(t, { status = 0, code = 0, text = "" } = {}) {
    const at = now()
    const body = String(text || "")
    if (BUSY_CODES.has(code)) {
      t.refused++
      t.doubles = t.doubles.filter((d) => at - d < this.opts.doubleWindowMs)
      t.doubles.push(at)
      // a refusal while this account is still serving says the vendor's real
      // cap is lower than ours right now: lower it, do not argue
      if (t.inflight > 0) {
        t.narrowUntil = at + this.opts.narrowMs
        t.narrowLimit = Math.max(1, Math.min(t.limit, t.inflight))
        this.log("narrowed " + t.label + " to " + t.narrowLimit + " for " + Math.round(this.opts.narrowMs / 1000) + "s")
      }
      if (t.doubles.length >= this.opts.doubleRefusals) {
        t.holdUntil = at + this.opts.holdMs
        t.holdWhy = "refused " + t.doubles.length + " times in a minute (code " + code + ")"
        t.doubles = []
        this.log("resting " + t.label + " for " + Math.round(this.opts.holdMs / 1000) + "s: " + t.holdWhy)
      }
      return
    }
    if (code === 3012 || /unusual activity|3012/.test(body)) {
      t.holdUntil = at + this.opts.riskMs
      t.holdWhy = "risk control (3012)"
      this.log("resting " + t.label + " for " + Math.round(this.opts.riskMs / 1000) + "s: risk control")
      return
    }
    if (status === 405 || /aliyun|blocked due to/i.test(body)) {
      this.edgeUntil = at + this.opts.edgeMs
      this.edgeWhy = "code " + (code || status) + (body ? ": " + body.slice(0, 120) : "")
      this.log("the address is resting for " + Math.round(this.opts.edgeMs / 1000) + "s: " + this.edgeWhy)
      return
    }
    if (status === 401 || status === 403) {
      t.holdUntil = at + this.opts.authMs
      t.holdWhy = "sign-in refused (" + status + ")"
      this.log("resting " + t.label + " for " + Math.round(this.opts.authMs / 1000) + "s: sign-in refused")
      return
    }
    if (status >= 200 && status < 300) t.served++
  }

  // stats is what GET /healthz and the console table show.
  stats() {
    const at = now()
    return {
      edgeRestingMs: Math.max(0, this.edgeUntil - at),
      edgeWhy: this.edgeWhy,
      accounts: this.targets.map((t) => ({
        label: t.label,
        account: t.accountName,
        plan: t.plan,
        provider: t.providerId,
        inflight: t.inflight,
        limit: this.limitOf(t),
        served: t.served,
        refused: t.refused,
        restingMs: Math.max(0, t.holdUntil - at),
        restingWhy: t.holdWhy,
        narrowed: t.narrowUntil > at,
      })),
    }
  }
}

function sleep(ms, signal) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(new PoolError("aborted", "cancelled"))
    const id = setTimeout(() => {
      signal?.removeEventListener?.("abort", onAbort)
      resolve()
    }, ms)
    const onAbort = () => {
      clearTimeout(id)
      reject(new PoolError("aborted", "cancelled"))
    }
    signal?.addEventListener?.("abort", onAbort, { once: true })
  })
}
