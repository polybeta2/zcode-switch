// dress.mjs — a Messages request as ZCode's own client sends it, and the
// headers that go with it.
//
// Ported from the magpie zcode plugin (packages/zcode/index.mjs: dress 2793,
// startHeaders 2496, queryOf 2535, sessionOf 2883, userIdOf 2854, environment
// 2759) so this repo can serve the Start Plan without a second checkout. The
// texts in prompt.mjs are ZCode 3.17.2's byte for byte, and dress.test.mjs
// checks the hashes the plugin records for them — a reworded paragraph here
// fails a test instead of a request.
//
// Two rules the vendor enforces, both learned upstream the hard way:
//   - exactly three system blocks, the first two byte for byte ZCode's: a
//     fourth is turned away with 405/3012 (plugins#343);
//   - metadata.user_id names the sign-in's device, or the plan's prompt cache
//     is mostly missed and every request is paid for again (plugins#387).
import { createHash, randomUUID } from "node:crypto"
import { release } from "node:os"
import { arch, platform as sysPlatform } from "node:process"
import PROMPT from "./prompt.mjs"

// APP_VERSION is what X-ZCode-App-Version says. The vendor moved it to 3.17.2
// with these prompt texts; a ZCode that says another version is a ZCode the
// risk control may read differently, so it stays a constant here.
export const APP_VERSION = "3.17.2"
export const UA = "ZCode/" + APP_VERSION
export const ZCODE = "https://zcode.z.ai"
export const START_BASE = ZCODE + "/api/v1/zcode-plan/anthropic"

// uuidOf is a version 4 shaped UUID made from a seed: the same seed gives the
// same UUID every time, which is how a session id survives a restart.
export const uuidOf = (seed) => {
  const h = createHash("sha256").update(seed).digest("hex")
  return h.slice(0, 8) + "-" + h.slice(8, 12) + "-4" + h.slice(13, 16) + "-" + ((parseInt(h[16], 16) & 3) | 8).toString(16) + h.slice(17, 20) + "-" + h.slice(20, 32)
}

// keyDevice is a device id for an account known only by its key: the same key
// gives the same id on every start, as a sign-in's saved device does.
export const keyDevice = (key) => uuidOf("zcode-device\0" + key)

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i

// sessionOf is the x-session-id a request goes with: a bare UUID, the same on
// every turn of one conversation, made from the agent's session id when it
// sends one and from the device when it does not. The vendor keeps a prompt
// cache per session and seldom serves a request that names none (plugins#387).
export function sessionOf(id, device) {
  const own = typeof id === "string" ? id.trim() : ""
  if (UUID.test(own)) return own.toLowerCase()
  return uuidOf(own ? "zcode-session\0" + own : "zcode-session-device\0" + (device ?? ""))
}

// queryOf is the request's x-query-id: the user's query, a UUID from the
// session and the number of user turns so far, the same for every request of
// one turn and a new one for the next thing the user says. A body that does
// not read as a Messages request gives none, and none is sent.
export function queryOf(text, session) {
  let b
  try {
    b = JSON.parse(text)
  } catch {
    return ""
  }
  if (!Array.isArray(b?.messages)) return ""
  let n = 0
  for (const m of b.messages) {
    if (m?.role !== "user") continue
    const c = m.content
    if (typeof c === "string" ? c.trim() : Array.isArray(c) && c.some((x) => x && x.type !== "tool_result")) n++
  }
  return n ? uuidOf("zcode-query\0" + session + "\0" + n) : ""
}

// today is the local date, YYYY-MM-DD.
function today(now = new Date()) {
  const p = (n) => String(n).padStart(2, "0")
  return now.getFullYear() + "-" + p(now.getMonth() + 1) + "-" + p(now.getDate())
}

const osVersion = () => sysPlatform + " " + release() + " " + arch
const osCategory = () => (sysPlatform === "darwin" ? "macos" : sysPlatform === "win32" ? "windows" : "linux")

// environment is ZCode's environment section. The working directory and the
// git line are only said when the caller knows them: a wrong path is worse
// than a missing one (plugins#86).
function environment(place = {}) {
  const e = PROMPT.environment
  const lines = [e.heading, e.invokedLine]
  if (place.dir) {
    lines.push(" - " + e.cwdLabel + ": " + place.dir)
    if (place.git) lines.push(" - " + e.gitLabel + ": " + (place.git === "yes" ? e.gitYes : e.gitNo))
  }
  lines.push(" - " + e.platformLabel + ": " + sysPlatform, " - " + e.osVersionLabel + ": " + osVersion())
  lines.push(" - " + e.untrustedNote)
  return lines.join("\n")
}

// contextPrefix is the user turn ZCode puts before the agent's: the day in a
// system-reminder, the tags hugging the text.
function contextPrefix() {
  const c = PROMPT.context
  return { role: "user", content: [{ type: "text", text: "<system-reminder>" + c.currentDateLine.replace("{date}", today()) + "</system-reminder>" }] }
}

// userIdOf is metadata.user_id as ZCode puts it on every request: the
// machine's device id, no account uuid, and the session it belongs to.
export const userIdOf = (device, session = "") =>
  JSON.stringify({ device_id: device, account_uuid: "", session_id: session })

