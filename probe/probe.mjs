#!/usr/bin/env node
// probe.mjs — measure what a ZCode Start Plan will actually serve.
//
//   node probe/probe.mjs --check                 one request: does the sign-in work?
//   node probe/probe.mjs --dry-run               the request we would send, token redacted
//   node probe/probe.mjs                         the concurrency ramp (1,2,3,4,6)
//   node probe/probe.mjs --levels 1,2,3 --gap 45 the safe, short version
//   node probe/probe.mjs --mode models           is 3009 per model? (see README)
//   node probe/probe.mjs --mode rate --rate 12 --yes-risk --minutes 4
//                                                the throughput wall — this one can
//                                                block the IP for 1–2 hours
//
// Nothing here writes to the ZCode folder. Every attempt is appended to
// probe/results/run-*.jsonl and a summary to run-*.md.
import { parseArgs } from "node:util"
import { fileURLToPath } from "node:url"
import { readSignIn, describeSignIn, zcodeDir, jwtInfo } from "./lib/creds.mjs"
import { loadTargets } from "../gateway/accounts.mjs"
import { loadReference, buildStartPlanRequest, redacted, defaultRefPath } from "./lib/wire.mjs"
import { fireOne, openRun, summarizeCounts, countsLine, formatTable } from "./lib/report.mjs"

const HELP = `probe.mjs — ZCode Start Plan behaviour probe

  --plan                 is the plan active? reads its balance, sends no model request
  --check                one request, then stop
  --dry-run              print the dressed request (token redacted), send nothing
  --mode ramp|models|rate   default ramp
  --levels 1,2,3,4,6     concurrency per round, ramp mode
  --rounds 2             rounds per level
  --gap 45000            ms to rest between rounds
  --stagger 0            ms between launches inside a level (0 = all at once)
  --model GLM-5.3-Flash  the model a ramp uses
  --models a,b,c          models mode: three at once, one each
  --prompt "..."         the user text each request carries
  --skip-preflight       measure even when the preflight request is refused
  --max-requests 40      hard stop for the whole run
  --timeout 120000       per-request timeout, ms
  --app-version 3.17.2   X-ZCode-App-Version
  --ref <path>           packages/zcode/index.mjs (default ../../_ref/...)
  --dir <path>           the zcode v2 folder to read (default ~/.zcode/v2)
  --account <name|id>    measure one account this app saved, not the live sign-in
  --out <dir>            where results go (default probe/results)
  --stream               ask for stream:true
  --json                 print the raw result records too
  --yes-risk             allow --mode rate (can block the IP for 1–2 hours)
`

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
const now = () => new Date().toLocaleTimeString("en-GB", { hour12: false })

function bail(msg, code = 1) {
  console.error("\n" + msg + "\n")
  process.exit(code)
}

