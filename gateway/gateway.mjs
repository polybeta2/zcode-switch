#!/usr/bin/env node
// gateway.mjs — run the test gateway: a local, OpenAI-shaped door onto the
// accounts this app already switches between.
//
//   npm run gateway                 # 127.0.0.1:8787, token from the store
//   npm run gateway -- --list       # what it would send with, then exit
//   npm run gateway -- --port 9000 --plans start --inflight 2
//
// It holds no sign-in of its own. It reads the accounts Z·SWITCH has already
// saved (~/.zcode-switch/accounts) plus ZCode's own live sign-in, and it sends
// with one account at a time under the pool's rules. Nothing is written except
// its own token file, which exists so a client's config does not change every
// time the gateway restarts.
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs"
import { randomBytes } from "node:crypto"
import { join } from "node:path"
import { loadTargets, modelsOf, readModels, storeDir } from "./accounts.mjs"
import { Pool } from "./pool.mjs"
import { createGateway } from "./server.mjs"

const VERSION = "0.1.0-test"

function parseArgs(argv) {
  const out = {
    host: "127.0.0.1",
    port: 8787,
    token: "",
    plans: null,
    accounts: null,
    includeLive: true,
    // measured 2026-11-13 on a Start Plan: one request in flight per account.
    // Two at once on the same account earns 3008 immediately (probe ramp: every
    // level above 1 served exactly one and refused the rest, 22/22 as 3008), and
    // a refusal arrives in ~150ms — an admission decision, not a slow answer.
    inflight: 1,
    // starts on one account are spaced this far apart: at 1.2s a second request
    // overlaps the first and is served, at 0 it is refused
    gap: 800,
    rpm: 12,
    queueMs: 120000,
    defaultMaxTokens: 4096,
    list: false,
    dryRun: false,
    json: false,
    quiet: false,
    newToken: false,
  }
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]
    const next = () => {
      const v = argv[++i]
      if (v === undefined) throw new Error(a + " needs a value")
      return v
    }
    switch (a) {
      case "--host": out.host = next(); break
      case "--port": out.port = Number(next()); break
      case "--token": out.token = next(); break
      case "--new-token": out.newToken = true; break
      case "--plans": out.plans = next().split(",").map((s) => s.trim()).filter(Boolean); break
      case "--accounts": out.accounts = next().split(",").map((s) => s.trim()).filter(Boolean); break
      case "--no-live": out.includeLive = false; break
      case "--inflight": out.inflight = Number(next()); break
      case "--gap": out.gap = Number(next()); break
      case "--rpm": out.rpm = Number(next()); break
      case "--queue-ms": out.queueMs = Number(next()); break
      case "--default-max-tokens": out.defaultMaxTokens = Number(next()); break
      case "--list": out.list = true; break
      case "--dry-run": out.dryRun = true; break
      case "--json": out.json = true; break
      case "--quiet": out.quiet = true; break
      case "--version": console.log(VERSION); process.exit(0)
      case "--help": case "-h": out.help = true; break
      default:
        if (a.startsWith("-")) throw new Error("unknown flag: " + a)
    }
  }
  return out
}

// tokenFile keeps the gateway's own token beside the accounts, so a client's
// config outlives a restart. It is a local door, not a secret worth an OS
// keychain: loopback only, and rotatable with --new-token.
function tokenFile() {
  return join(storeDir(), "gateway.json")
}

function loadToken(opts) {
  if (opts.token) return { token: opts.token, source: "--token" }
  if (process.env.ZCODE_GATEWAY_TOKEN) return { token: process.env.ZCODE_GATEWAY_TOKEN, source: "ZCODE_GATEWAY_TOKEN" }
  const path = tokenFile()
  if (!opts.newToken && existsSync(path)) {
    try {
      const saved = JSON.parse(readFileSync(path, "utf8"))
      if (saved && typeof saved.token === "string" && saved.token) return { token: saved.token, source: path }
    } catch {}
  }
  const token = "zsg_" + randomBytes(18).toString("hex")
  try {
    mkdirSync(storeDir(), { recursive: true })
    writeFileSync(path, JSON.stringify({ token, createdAt: new Date().toISOString(), note: "Z·SWITCH test gateway token" }, null, 2) + "\n", "utf8")
  } catch {}
  return { token, source: path }
}

function printList(targets) {
  const rows = targets.map((t, i) => [
    String(i + 1),
    t.accountName + (t.aliases && t.aliases.length ? " +" + t.aliases.length : ""),
    t.plan,
    t.dress ? "dressed" : "plain",
    (Array.isArray(t.models) && t.models.length ? t.models.join(",") : "any").slice(0, 30),
    t.key.length + " chars",
    t.base || "(default)",
  ])
  const head = ["#", "account", "plan", "wire", "models", "key", "base"]
  const widths = head.map((h, i) => Math.max(h.length, ...rows.map((r) => r[i].length)))
  const line = (r) => r.map((c, i) => (i === r.length - 1 ? c : c.padEnd(widths[i]))).join("  ").trimEnd()
  console.log(line(head))
  console.log(widths.map((w) => "-".repeat(w)).join("  "))
  for (const r of rows) console.log(line(r))
  const counts = targets.reduce((acc, t) => ((acc[t.plan] = (acc[t.plan] || 0) + 1), acc), {})
  console.log("")
  console.log(targets.length + " target(s): " + Object.entries(counts).map(([k, v]) => v + " " + k).join(", "))
  console.log("serves: " + modelsOf(targets, readModels()).join(", "))
}

