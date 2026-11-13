// server.mjs — the local endpoint: OpenAI in, a ZCode plan out.
//
// Three routes matter: /v1/chat/completions (what OpenAI-shaped clients speak),
// /v1/messages (what Claude Code and anything else Anthropic-shaped speaks, and
// the one shape that goes through without translation) and /v1/models. A fourth,
// /healthz, says what the pool is doing, because a proxy that hides its queues
// is a proxy whose user cannot tell a slow plan from a broken one.
//
// The refusal handling is the interesting part, and it is the vendor's language
// rather than HTTP's: a plan refuses with 200 and {"code":3008}, or with 405 and
// a firewall page, or 429, and each of those has to become a claim the client
// can act on (429 or 503) plus a hold the pool remembers.
import { createHash, timingSafeEqual } from "node:crypto"
import { createServer } from "node:http"
import { Pool, PoolError, BUSY_WAITS } from "./pool.mjs"
import { dress, marked, queryOf, sessionOf } from "./dress.mjs"
import { StreamTranslator, sseText, toAnthropic, toOpenAI } from "./translate.mjs"
import { collect, headersFor, send } from "./vendor.mjs"
import { modelsOf as servedModels, readModels } from "./accounts.mjs"

const MAX_BODY = 32 * 1024 * 1024

const SPENT_CODES = new Set([1005, 1113])
const RATE_CODES = new Set([1302, 1303, 3002])

// kindOf is a response as one of the things the gateway knows how to answer.
export function kindOf(status, code) {
  if (code && SPENT_CODES.has(code)) return "spent"
  if (code && (code === 3008 || code === 3009 || code === 3010)) return "busy"
  if (code && RATE_CODES.has(code)) return "rate"
  if (code === 3012) return "risk"
  if (status === 405) return "edge"
  if (status === 429) return "rate"
  if (status === 401 || status === 403) return "auth"
  if (status === 408 || status === 504) return "timeout"
  if (status >= 500) return "other"
  if (status >= 200 && status < 300) return "ok"
  return "other"
}

const STATUS_FOR = { spent: 402, busy: 429, rate: 429, risk: 403, edge: 503, auth: 401, timeout: 504, other: 502, none: 503, aborted: 499 }

