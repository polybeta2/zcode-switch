// vendor.mjs — one Messages request, sent as ZCode sends it.
//
// node:https rather than fetch, because what comes back has to be readable as
// it arrives: a plan's answer is a server-sent event stream and a proxy that
// waits for the whole body turns a live answer into a pause. Nothing here
// retries or interprets: a refusal is the pool's business, not this file's.
import { request as httpRequest } from "node:http"
import { request as httpsRequest } from "node:https"
import { release as osRelease } from "node:os"
import { arch, platform } from "node:process"
import { APP_VERSION, startHeaders } from "./dress.mjs"

// endpointOf is where a Messages request goes: the base the account's own
// config names, and /v1/messages under it.
export function endpointOf(target) {
  return String(target.base || "").replace(/\/+$/, "") + "/v1/messages"
}

// appHeaders is what ZCode puts on a coding plan's or an API key's request:
// the same fingerprint the app itself sends when it asks for a quota
// (src-tauri/src/quota.rs: zai_headers_with_version). The Start Plan wants
// more than this, so it gets its own set (dress.mjs: startHeaders).
function appHeaders(target, token) {
  let lang = "unknown"
  let tz = "unknown"
  try {
    const o = Intl.DateTimeFormat().resolvedOptions()
    lang = o.locale || lang
    tz = o.timeZone || tz
  } catch {}
  const h = {
    "content-type": "application/json",
    Authorization: "Bearer " + token,
    "anthropic-version": "2023-06-01",
    "User-Agent": "ZCode/" + APP_VERSION,
    "HTTP-Referer": "https://zcode.z.ai",
    "X-Title": "Z Code@electron",
    "X-ZCode-App-Version": APP_VERSION,
    "X-Platform": platform + "-" + arch,
    "X-Release-Channel": "production",
    "X-Client-Language": lang,
    "X-Client-Timezone": tz,
    "X-Os-Category": platform === "darwin" ? "macos" : platform === "win32" ? "windows" : "linux",
    "X-Os-Version": osRelease(),
    "x-request-id": crypto.randomUUID(),
  }
  if (target.device) h["X-Device-Mid"] = target.device
  return h
}

// headersFor is the header set a target's plan expects.
export function headersFor(target, { sessionId = "", queryId = "" } = {}) {
  if (target.dress) return startHeaders({ jwt: target.key, sessionId, queryId })
  return appHeaders(target, target.key)
}

// send posts the body and resolves as soon as the response's head arrives, so
// a streamed answer can be forwarded while it is still being written. The
// caller owns the returned stream and must consume or destroy it.
export function send({ target, body, headers, timeoutMs = 600_000, signal } = {}) {
  const url = new URL(endpointOf(target))
  const payload = Buffer.from(JSON.stringify(body), "utf8")
  // an http base is a local stand-in for the vendor (the tests', and anyone
  // measuring a mock); everything the plans speak is https
  const send_ = url.protocol === "http:" ? httpRequest : httpsRequest
  const port = url.port || (url.protocol === "http:" ? 80 : 443)
  return new Promise((resolve, reject) => {
    const req = send_(
      {
        method: "POST",
        hostname: url.hostname,
        port,
        path: url.pathname + url.search,
        headers: { ...headers, "content-length": payload.length },
      },
      (res) => resolve({ status: res.statusCode || 0, headers: res.headers, stream: res }),
    )
    // a plan answers slowly when it is thinking; the idle timer is what catches
    // a socket that is open but dead, and the deadline what catches a stream
    // that never ends
    req.setTimeout(timeoutMs, () => req.destroy(new Error("the vendor did not answer within " + Math.round(timeoutMs / 1000) + "s")))
    req.on("error", reject)
    if (signal) signal.addEventListener("abort", () => req.destroy(new Error("cancelled")), { once: true })
    req.end(payload)
  })
}

// collect reads a whole response, bounded so a broken answer cannot grow
// without end.
export async function collect(stream, limit = 32 * 1024 * 1024) {
  const chunks = []
  let size = 0
  for await (const chunk of stream) {
    size += chunk.length
    if (size > limit) {
      stream.destroy()
      throw new Error("the vendor's answer passed " + Math.round(limit / 1024 / 1024) + " MB")
    }
    chunks.push(chunk)
  }
  return Buffer.concat(chunks).toString("utf8")
}
