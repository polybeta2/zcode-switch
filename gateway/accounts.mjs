// accounts.mjs — the accounts this app already holds, as things a gateway can
// send with.
//
// The app keeps one file per account under ~/.zcode-switch/accounts, and each
// file carries the account's ZCode config.json as it was when the account was
// saved: that is where the plan's key and its baseURL live
// (provider["builtin:zai-start-plan"].options.apiKey), beside the device the
// account pretends to be (virtual_device_mid). Nothing here writes, and nothing
// here decrypts: credentials.json's values are sealed by the OS and are left
// alone — the config snapshot is in the clear, which is the one we read.
import { readFileSync, readdirSync, existsSync } from "node:fs"
import { homedir } from "node:os"
import { join } from "node:path"

export const START_PROVIDER = "builtin:zai-start-plan"

// storeDir is where this app keeps its accounts (store.rs: store_dir).
export function storeDir() {
  if (process.env.ZCODE_SWITCH_STORE_DIR) return process.env.ZCODE_SWITCH_STORE_DIR
  return join(homedir(), ".zcode-switch")
}

// zcodeDir is where ZCode itself keeps its sign-in: the live account, for the
// run where the plan was activated but not yet saved into an account file.
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

// PLANS names what each of ZCode's providers is, in the order a request should
// prefer them: the Start Plan is the one whose traffic the vendor watches
// hardest, the coding plan is the ordinary subscription, an API key is paid.
const PLANS = {
  "builtin:zai-start-plan": { plan: "start", site: "zai", dress: true, rank: 0 },
  "builtin:bigmodel-start-plan": { plan: "start", site: "bigmodel", dress: true, rank: 0 },
  "builtin:bigmodel-coding-plan": { plan: "coding", site: "bigmodel", dress: false, rank: 1 },
  "builtin:zai-coding-plan": { plan: "coding", site: "zai", dress: false, rank: 1 },
  "builtin:bigmodel": { plan: "api", site: "bigmodel", dress: false, rank: 2 },
  "builtin:zai": { plan: "api", site: "zai", dress: false, rank: 2 },
}

// usableKey is a key this gateway can send: not empty, and not one of the
// sealed values credentials.json holds (enc:v1: is this app's own, enc:v2: the
// OS's) — either would only earn an empty-bodied 401.
const usableKey = (k) => typeof k === "string" && k.trim().length > 20 && !/^enc:v\d/.test(k)

// targetsFromConfig reads one ZCode config.json the way the app does: every
// provider entry with a key in it is a target, and the entry's own baseURL is
// where it goes.
export function targetsFromConfig(config, { accountId, accountName, device }) {
  const providers = config && typeof config === "object" ? config.provider : null
  if (!providers || typeof providers !== "object") return []
  const out = []
  for (const [providerId, entry] of Object.entries(providers)) {
    const known = PLANS[providerId]
    const options = entry && typeof entry === "object" ? entry.options : null
    if (!known || !options || typeof options !== "object") continue
    const key = options.apiKey
    if (!usableKey(key)) continue
    const base = typeof options.baseURL === "string" && options.baseURL.trim() ? options.baseURL.trim() : ""
    // the entry declares the models this plan serves, in the spelling the
    // vendor wants: a request naming another model is refused with 3006
    const models = Object.keys(entry.models && typeof entry.models === "object" ? entry.models : {}).filter((m) => typeof m === "string" && m.trim())
    out.push({
      accountId,
      accountName,
      providerId,
      plan: known.plan,
      site: known.site,
      dress: known.dress,
      rank: known.rank,
      enabled: entry.enabled !== false,
      key: key.trim(),
      base,
      models,
      device: device || "",
      label: accountName + " · " + known.plan + (known.plan === "start" ? "" : " (" + providerId.replace("builtin:", "") + ")"),
    })
  }
  return out
}

// readAccounts is every account file this app holds.
export function readAccounts() {
  const dir = join(storeDir(), "accounts")
  if (!existsSync(dir)) return []
  const out = []
  for (const name of readdirSync(dir)) {
    if (!name.endsWith(".json")) continue
    const a = readJson(join(dir, name))
    if (!a || typeof a !== "object" || !a.id) continue
    out.push(a)
  }
  return out.sort((x, y) => String(y.updated_at || "").localeCompare(String(x.updated_at || "")))
}

// readLive is ZCode's own sign-in, the one the app switches to.
export function readLive() {
  const cfg = readJson(join(zcodeDir(), "config.json"))
  if (!cfg) return null
  const tele = readJson(join(zcodeDir(), "telemetry-state.json"))
  return {
    id: "live",
    name: "本机 ZCode",
    config: cfg,
    virtual_device_mid: tele && typeof tele.deviceMid === "string" ? tele.deviceMid : "",
    updated_at: "",
  }
}

// loadTargets is every target these options allow, best first, one per key.
//
// Preferring the Start Plan is a guess about intent, not a fact about billing:
// an operator who wants the subscription instead says so (plans), and one who
// wants a single account says that too (accounts: ids or names).
export function loadTargets({ includeLive = true, plans = null, accounts = null } = {}) {
  const all = []
  const accountList = readAccounts()
  for (const a of accountList) {
    if (accounts && !accounts.some((x) => x === a.id || x === a.name)) continue
    all.push(...targetsFromConfig(a.config, { accountId: a.id, accountName: a.name, device: a.virtual_device_mid || "" }))
  }
  if (includeLive) {
    const live = readLive()
    if (live) {
      const extra = targetsFromConfig(live.config, { accountId: "live", accountName: live.name, device: live.virtual_device_mid })
      // a key already known from an account file is the same plan and the same
      // device: sending it twice would only double the load on one budget
      const seen = new Set(all.map((t) => t.key))
      for (const t of extra) if (!seen.has(t.key)) all.push(t)
    }
  }
  const wanted = plans && plans.length ? new Set(plans) : null
  const kept = all.filter((t) => !wanted || wanted.has(t.plan))
  kept.sort((x, y) => x.rank - y.rank || Number(y.enabled) - Number(x.enabled) || x.accountName.localeCompare(y.accountName))
  return kept
}

// readModels is the model list ZCode's own provider config knows, for
// /v1/models. A client that sends one of these gets the model it asked for; the
// list is the vendor's, not ours, so it is read rather than guessed.
// modelsOf is every model some target can serve, in the plans' own spelling.
// It is what GET /v1/models reports, because a name the plans do not declare
// cannot be served at all — the vendor refuses it with code 3006.
export function modelsOf(targets, fallback = []) {
  const seen = []
  for (const t of targets || []) {
    for (const m of Array.isArray(t.models) ? t.models : []) if (m && !seen.includes(m)) seen.push(m)
  }
  return seen.length ? seen : fallback
}

export function readModels() {
  const cfg = readJson(join(zcodeDir(), "provider_config.json"))
  const rules = cfg?.config?.modelConfigRules
  const ids = []
  const add = (v) => {
    if (typeof v === "string" && v.trim() && !ids.includes(v.trim())) ids.push(v.trim())
  }
  for (const r of Array.isArray(rules?.providerModelRules) ? rules.providerModelRules : []) add(r?.modelId)
  for (const r of Array.isArray(rules?.manualProviderModelRules) ? rules.manualProviderModelRules : []) add(r?.modelId)
  if (!ids.length) for (const m of FALLBACK_MODELS) add(m)
  return ids
}

// FALLBACK_MODELS is only for a machine with no provider_config.json yet.
export const FALLBACK_MODELS = ["glm-4.6", "glm-4.5", "glm-4.5-air"]
