// wire.mjs — one Start Plan request, shaped by the plugin's own code.
//
// The dressing is the part that decides whether zcode.z.ai serves a request at
// all: three cached system blocks with the vendor's own texts, a context-prefix
// turn with today's date, metadata.user_id naming the device, and a header set
// that says "ZCode's own desktop client". None of it is worth re-deriving here,
// so the probe imports the reference implementation and calls it. Every later
// Rust port can then be diffed against one source of truth instead of against
// a memory of it.
import { randomUUID } from "node:crypto"
import { pathToFileURL, fileURLToPath } from "node:url"
import { release } from "node:os"
import { existsSync } from "node:fs"
import { dirname, resolve } from "node:path"

// START_BASE is where a Start Plan's model requests go (plugin index.mjs:33).
export const START_BASE = "https://zcode.z.ai/api/v1/zcode-plan/anthropic"

// APP_VERSION is what X-ZCode-App-Version says. The plugin pins 3.17.2; the
// app probes for the latest before falling back, so pass --app-version when
// testing a newer one.
export const APP_VERSION = "3.17.2"

const OS_CATEGORY = { win32: "windows", darwin: "macos", linux: "linux" }

// defaultRefPath is the plugin checkout kept beside this repo. It is resolved
// from this file, not from the working directory, so the probe runs the same
// way from the repo root, from probe/, and from a shortcut.
export function defaultRefPath() {
  if (process.env.ZCODE_PLUGIN_REF) return process.env.ZCODE_PLUGIN_REF
  const here = dirname(fileURLToPath(import.meta.url)) // probe/lib
  const candidates = [
    resolve(here, "..", "..", "..", "_ref", "magpie-plugins", "packages", "zcode", "index.mjs"),
    resolve(process.cwd(), "..", "_ref", "magpie-plugins", "packages", "zcode", "index.mjs"),
    resolve(process.cwd(), "_ref", "magpie-plugins", "packages", "zcode", "index.mjs"),
  ]
  return candidates.find((p) => existsSync(p)) || candidates[0]
}

// loadReference imports _internal from the plugin: dress, PROMPT, sessionOf and
// queryOf are the four the wire needs. A missing piece fails loudly rather than
// falling back to a hand-copied approximation.
export async function loadReference(refPath = defaultRefPath()) {
  if (!existsSync(refPath)) {
    throw new Error(
      `plugin reference not found at ${refPath}\n` +
        `clone it with: git clone --depth 1 --filter=blob:none --sparse https://github.com/magpie-community/plugins _ref/magpie-plugins\n` +
        `                git -C _ref/magpie-plugins sparse-checkout set packages/zcode\n` +
        `or pass --ref <path to packages/zcode/index.mjs>`,
    )
  }
  const mod = await import(pathToFileURL(refPath).href)
  const I = mod && mod._internal
  for (const need of ["dress", "PROMPT", "sessionOf", "queryOf"]) {
    if (!I || typeof I[need] === "undefined") throw new Error(`plugin ${refPath} does not export _internal.${need}`)
  }
  return I
}

// startHeaders mirrors index.mjs:2496-2526 byte for byte, with the session and
// query ids the caller worked out.
export function startHeaders({ jwt, sessionId, queryId, appVersion = APP_VERSION }) {
  const h = new Headers()
  h.set("content-type", "application/json")
  h.set("Authorization", "Bearer " + jwt)
  h.set("anthropic-version", "2023-06-01")
  h.set("HTTP-Referer", "https://zcode.z.ai")
  h.set("User-Agent", `ZCode/${appVersion} ai-sdk/anthropic/3.0.81`)
  h.set("X-ZCode-App-Version", appVersion)
  h.set("X-Title", "Z Code@electron")
  h.set("X-Release-Channel", "production")
  const o = Intl.DateTimeFormat().resolvedOptions()
  h.set("X-Client-Language", o.locale || "unknown")
  h.set("X-Client-Timezone", o.timeZone || "unknown")
  h.set("X-ZCode-Agent", "glm")
  h.set("X-Platform", process.platform)
  h.set("X-Os-Category", OS_CATEGORY[process.platform] || process.platform)
  h.set("X-Os-Version", release())
  h.set("x-request-id", randomUUID())
  h.set("x-zcode-session-type", "main")
  h.set("x-zcode-trace-id", randomUUID())
  h.set("x-zcode-query-source", "main_turn")
  h.set("x-session-id", sessionId)
  // the plugin sends x-query-id only when the body reads as a Messages request
  if (queryId) h.set("x-query-id", queryId)
  return h
}

// messagesBody is the smallest body that is still a Messages request: one short
// answer, so a slot is held for a moment rather than for a minute.
export function messagesBody({ model, text, maxTokens = 16, stream = false }) {
  return {
    model,
    max_tokens: maxTokens,
    stream,
    system: "You are a coding assistant.",
    messages: [{ role: "user", content: text }],
  }
}

// buildStartPlanRequest is what a probe fires: the dressed body, the ids, and
// the headers, with nothing left for the caller to remember.
export function buildStartPlanRequest({ ref, jwt, device, model, prompt, base = START_BASE, maxTokens = 16, stream = false, sessionSeed, appVersion = APP_VERSION }) {
  const sessionId = ref.sessionOf(sessionSeed ?? "", device)
  const bare = JSON.stringify(messagesBody({ model, text: prompt, maxTokens, stream }))
  const body = ref.dress(bare, "zai", device, {}, sessionId)
  // queryOf counts the user's turns in the body as the agent sent it, before dressing
  const queryId = ref.queryOf(bare, sessionId)
  const headers = startHeaders({ jwt, sessionId, queryId, appVersion })
  // the base comes from config.json when it is there, so a moved endpoint is
  // followed rather than assumed
  const root = String(base || START_BASE).replace(/\/+$/, "")
  return { url: root + "/v1/messages", headers, body, sessionId, queryId, bare }
}

// redacted is a request as it can be printed: the token never is.
export function redacted(req) {
  const headers = {}
  for (const [k, v] of req.headers) headers[k] = /^(authorization|x-api-key)$/i.test(k) ? "Bearer ***" : v
  return { url: req.url, headers, body: JSON.parse(req.body) }
}