async function main() {
  const { values: v } = parseArgs({
    options: {
      mode: { type: "string", default: "ramp" },
      levels: { type: "string", default: "1,2,3,4,6" },
      rounds: { type: "string", default: "2" },
      gap: { type: "string", default: "45000" },
      stagger: { type: "string", default: "0" },
      model: { type: "string", default: "GLM-5.3-Flash" },
      models: { type: "string", default: "GLM-5.3-Flash,GLM-5.2,GLM-5-Turbo" },
      prompt: { type: "string", default: "Reply with exactly: ok" },
      "max-requests": { type: "string", default: "40" },
      timeout: { type: "string", default: "120000" },
      "app-version": { type: "string", default: "" },
      ref: { type: "string", default: "" },
      dir: { type: "string", default: "" },
      out: { type: "string", default: "" },
      rate: { type: "string", default: "12" },
      minutes: { type: "string", default: "4" },
      account: { type: "string", default: "" },
      check: { type: "boolean", default: false },
      plan: { type: "boolean", default: false },
      "skip-preflight": { type: "boolean", default: false },
      "dry-run": { type: "boolean", default: false },
      stream: { type: "boolean", default: false },
      json: { type: "boolean", default: false },
      "yes-risk": { type: "boolean", default: false },
      help: { type: "boolean", default: false },
    },
  })

  if (v.help) return console.log(HELP)

  // ---- the sign-in ------------------------------------------------------------
  let signIn
  try {
    signIn = readSignIn(v.dir || zcodeDir())
  } catch (e) {
    bail("cannot read the ZCode sign-in: " + e.message)
  }
  // --account measures a plan the app saved, which is the only way to probe one
  // whose plan has started while the live sign-in is still on an account that
  // has none. It reads the same store the gateway reads, so both see one set.
  if (v.account) {
    const targets = loadTargets({ includeLive: false, plans: ["start"] })
    const hit = targets.find((t) => t.accountId === v.account || t.accountName === v.account || t.accountName.startsWith(v.account))
    if (!hit) {
      bail(
        "no saved Start Plan account matches " + JSON.stringify(v.account) + "\nsaved: " +
          (targets.map((t) => t.accountName).join(", ") || "(none)"),
      )
    }
    signIn = {
      dir: "account " + hit.accountId,
      credPath: "",
      cfgPath: "",
      jwt: hit.key,
      source: "saved account \"" + hit.accountName + "\" (v" + hit.plan + ")",
      base: hit.base,
      enabled: hit.enabled,
      device: hit.device,
      signIn: jwtInfo(hit.key),
      activeProvider: hit.providerId,
      codingPlanKeys: 0,
      models: hit.models,
    }
  }
  const level = v["app-version"] || ""
  const known = { jwt: signIn.jwt, device: signIn.device }

  // ---- the plan itself ----------------------------------------------------------
  // The balance read answers "is the plan on yet?" without spending a model
  // request: an empty plans array is an account with no Start Plan at this
  // moment, which is also why a model request would come back 1005.
  if (v.plan) {
    console.log(describeSignIn(signIn))
    if (!signIn.jwt) bail("no Start Plan token found — sign in to ZCode first")
    const url = "https://zcode.z.ai/api/v1/zcode-plan/billing/balance?app_version=3.17.2"
    const res = await fetch(url, {
      headers: {
        "Content-Type": "application/json",
        Accept: "application/json",
        "User-Agent": "ZCode/3.17.2",
        Authorization: "Bearer " + signIn.jwt,
        "X-Device-Mid": signIn.device,
      },
    })
    const text = await res.text()
    console.log("\nbalance  http " + res.status + "\n")
    let env = null
    try { env = JSON.parse(text) } catch {}
    if (!env) return console.log(text.slice(0, 1200))
    console.log(JSON.stringify(env, null, 2).slice(0, 3000))
    const plans = env?.data?.plans ?? []
    const balances = env?.data?.balances ?? []
    if (env?.data?.server_time) {
      const skew = Math.round(env.data.server_time - Date.now() / 1000)
      console.log("\nserver clock: " + new Date(env.data.server_time * 1000).toLocaleString() + (Math.abs(skew) > 120 ? "  (this machine is " + skew + "s off)" : ""))
    }
    console.log(plans.length ? "\nplan: " + plans.length + " entr" + (plans.length === 1 ? "y" : "ies") + ", " + balances.length + " balance row(s)" : "\nno Start Plan on this account right now — a model request answers 1005 until one starts")
    return
  }

  let ref
  try {
    ref = await loadReference(v.ref || defaultRefPath())
  } catch (e) {
    bail(String(e.message || e))
  }

  const timeoutMs = Number(v.timeout) || 120000
  const maxRequests = Number(v["max-requests"]) || 40
  const mk = (model, seed) =>
    buildStartPlanRequest({
      ref,
      jwt: known.jwt,
      device: known.device,
      model,
      prompt: v.prompt,
      base: signIn.base || undefined,
      maxTokens: 16,
      stream: !!v.stream,
      sessionSeed: seed,
      appVersion: level || undefined,
    })

  console.log("zcode start plan probe  " + new Date().toLocaleString())
  console.log(describeSignIn(signIn))
  console.log("plugin ref   " + (v.ref || defaultRefPath()))

  // ---- dry run ----------------------------------------------------------------
  if (v["dry-run"]) {
    const req = mk(v.model, "dry-run")
    console.log("\n--- request (token redacted) ---")
    console.log(JSON.stringify(redacted(req), null, 2).slice(0, 4000))
    console.log("\nblock count: " + (redacted(req).body.system || []).length + "   system chars: " +
      (redacted(req).body.system || []).reduce((n, b) => n + (b.text || "").length, 0))
    return
  }

  const outDir = v.out || fileURLToPath(new URL("./results/", import.meta.url))
  const run = openRun(outDir)
  const meta = { started: new Date().toISOString(), mode: v.mode, args: v, signIn: { ...signIn.signIn, dir: signIn.dir, device: signIn.device }, model: v.model }
  run.append({ type: "meta", ...meta })

  let used = 0
  let stopped = null
  const allLevels = []
  const t0 = Date.now()

  const send = async (req, note) => {
    if (used >= maxRequests) {
      stopped = stopped || `max-requests (${maxRequests}) reached`
      return null
    }
    used++
    const r = await fireOne(req, { timeoutMs })
    run.append({ type: "attempt", at: new Date().toISOString(), note, ...r })
    if (v.json) console.log(JSON.stringify({ note, ...r }))
    if (r.kind === "edge") stopped = "the edge firewall answered 405 / a block page"
    return r
  }

  // ---- preflight --------------------------------------------------------------
  // One request before anything is measured. A plan with no quota left answers
  // every request the same way, so a ramp would produce a table of one number
  // and say nothing about concurrency — better to say so than to plot it.
  if (!v.check && !v["skip-preflight"] && v.mode !== "rate") {
    const first = await send(mk(v.model, "preflight"), { preflight: true, level: 1 })
    const kind = first ? first.kind : "unknown"
    if (kind !== "ok") {
      const why = {
        spent:
          `the plan answered ${first.code || "1005"} "${first.msg || "exceed quota limit"}" — this account has no Start Plan quota right now.\n` +
          "Nothing about concurrency can be measured until it resets. Re-run after the reset,\n" +
          "or pass --skip-preflight to measure what the refusal looks like anyway.",
        auth: "the token was refused (401/403). Sign in to ZCode again, then re-run.",
        edge: "the edge firewall answered 405 — this address is blocked for 1–2 hours. Stop.",
        risk: "code 3012: the vendor's risk control has taken an interest in this account. Stop for the day.",
        busy: `the plan answered ${first.code || "3008"} before anything else was in flight — that is not a probe result, it is a leftover cap. Wait a minute and re-run.`,
        rate: "the plan answered with a rate limit before anything else was in flight. Wait a minute and re-run.",
        timeout: "the request timed out. Check the network before reading anything into a ramp.",
        net: "the request could not be sent: " + (first.msg || "network error"),
        other: `unexpected answer: http ${first.status} ${first.code || ""} ${String(first.msg || "").slice(0, 120)}`,
      }[kind]
      console.log("\npreflight: " + kind.toUpperCase() + "  http " + first.status + "  " + (first.code || "") + "\n\n" + why + "\n")
      run.write("# probe\n\n" + new Date().toISOString() + "\n\npreflight stopped the run: " + kind + "\n\n" + why + "\n")
      run.append({ type: "preflight-stop", kind, ...first })
      return
    }
    console.log(`\npreflight ok  http ${first.status}  ${first.ms}ms  ${first.bytes}B\n`)
  }

  // ---- one request ------------------------------------------------------------
  if (v.check) {
    const r = await send(mk(v.model, "check"), { level: 1, i: 0 })
    console.log("\n" + (r ? `${r.kind.toUpperCase()}  http ${r.status}  code ${r.code || "-"}  ${r.ms}ms` : "not sent"))
    if (r && r.sample) console.log("  " + r.sample.replace(/\s+/g, " ").slice(0, 200))
    if (r && r.kind === "ok") console.log("\nThe sign-in is served on the Start Plan. Run the ramp next.")
    if (r && r.kind === "auth") console.log("\nThe token was refused: sign in to ZCode again, then re-run.")
    if (r && r.kind === "spent") console.log("\nThe plan itself is spent (1113/1005) — no free requests left on this account.")
    run.write(`# probe check\n\n${new Date().toISOString()}\n\n${JSON.stringify(meta, null, 2)}\n\n${JSON.stringify(r, null, 2)}\n`)
    return
  }

  // ---- the concurrency ramp ---------------------------------------------------
  if (v.mode === "ramp") {
    const levels = String(v.levels).split(",").map((n) => Number(n.trim())).filter((n) => n > 0 && n <= 16)
    const rounds = Math.max(1, Number(v.rounds) || 1)
    const gap = Math.max(0, Number(v.gap) || 0)
    const stagger = Math.max(0, Number(v.stagger) || 0)
    const stamp = Date.now().toString(36)

    for (const level of levels) {
      if (stopped) break
      const runs = []
      for (let roundNo = 1; roundNo <= rounds; roundNo++) {
        if (stopped) break
        const reqs = Array.from({ length: level }, (_, i) => mk(v.model, `probe-${stamp}-L${level}-R${roundNo}-${i}`))
        console.log(
          `\nlevel ${level}  round ${roundNo}/${rounds}  firing ${level} ` +
            (stagger ? `staggered by ${stagger}ms …` : "at once …"),
        )
        const results = (
          await Promise.all(
            reqs.map(async (q, i) => {
              if (stagger && i) await sleep(i * stagger)
              return send(q, { level, round: roundNo, i })
            }),
          )
        ).filter(Boolean)
        const counts = summarizeCounts(results)
        runs.push({ level, round: roundNo, results, counts })
        console.log(`  ${countsLine(counts)}  (${Math.max(...results.map((r) => r.ms), 0)}ms worst)`)
        if (stopped) break
        if (roundNo < rounds && gap) {
          console.log(`  resting ${Math.round(gap / 1000)}s …`)
          await sleep(gap)
        }
      }
      const merged = runs.flatMap((r) => r.results)
      allLevels.push({ level, results: merged, counts: summarizeCounts(merged) })
      if (runs.length && runs.every((r) => r.counts.ok === 0) && allLevels.length >= 2) {
        stopped = stopped || `level ${level} served nothing and neither did the level before it`
      }
      if (gap && level !== levels[levels.length - 1] && !stopped) await sleep(gap)
    }
  }

  // ---- model granularity ------------------------------------------------------
  else if (v.mode === "models") {
    const models = String(v.models).split(",").map((s) => s.trim()).filter(Boolean)
    const stamp = Date.now().toString(36)
    // A: three at once on one model. B: three at once, one each.
    for (const [tag, reqs] of [
      ["same-model", models.slice(0, 1).flatMap((m) => [0, 1, 2].map((i) => mk(m, `probe-${stamp}-A-${i}`)))],
      ["distinct-models", models.map((m, i) => mk(m, `probe-${stamp}-B-${i}`))],
    ]) {
      if (stopped) break
      console.log(`\n${tag}: firing ${reqs.length} at once …`)
      const results = (await Promise.all(reqs.map((q, i) => send(q, { mode: "models", tag, i, model: models[i] ?? models[0] })))).filter(Boolean)
      const counts = summarizeCounts(results)
      allLevels.push({ level: tag, results, counts })
      console.log(`  ${countsLine(counts)}`)
      if (!stopped) { console.log("  resting 45s …"); await sleep(45000) }
    }
  }

  // ---- the throughput wall (opt-in, can block the IP) -------------------------
  else if (v.mode === "rate") {
    if (!v["yes-risk"]) {
      bail(
        "refusing --mode rate without --yes-risk.\n\n" +
          "This mode raises the arrival rate until the edge firewall blocks the address:\n" +
          "the plugin's own logs show 3–7.5 minutes from a sustained cap to a 405 that\n" +
          "lasts 1–2 hours (index.mjs:2229). While it is on, this address gets nothing from\n" +
          "zcode.z.ai — including the app's own quota reads and check-in.",
      )
    }
    const rate = Math.max(1, Number(v.rate) || 12)
    const minutes = Math.max(1, Number(v.minutes) || 4)
    const interval = Math.round(60000 / rate)
    const until = Date.now() + minutes * 60000
    const stamp = Date.now().toString(36)
    console.log(`\nrate mode: ${rate}/min for ${minutes}min (${interval}ms apart). Any 405 stops the run.`)
    let i = 0
    while (Date.now() < until && !stopped && used < maxRequests) {
      const r = await send(mk(v.model, `probe-${stamp}-RATE-${i}`), { mode: "rate", i, rate })
      if (r) console.log(`  ${now()}  ${r.kind}  ${r.status}  ${r.ms}ms  ${r.code || ""}`)
      i++
      await sleep(interval)
    }
  } else {
    bail("unknown --mode " + v.mode)
  }

  // ---- the summary ------------------------------------------------------------
  const table = formatTable(allLevels)
  const dur = Math.round((Date.now() - t0) / 1000)
  const lines = []
  lines.push("# ZCode Start Plan probe")
  lines.push("")
  lines.push(`run ${new Date().toISOString()}  ·  ${used} requests  ·  ${dur}s`)
  lines.push("")
  lines.push("```")
  lines.push(table)
  lines.push("```")
  lines.push("")
  if (stopped) lines.push(`**Stopped:** ${stopped}.`)
  lines.push("")
  lines.push("## how to read it")
  lines.push("")
  lines.push("- `ok` at a level: that many requests were served at once.")
  lines.push("- `busy` = 3008/3009/3010, the in-flight cap. The last level with every request served is the account's real ceiling.")
  lines.push("- `spent` = 1113/1005, the plan is out of free requests. Nothing about concurrency.")
  lines.push("- `rate` = 429/1302/1303/3002, an arrival-rate limit rather than an in-flight one.")
  lines.push("- `risk` = 3012, the vendor's risk control. Stop for the day if this appears.")
  lines.push("- `edge` = 405 / a block page: the address is blocked, 1–2 hours. Stop here.")
  lines.push("")
  lines.push("## what was sent")
  lines.push("")
  lines.push("```json")
  lines.push(JSON.stringify({ model: v.model, prompt: v.prompt, stream: !!v.stream, maxTokens: 16, appVersion: level || "3.17.2" }, null, 2))
  lines.push("```")
  run.write(lines.join("\n") + "\n")

  console.log("\n" + table)
  if (stopped) console.log("\nstopped: " + stopped)
  console.log(`\n${used} requests in ${dur}s`)
  console.log("summary  " + run.summary)
  console.log("attempts " + run.jsonl)
}

main().catch((e) => bail("probe failed: " + (e && e.stack ? e.stack : e)))
