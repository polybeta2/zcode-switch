// creds.mjs — the sign-in a Start Plan request needs, read and never written.
//
// Where the token actually lives, as measured on ZCode 3.17 (not as guessed):
//
//   v2/config.json  provider["builtin:zai-start-plan"].options.apiKey
//       The Start Plan JWT, in plain text, beside the baseURL it belongs to
//       (https://zcode.z.ai/api/v1/zcode-plan/anthropic). This is the one to
//       read: the plugin also finds it there (index.mjs:1796-1800).
//
//   v2/credentials.json  "zcodejwttoken"
//       The same session token, but sealed: every value in that file reads
//       "enc:v2:os:<key id>:<…>" and its key is held by the OS, so it cannot
//       be read from here. zcode-switch's own zcrypto.rs knows only "enc:v1:"
//       (its own format), so this file is opaque to both. Kept as a fallback
//       for a ZCode that still writes it in the clear, and reported honestly
//       when it does not.
//
//   v2/telemetry-state.json  deviceMid
//       The device every dressed request names in metadata.user_id.
import { readFileSync, existsSync } from "node:fs"
import { homedir } from "node:os"
import { join } from "node:path"

const START_PROVIDER = "builtin:zai-start-plan"

// zcodeDir is the folder the app would use: ZCODE_DIR for a probe against a
// copy, else the platform's own.
export function zcodeDir() {
  if (process.env.ZCODE_DIR) return process.env.ZCODE_DIR
  return join(homedir(), ".zcode", "v2")
}

const readJson = (p) => {
  try {
    return JSON.parse(readFileSync(p, "utf8"))
  } catch {
    return null
  }
}

// jwtInfo reads a JWT's claims for its times and names only: a payload can
// carry an address or a name, so it never comes back whole. The Start Plan
// token carries iat and no exp — it does not expire on its own, and a spent
// plan is what stops it, not a clock.
export function jwtInfo(jwt) {
  if (typeof jwt !== "string" || !jwt.trim()) return { present: false }
  const parts = jwt.split(".")
  if (parts.length !== 3) return { present: true, readable: false }
  let p
  try {
    p = JSON.parse(Buffer.from(parts[1], "base64url").toString("utf8"))
  } catch {
    return { present: true, readable: false, opaque: true }
  }
  const exp = Number.isFinite(p.exp) ? p.exp * 1000 : null
  const iat = Number.isFinite(p.iat) ? p.iat * 1000 : null
  return {
    present: true,
    readable: true,
    user: typeof p.sub === "string" ? p.sub : typeof p.user_id === "string" ? p.user_id : "",
    tokenVersion: p.token_version ?? null,
    iat,
    iatAt: iat ? new Date(iat).toISOString() : null,
    exp,
    expAt: exp ? new Date(exp).toISOString() : null,
    expiresInSec: exp ? Math.round((exp - Date.now()) / 1000) : null,
    expired: exp ? Date.now() >= exp : false,
    claims: Object.keys(p).sort(),
  }
}

// readConfigToken is the primary source: ZCode's provider list names the Start
// Plan's baseURL and carries its JWT as that provider's apiKey.
function readConfigToken(dir) {
  const cfg = readJson(join(dir, "config.json"))
  const sp = cfg?.provider?.[START_PROVIDER]
  const jwt = typeof sp?.options?.apiKey === "string" ? sp.options.apiKey.trim() : ""
  if (!jwt) return null
  return { jwt, base: typeof sp?.options?.baseURL === "string" ? sp.options.baseURL : "", enabled: sp?.enabled !== false, source: "config.json " + START_PROVIDER }
}

// readSignIn is what one run needs: the token, where it came from, the device,
// and whether anything else was found and left unreadable.
export function readSignIn(dir = zcodeDir()) {
  const credPath = join(dir, "credentials.json")
  const cfgPath = join(dir, "config.json")
  if (!existsSync(credPath) && !existsSync(cfgPath)) {
    throw new Error(`no credentials.json or config.json in ${dir} — sign in to ZCode first (or point ZCODE_DIR at its data folder)`)
  }
  const creds = readJson(credPath) || {}
  const telemetry = readJson(join(dir, "telemetry-state.json")) || {}
  const device = typeof telemetry.deviceMid === "string" ? telemetry.deviceMid : ""

  // primary: the provider list's plaintext JWT
  let found = readConfigToken(dir)
  let sealed = false
  if (!found) {
    // fallback: credentials.json, only when it is not sealed
    const raw = typeof creds["zcodejwttoken"] === "string" ? creds["zcodejwttoken"] : ""
    if (raw.startsWith("enc:")) sealed = true
    else if (raw) found = { jwt: raw.replace(/^Bearer /, "").trim(), base: "", enabled: true, source: "credentials.json zcodejwttoken" }
  }

  const jwt = found?.jwt || ""
  return {
    dir,
    credPath,
    cfgPath,
    jwt,
    source: found?.source || (sealed ? "credentials.json zcodejwttoken (sealed, unreadable)" : "not found"),
    base: found?.base || "",
    enabled: found?.enabled !== false,
    device,
    signIn: jwtInfo(jwt),
    activeProvider: typeof creds["oauth:active_provider"] === "string" ? creds["oauth:active_provider"] : "",
    codingPlanKeys: Object.keys(creds).filter((k) => k.includes(":coding-plan:") && k.endsWith(":api-key")).length,
    models: readModels(dir),
  }
}

// readModels lists the model ids ZCode's own model list would offer, when the
// file that holds them is readable. Only used to print a default.
function readModels(dir = zcodeDir()) {
  const p = join(dir, "provider_config.json")
  const pc = readJson(p)
  const rules = pc?.config?.modelConfigRules
  if (!rules || typeof rules !== "object") return []
  const out = new Set()
  for (const v of Object.values(rules)) {
    for (const id of v?.models ?? v?.modelIds ?? []) if (typeof id === "string") out.add(id)
  }
  return [...out]
}

// describeSignIn is what a run prints and logs: no token, no address.
export function describeSignIn(s) {
  const j = s.signIn
  const when = !j.readable
    ? j.opaque
      ? "encrypted payload"
      : "unreadable claims"
    : j.expiresInSec == null
      ? "no expiry"
      : j.expired
        ? "EXPIRED"
        : `${Math.floor(j.expiresInSec / 60)}m left`
  const lines = [
    `zcode dir    ${s.dir}`,
    `start plan   ${j.present ? `token present (${when})` : "NO TOKEN"}  from ${s.source}`,
  ]
  if (s.base) lines.push(`base url     ${s.base}${s.enabled ? "" : "  (provider disabled)"}`)
  if (j.readable && j.user) lines.push(`account      ${j.user}`)
  lines.push(`device id    ${s.device ? s.device.slice(0, 8) + "…" : "MISSING"}`)
  if (s.models.length) lines.push(`models       ${s.models.slice(0, 6).join(", ")}${s.models.length > 6 ? " …" : ""}`)
  return lines.join("\n")
}