const HELP = [
  "Z·SWITCH gateway (test build " + VERSION + ")",
  "",
  "  --port <n>            listen port (default 8787)",
  "  --host <addr>         listen address (default 127.0.0.1)",
  "  --token <s>           require this token instead of the stored one",
  "  --new-token           mint a new token and store it",
  "  --plans <a,b>         start | coding | api (default: all, best first)",
  "  --accounts <a,b>      account ids or names (default: all)",
  "  --no-live             do not include ZCode's own live sign-in",
  "  --inflight <n>        requests per account at once (default 1, as measured)",
  "  --gap <ms>            shortest gap between two starts on one account (default 800)",
  "  --rpm <n>             starts per minute per address (default 12, 0 = off)",
  "  --queue-ms <n>        how long a request may wait for a slot (default 120000)",
  "  --default-max-tokens  used when a Messages request omits max_tokens",
  "  --list                print the accounts it would send with, then exit",
  "  --dry-run             print the configuration, do not listen",
  "  --json                machine-readable startup line",
  "  --quiet               no per-request log",
  "",
  "Endpoints: GET /healthz  GET /v1/models  POST /v1/chat/completions  POST /v1/messages",
].join("\n")

async function main() {
  let opts
  try {
    opts = parseArgs(process.argv.slice(2))
  } catch (e) {
    console.error(e.message)
    console.error("try --help")
    process.exit(2)
  }
  if (opts.help) return console.log(HELP)

  const targets = loadTargets({ includeLive: opts.includeLive, plans: opts.plans, accounts: opts.accounts })
  if (opts.list || !targets.length) {
    printList(targets)
    if (!targets.length) {
      console.error("")
      console.error("No account has a key this gateway can send. Sign in with Z·SWITCH first,")
      console.error("or save an account whose provider has a Start Plan or coding plan key.")
      process.exit(1)
    }
    if (opts.list) return
  }

  const { token, source } = loadToken(opts)
  const log = opts.quiet ? () => {} : (m) => console.log(new Date().toISOString().slice(11, 19) + "  " + m)
  const pool = new Pool(targets, { inflight: opts.inflight, startGapMs: opts.gap, rpm: opts.rpm, queueMs: opts.queueMs, log })
  const server = createGateway({ pool, token, defaultMaxTokens: opts.defaultMaxTokens, log, version: VERSION })

  if (opts.dryRun) {
    const view = {
      listen: opts.host + ":" + opts.port,
      token: token.slice(0, 10) + "…",
      tokenSource: source,
      targets: targets.map((t) => ({ account: t.accountName, plan: t.plan, provider: t.providerId, wire: t.dress ? "dressed" : "plain" })),
      pool: { inflight: opts.inflight, rpm: opts.rpm, queueMs: opts.queueMs },
    }
    return console.log(JSON.stringify(view, null, 2))
  }

  server.on("error", (e) => {
    console.error("the gateway could not listen: " + e.message)
    process.exit(1)
  })
  server.listen(opts.port, opts.host, () => {
    const base = "http://" + opts.host + ":" + opts.port
    const plans = targets.reduce((acc, t) => ((acc[t.plan] = (acc[t.plan] || 0) + 1), acc), {})
    if (opts.json) {
      console.log(JSON.stringify({ listen: base, token, tokenSource: source, targets: targets.length, plans, models: modelsOf(targets, readModels()) }))
      return
    }
    console.log("")
    console.log("  Z·SWITCH gateway — test build " + VERSION)
    console.log("  " + base + "   (" + targets.length + " target(s): " + Object.entries(plans).map(([k, v]) => v + " " + k).join(", ") + ")")
    console.log("  token: " + token + "   from " + source)
    console.log("")
    console.log("  OpenAI-shaped:     POST " + base + "/v1/chat/completions")
    console.log("  Anthropic-shaped:  POST " + base + "/v1/messages")
    const canServe = modelsOf(targets, readModels())
    console.log("  models:            " + (canServe.length ? canServe.join(", ") : "(none declared)"))
    console.log("  models:            GET  " + base + "/v1/models")
    console.log("  state:             GET  " + base + "/healthz")
    console.log("")
    console.log("  curl " + base + "/v1/chat/completions \\")
    console.log("    -H 'Authorization: Bearer " + token + "' -H 'Content-Type: application/json' \\")
    console.log("    -d '{\"model\":\"" + (canServe[0] || "glm-4.6") + "\",\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}]}'")
    console.log("")
    console.log("  Every account goes out from this one address: the vendor's cap is per account,")
    console.log("  the firewall's is per address. " + opts.inflight + " in flight per account at most;")
    console.log("  the plan refuses a second one with 3008, so extra accounts buy concurrency,")
    console.log("  not extra speed.")
    console.log("")
  })

  let closing = false
  const shutdown = () => {
    if (closing) return
    closing = true
    console.log("")
    console.log("closing…")
    server.close(() => process.exit(0))
    setTimeout(() => process.exit(0), 2000).unref()
  }
  process.on("SIGINT", shutdown)
  process.on("SIGTERM", shutdown)
}

main().catch((e) => {
  console.error(e && e.stack ? e.stack : String(e))
  process.exit(1)
})