// dress is a Messages request body as ZCode sends it: ZCode's three cached
// system blocks and no more, the agent's own system text appended to the third;
// the context prefix as the first turn; no cache mark in the turns but on the
// last; none on the tools; metadata.user_id naming the sign-in's device.
// Anything that does not read as a Messages request goes as it came.
export function dress(text, site, device, place = {}, session = "") {
  if (typeof text !== "string" || !text.includes("messages")) return text
  let b
  try {
    b = JSON.parse(text)
  } catch {
    return text
  }
  if (!b || typeof b !== "object" || !Array.isArray(b.messages)) return text
  const cached = (t) => ({ type: "text", text: t, cache_control: { type: "ephemeral" } })
  const provider = site === "bigmodel" ? "bigmodel-api" : "zai-api"
  const model = typeof b.model === "string" ? b.model : ""
  const own = []
  if (typeof b.system === "string") {
    if (b.system.trim()) own.push(b.system)
  } else if (Array.isArray(b.system)) {
    for (const x of b.system) if (x?.type === "text" && typeof x.text === "string" && x.text.trim()) own.push(x.text)
  }
  const skill = Array.isArray(b.tools) && b.tools.some((t) => t && typeof t === "object" && t.name === "Skill")
  const dynamic = [
    PROMPT.dynamicBehavior,
    ...(skill ? [PROMPT.sessionGuidance] : []),
    environment(place),
    ...(model ? [PROMPT.poweredBy.replace("{provider}", provider).replace("{model}", model)] : []),
    PROMPT.contextManagement,
    ...own,
  ]
  b.system = [cached(PROMPT.cliPrefix), cached(PROMPT.stableSections.join("\n\n")), cached("\n\n" + dynamic.join("\n\n"))]
  b.messages = [contextPrefix(), ...b.messages]
  let last = null
  for (const m of b.messages) {
    if (!m || m.role === "system") continue
    last = m
    if (Array.isArray(m.content)) for (const x of m.content) if (x && typeof x === "object") delete x.cache_control
  }
  if (last && typeof last.content === "string") last.content = [cached(last.content)]
  else if (last && Array.isArray(last.content) && last.content.length) {
    const x = last.content[last.content.length - 1]
    if (x && typeof x === "object" && !x.cache_control) x.cache_control = { type: "ephemeral" }
  }
  if (Array.isArray(b.tools)) for (const t of b.tools) if (t && typeof t === "object") delete t.cache_control
  const meta = b.metadata && typeof b.metadata === "object" ? b.metadata : {}
  b.metadata = { ...meta, user_id: userIdOf(device, session) }
  return JSON.stringify(b)
}

// dressObject is dress for a caller that already holds the body as an object.
export function dressObject(body, { site = "zai", device = "", place = {}, session = "" } = {}) {
  return JSON.parse(dress(JSON.stringify(body), site, device, place, session))
}

// marked is a coding-plan body with metadata.user_id as ZCode sends it, when
// the agent sent none of its own. The coding plans' endpoints do not need the
// dressing; they do need the device, or the cache lands elsewhere.
export function marked(text, device, session) {
  if (typeof text !== "string" || !device || !text.includes("messages")) return text
  let b
  try {
    b = JSON.parse(text)
  } catch {
    return text
  }
  if (!b || typeof b !== "object" || Array.isArray(b) || !Array.isArray(b.messages)) return text
  const meta = b.metadata && typeof b.metadata === "object" && !Array.isArray(b.metadata) ? b.metadata : {}
  if (typeof meta.user_id === "string" && meta.user_id.trim()) return text
  b.metadata = { ...meta, user_id: userIdOf(device, session) }
  return JSON.stringify(b)
}

// startHeaders is a Start Plan request's headers, its key the bearer: ZCode's
// own and the body's content-type, and nothing else. An agent's headers never
// go on — anthropic-beta, x-app, x-stainless-* and the rest are the agent's,
// and ZCode sends none of them on this endpoint (it has no tool-search beta on
// the Start Plan at all).
export function startHeaders({ jwt, sessionId, queryId, appVersion = APP_VERSION, contentType = "application/json" }) {
  const h = {}
  h["content-type"] = contentType || "application/json"
  h["Authorization"] = "Bearer " + jwt
  h["anthropic-version"] = "2023-06-01"
  h["HTTP-Referer"] = ZCODE
  h["User-Agent"] = "ZCode/" + appVersion + " ai-sdk/anthropic/3.0.81"
  h["X-ZCode-App-Version"] = appVersion
  // the desktop app's agent names itself electron; "cli" is the zcode command
  // line's alone, and it sends no X-Device-Mid on the app's calls
  h["X-Title"] = "Z Code@electron"
  h["X-Release-Channel"] = "production"
  let o = {}
  try {
    o = Intl.DateTimeFormat().resolvedOptions()
  } catch {}
  h["X-Client-Language"] = o.locale || "unknown"
  h["X-Client-Timezone"] = o.timeZone || "unknown"
  h["X-ZCode-Agent"] = "glm"
  h["X-Platform"] = sysPlatform + "-" + arch
  h["X-Os-Category"] = osCategory()
  h["X-Os-Version"] = release()
  h["x-request-id"] = randomUUID()
  h["x-zcode-session-type"] = "main"
  h["x-zcode-trace-id"] = randomUUID()
  h["x-zcode-query-source"] = "main_turn"
  if (sessionId) h["x-session-id"] = sessionId
  // x-query-id only when the body reads as a Messages request
  if (queryId) h["x-query-id"] = queryId
  return h
}
