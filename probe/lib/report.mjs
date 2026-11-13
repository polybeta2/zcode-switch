// report.mjs — what an answer means, and the file a run leaves behind.
//
// The kinds are ZCode's own, as the plugin reads them (index.mjs:2032-2068,
// 2303-2320): a refusal that is an in-flight cap is not a spent plan and not a
// rate limit, and the three have different fixes. The probe keeps them apart so
// a run says which wall it hit.
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs"
import { join } from "node:path"

// 3008: the account's cap. 3009: the model's. 3010: the turn has not come.
export const BUSY_CODES = new Set(["3008", "3009", "3010"])
// 1113 / 1005: the plan itself is spent.
const SPENT_CODES = new Set(["1113", "1005"])
// 1302 / 1303 / 3002: a rate limit, not a concurrency cap.
const RATE_CODES = new Set(["1302", "1303", "3002"])
// 3012: the vendor's risk control has taken an interest in the account.
const RISK_CODE = "3012"

export const KINDS = ["ok", "busy", "spent", "rate", "risk", "edge", "auth", "timeout", "net", "other"]

// classify says what one answer was. The body is read before the status: ZCode
// reports its own codes in the body, sometimes under a 200.
export function classify({ status, text }) {
  let body = null
  try {
    body = JSON.parse(text)
  } catch {}
  const raw = body?.code ?? body?.error?.code ?? body?.error?.type ?? body?.type
  const code = raw == null ? "" : String(raw)
  const msg = String(body?.msg ?? body?.message ?? body?.error?.message ?? "")
  if (status === 405) return { kind: "edge", code, msg }
  if (/aliyun|errors\.aliyun|blocked|access denied/i.test(String(text).slice(0, 400))) return { kind: "edge", code, msg }
  if (code === RISK_CODE || /unusual activity|risk control|风控/i.test(msg)) return { kind: "risk", code, msg }
  if (BUSY_CODES.has(code) || /concurrency limit exceeded/i.test(msg)) return { kind: "busy", code, msg }
  if (SPENT_CODES.has(code)) return { kind: "spent", code, msg }
  if (status === 429 || RATE_CODES.has(code)) return { kind: "rate", code, msg }
  if (status === 401 || status === 403) return { kind: "auth", code, msg }
  if (status >= 200 && status < 300 && !body?.error) return { kind: "ok", code, msg }
  return { kind: "other", code, msg }
}

// fireOne sends one request and never retries: a probe that retried would
// measure its own patience rather than the server's cap.
export async function fireOne(req, { timeoutMs = 120000 } = {}) {
  const t0 = performance.now()
  try {
    const res = await fetch(req.url, {
      method: "POST",
      headers: req.headers,
      body: req.body,
      signal: AbortSignal.timeout(timeoutMs),
    })
    const text = await res.text()
    const ms = Math.round(performance.now() - t0)
    const c = classify({ status: res.status, text })
    return { ok: c.kind === "ok", kind: c.kind, code: c.code, msg: c.msg, status: res.status, ms, bytes: text.length, sample: text.slice(0, 300) }
  } catch (e) {
    const ms = Math.round(performance.now() - t0)
    const timeout = e?.name === "TimeoutError" || e?.name === "AbortError"
    return { ok: false, kind: timeout ? "timeout" : "net", code: "", msg: String(e?.message || e), status: 0, ms, bytes: 0, sample: "" }
  }
}

// summarizeCounts is a level's tally in kind order, zeroes included.
export function summarizeCounts(results) {
  const out = Object.fromEntries(KINDS.map((k) => [k, 0]))
  for (const r of results) out[r.kind] = (out[r.kind] || 0) + 1
  return out
}

export function countsLine(counts) {
  return KINDS.filter((k) => counts[k]).map((k) => `${k}=${counts[k]}`).join(" ")
}

// Run writes one folder per run: an append-only jsonl of every attempt, a
// summary markdown, and the metadata a later reader needs to trust either.
export function openRun(dir) {
  mkdirSync(dir, { recursive: true })
  const stamp = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19)
  const base = join(dir, `run-${stamp}`)
  return {
    base,
    jsonl: base + ".jsonl",
    summary: base + ".md",
    append(record) {
      appendFileSync(this.jsonl, JSON.stringify(record) + "\n")
    },
    write(text) {
      writeFileSync(this.summary, text)
    },
  }
}

// formatTable renders the ramp as the one table a reader wants: at what
// concurrency the account still answers everything, and what it says past it.
export function formatTable(levels) {
  const head = ["level", "ok", "busy", "rate", "spent", "risk", "edge", "other", "p50ms", "maxms"]
  const rows = [head]
  for (const l of levels) {
    const c = l.counts
    const ok = l.results.filter((r) => r.ok).map((r) => r.ms).sort((a, b) => a - b)
    const all = l.results.map((r) => r.ms).sort((a, b) => a - b)
    const p50 = ok.length ? ok[Math.floor(ok.length / 2)] : all.length ? all[Math.floor(all.length / 2)] : 0
    rows.push([
      String(l.level),
      String(c.ok),
      String(c.busy),
      String(c.rate),
      String(c.spent),
      String(c.risk),
      String(c.edge),
      String(c.other + c.auth + c.timeout + c.net),
      String(p50),
      String(all.length ? all[all.length - 1] : 0),
    ])
  }
  const widths = rows[0].map((_, i) => Math.max(...rows.map((r) => String(r[i]).length)))
  return rows.map((r) => r.map((cell, i) => String(cell).padEnd(widths[i])).join("  ").trimEnd()).join("\n")
}