// saidOf is the vendor's own words, so the client's log says what happened
// rather than "upstream error".
function saidOf(j, text) {
  if (j && typeof j === "object") {
    if (typeof j.msg === "string" && j.msg) return j.msg
    if (typeof j.message === "string" && j.message) return j.message
    if (j.error && typeof j.error.message === "string" && j.error.message) return j.error.message
  }
  const t = String(text || "").trim()
  return t ? t.slice(0, 300) : "the provider refused the request"
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

// modelFor is the spelling a plan serves a requested model under, or null when
// it serves no such model. Each plan entry in an account declares its own list,
// and the vendor refuses a name that is not on it with code 3006 — so the name
// the client asked for is matched against the plan, not sent in the hope that
// the models are interchangeable.
export function modelFor(target, want) {
  const models = Array.isArray(target.models) ? target.models : []
  if (!models.length) return want || ""
  if (!want) return models[0]
  const hit = models.find((m) => String(m).toLowerCase() === String(want).toLowerCase())
  return hit || null
}

export function createGateway({ pool, token = "", defaultMaxTokens = 4096, log = () => {}, version = "test", busyWaits = BUSY_WAITS } = {}) {
  const tokenHash = token ? createHash("sha256").update(token).digest() : null

  const authed = (req) => {
    if (!tokenHash) return true // loopback only, and the operator chose no token
    const h = String(req.headers.authorization || "")
    const key = String(req.headers["x-api-key"] || "")
    const given = h.toLowerCase().startsWith("bearer ") ? h.slice(7).trim() : key.trim()
    if (!given) return false
    const g = createHash("sha256").update(given).digest()
    return g.length === tokenHash.length && timingSafeEqual(g, tokenHash)
  }

  const sendJson = (res, status, obj) => {
    const body = JSON.stringify(obj)
    res.writeHead(status, { "content-type": "application/json; charset=utf-8", "content-length": Buffer.byteLength(body) })
    res.end(body)
  }

  const fail = (res, status, message, type = "upstream_error", code = null) =>
    sendJson(res, status, { error: { message, type, code, param: null } })

  // modelsOf is every model some account here can serve, in the plans' own
  // spelling; ZCode's provider config is the fallback for a store with no plan.
  const modelsOf = () => servedModels(pool.targets, readModels())

  // unfitFor is the accounts a request cannot use, because they do not serve
  // the model it names.
  const unfitFor = (want) => {
    const unfit = new Set()
    for (const t of pool.targets) if (modelFor(t, want) === null) unfit.add(t)
    return unfit
  }

  async function readBody(req) {
    const chunks = []
    let size = 0
    for await (const c of req) {
      size += c.length
      if (size > MAX_BODY) throw new Error("the request body passed " + Math.round(MAX_BODY / 1024 / 1024) + " MB")
      chunks.push(c)
    }
    const text = Buffer.concat(chunks).toString("utf8")
    if (!text.trim()) return {}
    return JSON.parse(text)
  }

  // seedOf is the conversation a request belongs to: the client's own session
  // header when it sends one, its user field, else the first turn it sent. A
  // session id that changes per request would miss the plan's prompt cache on
  // every turn, which is the difference between a cheap turn and an expensive
  // one (plugins#387).
  function seedOf(body, req) {
    const h = req.headers["x-session-id"] || req.headers["x-conversation-id"]
    if (typeof h === "string" && h.trim()) return h.trim()
    if (typeof body.user === "string" && body.user.trim()) return body.user.trim()
    const first = Array.isArray(body.messages) ? body.messages.find((m) => m && m.role === "user") : null
    const t = first ? (typeof first.content === "string" ? first.content : JSON.stringify(first.content || "")) : ""
    return t ? createHash("sha256").update(t).digest("hex").slice(0, 32) : ""
  }

  // oneTry sends one body through one account and says what came back.
  async function oneTry({ target, body, sessionId }) {
    const raw = JSON.stringify(body)
    // the query id names what was asked, so it is derived from the body as it
    // will be sent rather than from the client's copy of it
    const text = target.dress ? dress(raw, target.site, target.device, {}, sessionId) : marked(raw, target.device, sessionId)
    const queryId = queryOf(text, sessionId)
    const headers = headersFor(target, { sessionId, queryId })
    const up = await send({ target, body: JSON.parse(text), headers })
    return { up, ctype: String(up.headers["content-type"] || "") }
  }

  async function handleChat(body, res, req) {
    let anth
    const warnings = []
    try {
      anth = toAnthropic(body, { defaultMaxTokens, warn: (m) => warnings.push(m) })
    } catch (e) {
      return fail(res, 400, e.message, "invalid_request_error")
    }
    for (const w of warnings) log("warning: " + w)
    const stream = Boolean(body.stream)
    const seed = seedOf(body, req)
    const tried = new Set()
    let lastSaid = "no account answered"
    let lastKind = "other"
    // an account that does not serve the model asked for is not worth a slot:
    // the vendor would refuse it with 3006 and it would only cost a request
    const want = String(body.model || "")
    const unfit = unfitFor(want)
    if (pool.targets.length && unfit.size >= pool.targets.length) {
      return fail(
        res,
        400,
        "no plan here serves " + JSON.stringify(want) + "; this gateway serves: " + modelsOf().join(", "),
        "invalid_request_error",
        "model_not_found",
      )
    }

    // A request is not given three attempts, it is given the queue's time. With a
    // dozen saved accounts of which most have no plan, counting attempts spends
    // them on dead accounts and then refuses a request that a live plan could
    // have served. So: run until the deadline, keep a busy account eligible
    // (3008 means "not this instant", not "not this account"), and drop only the
    // accounts that said they cannot serve at all.
    const deadline = Date.now() + (pool.opts?.queueMs || 120_000)
    for (let attempt = 0; attempt < 32 && Date.now() < deadline; attempt++) {
      const exclude = new Set([...tried, ...unfit])
      if (pool.pending(exclude) === 0) break
      let slot
      try {
        slot = await pool.acquire({ exclude })
      } catch (e) {
        const kind = e instanceof PoolError ? e.kind : "other"
        // busy after every plan answered "no quota" is a quota answer, not a
        // traffic one, and 402 is what a client knows how to act on
        const late = kind === "busy" && tried.size ? "spent" : kind
        return fail(
          res,
          STATUS_FOR[late] || 503,
          late === "spent" ? lastSaid : e.message,
          late === "edge" ? "upstream_error" : late === "spent" ? "insufficient_quota" : "rate_limit_error",
        )
      }
      const { target, release } = slot
      const sessionId = sessionOf(seed, target.device)
      // the plan is addressed by its own spelling of the model
      const served = modelFor(target, want) || want
      const out = served === anth.model ? anth : { ...anth, model: served }
      let up
      let ctype
      try {
        ;({ up, ctype } = await oneTry({ target, body: out, sessionId }))
      } catch (e) {
        release()
        lastSaid = "the vendor did not answer: " + e.message
        lastKind = /cancelled/.test(e.message) ? "aborted" : "timeout"
        if (lastKind === "aborted") return fail(res, 499, lastSaid, "cancelled")
        tried.add(target)
        log(target.label + ": " + lastSaid)
        continue
      }

      const isStream = stream && ctype.includes("text/event-stream")
      if (isStream && up.status >= 200 && up.status < 300) {
        return pipeChat({ up, res, model: body.model, target, release, pool, log })
      }

      let text = ""
      try {
        text = await collect(up.stream)
      } catch (e) {
        release()
        lastSaid = "the vendor's answer broke off: " + e.message
        lastKind = "other"
        continue
      }
      release()
      let j = null
      try {
        j = JSON.parse(text)
      } catch {}
      const code = j && typeof j.code === "number" ? j.code : 0
      const kind = up.status >= 200 && up.status < 300 ? (code ? kindOf(up.status, code) : "ok") : kindOf(up.status, code)
      if (kind === "ok") {
        pool.note(target, { status: up.status })
        if (stream) {
          // the vendor answered a stream request with a whole body: replay it
          // as the chunk sequence the client is waiting for, sentinel and all,
          // rather than leaving it to parse one object as a stream
          const whole = toOpenAI(j, { model: body.model })
          const msg = whole.choices[0].message
          const tr = new StreamTranslator({ model: whole.model })
          res.writeHead(200, { "content-type": "text/event-stream; charset=utf-8", "cache-control": "no-cache, no-transform", connection: "close" })
          res.write(tr.chunk({ role: "assistant", content: "" }))
          if (msg.reasoning_content) res.write(tr.chunk({ reasoning_content: msg.reasoning_content }))
          if (msg.content) res.write(tr.chunk({ content: msg.content }))
          if (msg.tool_calls) res.write(tr.chunk({ tool_calls: msg.tool_calls.map((c, i) => ({ index: i, ...c })) }))
          tr.usage = { ...(j.usage || {}) }
          res.end(tr.finish())
          return
        }
        return sendJson(res, 200, toOpenAI(j, { model: body.model }))
      }

      pool.note(target, { status: up.status, code, text })
      lastSaid = saidOf(j, text)
      lastKind = kind
      log(target.label + " refused (" + (code || up.status) + "): " + lastSaid.slice(0, 160))
      if (kind === "edge") return fail(res, 503, "the address is resting: " + lastSaid, "upstream_error", code || up.status)
      if (kind === "busy" || kind === "rate") {
        // the vendor's own advice, and what the plugin does: wait a moment and
        // ask the same account again rather than spending another one
        await sleep(busyWaits[Math.min(attempt, busyWaits.length - 1)] || 2000)
        continue
      }
      tried.add(target)
    }
    return fail(
      res,
      STATUS_FOR[lastKind] || 502,
      lastSaid || "no account answered",
      lastKind === "spent" ? "insufficient_quota" : lastKind === "busy" || lastKind === "rate" ? "rate_limit_error" : "upstream_error",
    )
  }

  // pipeChat forwards a streamed answer while it is being written.
  function pipeChat({ up, res, model, target, release, pool: p, log: l }) {
    const tr = new StreamTranslator({ model })
    res.writeHead(200, {
      "content-type": "text/event-stream; charset=utf-8",
      "cache-control": "no-cache, no-transform",
      connection: "keep-alive",
      "x-accel-buffering": "no",
    })
    let done = false
    const finish = () => {
      if (done) return
      done = true
      release()
      try {
        res.end()
      } catch {}
    }
    res.on("close", () => {
      if (!done) {
        try {
          up.stream.destroy()
        } catch {}
        finish()
      }
    })
    ;(async () => {
      try {
        for await (const chunk of up.stream) {
          const out = tr.feed(chunk.toString("utf8"))
          if (out) res.write(out)
        }
        const tail = tr.finish()
        if (tail) res.write(tail)
        p.note(target, { status: 200 })
      } catch (e) {
        if (!res.writableEnded) {
          try {
            res.write(sseText({ error: { message: "the vendor's stream broke off: " + e.message, type: "upstream_error", code: null } }))
          } catch {}
        }
        l(target.label + ": the stream broke off: " + e.message)
      } finally {
        finish()
      }
    })()
  }

  // handleMessages is the Anthropic-shaped endpoint: dressed, forwarded, and
  // returned as it came, because a client that speaks Messages needs no
  // translation and every translation is a place to lose a field.
  async function handleMessages(body, res) {
    if (!body || typeof body !== "object") return fail(res, 400, "a request body is required", "invalid_request_error")
    if (!Array.isArray(body.messages) || !body.messages.length) return fail(res, 400, "messages must not be empty", "invalid_request_error")
    if (!Number.isFinite(body.max_tokens)) body = { ...body, max_tokens: defaultMaxTokens }
    const tried = new Set()
    const want = String(body.model || "")
    const unfit = unfitFor(want)
    if (pool.targets.length && unfit.size >= pool.targets.length) {
      return fail(
        res,
        400,
        "no plan here serves " + JSON.stringify(want) + "; this gateway serves: " + modelsOf().join(", "),
        "invalid_request_error",
        "model_not_found",
      )
    }
    // the same policy as /v1/chat/completions: time, not an attempt count
    const deadline = Date.now() + (pool.opts?.queueMs || 120_000)
    let lastKind = "other"
    let lastSaid = "no account answered"
    for (let attempt = 0; attempt < 32 && Date.now() < deadline; attempt++) {
      const exclude = new Set([...tried, ...unfit])
      if (pool.pending(exclude) === 0) break
      let slot
      try {
        slot = await pool.acquire({ exclude })
      } catch (e) {
        const kind = e instanceof PoolError ? e.kind : "other"
        const late = kind === "busy" && tried.size ? "spent" : kind
        return fail(res, STATUS_FOR[late] || 503, late === "spent" ? lastSaid : e.message, "upstream_error")
      }
      const { target, release } = slot
      const seed = createHash("sha256").update(JSON.stringify(body.messages.slice(0, 1))).digest("hex").slice(0, 32)
      const sessionId = sessionOf(seed, target.device)
      const served = modelFor(target, want) || want
      let up
      let ctype
      try {
        ;({ up, ctype } = await oneTry({ target, body: served === body.model ? body : { ...body, model: served }, sessionId }))
      } catch (e) {
        release()
        tried.add(target)
        lastSaid = "the vendor did not answer: " + e.message
        lastKind = /cancelled/.test(e.message) ? "aborted" : "timeout"
        log(target.label + ": " + e.message)
        continue
      }
      if (up.status >= 200 && up.status < 300) {
        res.writeHead(up.status, { "content-type": ctype || "application/json", "cache-control": "no-cache, no-transform" })
        up.stream.pipe(res)
        up.stream.on("end", () => {
          release()
          pool.note(target, { status: up.status })
        })
        up.stream.on("error", () => release())
        res.on("close", () => {
          try {
            up.stream.destroy()
          } catch {}
          release()
        })
        return
      }
      const text = await collect(up.stream).catch(() => "")
      release()
      let j = null
      try {
        j = JSON.parse(text)
      } catch {}
      const code = j && typeof j.code === "number" ? j.code : 0
      pool.note(target, { status: up.status, code, text })
      const kind = kindOf(up.status, code)
      lastSaid = saidOf(j, text)
      lastKind = kind
      if (kind === "edge") return fail(res, 503, lastSaid, "upstream_error", code || up.status)
      if (kind === "busy" || kind === "rate") {
        await sleep(busyWaits[Math.min(attempt, busyWaits.length - 1)] || 2000)
        continue
      }
      tried.add(target)
    }
    return fail(
      res,
      STATUS_FOR[lastKind] || 502,
      lastSaid,
      lastKind === "spent" ? "insufficient_quota" : lastKind === "busy" || lastKind === "rate" ? "rate_limit_error" : "upstream_error",
    )
  }

  const server = createServer(async (req, res) => {
    const url = new URL(req.url || "/", "http://127.0.0.1")
    try {
      if (req.method === "GET" && (url.pathname === "/healthz" || url.pathname === "/v1/health")) {
        const st = pool.stats()
        return sendJson(res, 200, { ok: true, version, ...st })
      }
      if (!authed(req)) return fail(res, 401, "the gateway's token is missing or wrong (Authorization: Bearer …)", "authentication_error")
      if (req.method === "GET" && url.pathname === "/") {
        return sendJson(res, 200, {
          name: "zcode-switch gateway",
          version,
          endpoints: ["GET /v1/models", "POST /v1/chat/completions", "POST /v1/messages", "GET /healthz"],
          accounts: pool.size,
        })
      }
      if (req.method === "GET" && url.pathname === "/v1/models") {
        const models = modelsOf()
        return sendJson(res, 200, {
          object: "list",
          data: models.map((id) => ({ id, object: "model", created: 0, owned_by: "zcode" })),
        })
      }
      if (req.method === "GET" && url.pathname.startsWith("/v1/models/")) {
        const id = decodeURIComponent(url.pathname.slice("/v1/models/".length))
        return sendJson(res, 200, { id, object: "model", created: 0, owned_by: "zcode" })
      }
      if (req.method === "POST" && url.pathname === "/v1/chat/completions") {
        let body
        try {
          body = await readBody(req)
        } catch (e) {
          return fail(res, 400, e.message, "invalid_request_error")
        }
        return await handleChat(body, res, req)
      }
      if (req.method === "POST" && url.pathname === "/v1/messages") {
        let body
        try {
          body = await readBody(req)
        } catch (e) {
          return fail(res, 400, e.message, "invalid_request_error")
        }
        return await handleMessages(body, res)
      }
      return fail(res, 404, "no such endpoint: " + url.pathname, "invalid_request_error")
    } catch (e) {
      log("gateway error: " + (e && e.stack ? e.stack : e))
      if (!res.headersSent) return fail(res, 500, String(e && e.message ? e.message : e), "api_error")
      try {
        res.end()
      } catch {}
    }
  })
  server.keepAliveTimeout = 65_000
  return server
}
