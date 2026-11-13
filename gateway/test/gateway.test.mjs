// gateway.test.mjs — what the test gateway claims, checked without the vendor.
//
//   node --test gateway/test/
//
// The wire is the one thing these tests cannot prove: whether the vendor
// accepts a dressed request is a fact about the vendor, measured by ../probe.
// Everything below it — the translation, the refusals, the queue, the streamed
// answer — is ours, and is checked against a local stand-in for the plan.
import test from "node:test"
import assert from "node:assert/strict"
import { createServer } from "node:http"
import { once } from "node:events"
import { Pool } from "../pool.mjs"
import { createGateway, kindOf } from "../server.mjs"
import { StreamTranslator, finishReason, toAnthropic, toOpenAI } from "../translate.mjs"
import { jwtSubject, loadTargets } from "../accounts.mjs"
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"

// waitFor listens on an ephemeral port and resolves the base URL and the
// server, so tests never fight over 8787.
async function listening(server, host = "127.0.0.1") {
  server.listen(0, host)
  await once(server, "listening")
  return "http://" + host + ":" + server.address().port
}

const target = (over = {}) => ({
  accountId: "a1",
  accountName: "acct",
  providerId: "builtin:zai-start-plan",
  plan: "start",
  site: "zai",
  dress: true,
  rank: 0,
  key: "jwt." + "x".repeat(40),
  base: "",
  device: "device-1",
  label: "acct · start",
  ...over,
})

test("toAnthropic maps an OpenAI body onto Messages", () => {
  const req = toAnthropic(
    {
      model: "glm-4.6",
      messages: [
        { role: "system", content: "be brief" },
        { role: "user", content: "hello" },
        { role: "assistant", content: "", tool_calls: [{ id: "call_1", type: "function", function: { name: "read", arguments: "{\"p\":\"a\"}" } }] },
        { role: "tool", tool_call_id: "call_1", content: "file body" },
        { role: "user", content: "and then?" },
      ],
      tools: [{ type: "function", function: { name: "read", description: "read a file", parameters: { type: "object" } } }],
      temperature: 1.7,
      max_tokens: 128,
      stop: "END",
    },
    { defaultMaxTokens: 4096 },
  )
  assert.equal(req.model, "glm-4.6")
  assert.equal(req.system, "be brief")
  assert.equal(req.temperature, 1)
  assert.equal(req.max_tokens, 128)
  assert.deepEqual(req.stop_sequences, ["END"])
  assert.equal(req.tools[0].name, "read")
  assert.equal(req.tools[0].input_schema.type, "object")
  // the tool result and the next question share one user turn, because
  // Messages does not have a tool role
  const last = req.messages[req.messages.length - 1]
  assert.equal(last.role, "user")
  assert.equal(last.content[0].type, "tool_result")
  assert.equal(last.content[0].tool_use_id, "call_1")
  assert.equal(last.content[1].type, "text")
  assert.equal(last.content[1].text, "and then?")
  const assistant = req.messages.find((m) => m.role === "assistant")
  assert.equal(assistant.content[0].type, "tool_use")
  assert.deepEqual(assistant.content[0].input, { p: "a" })
})

test("toAnthropic refuses what it cannot honour", () => {
  assert.throws(() => toAnthropic({ model: "glm-4.6", messages: [{ role: "user", content: "hi" }], n: 2 }), /n > 1/)
  assert.throws(() => toAnthropic({ model: "glm-4.6", messages: [] }), /messages must not be empty/)
  const req = toAnthropic({ model: "glm-4.6", messages: [{ role: "user", content: "hi" }] }, { defaultMaxTokens: 777 })
  assert.equal(req.max_tokens, 777)
  assert.equal(req.model, "glm-4.6")
})

test("toOpenAI turns a Messages answer into a completion", () => {
  const out = toOpenAI(
    {
      id: "msg_1",
      model: "glm-4.6",
      stop_reason: "tool_use",
      content: [
        { type: "text", text: "let me look" },
        { type: "thinking", thinking: "hmm" },
        { type: "tool_use", id: "toolu_1", name: "read", input: { p: "a" } },
      ],
      usage: { input_tokens: 10, output_tokens: 5, cache_read_input_tokens: 90, cache_creation_input_tokens: 0 },
    },
    { model: "glm-4.6" },
  )
  const msg = out.choices[0].message
  assert.equal(out.object, "chat.completion")
  assert.equal(msg.content, "let me look")
  assert.equal(msg.reasoning_content, "hmm")
  assert.equal(out.choices[0].finish_reason, "tool_calls")
  assert.equal(finishReason("end_turn"), "stop")
  assert.equal(finishReason("max_tokens"), "length")
  assert.equal(msg.tool_calls[0].function.name, "read")
  assert.equal(msg.tool_calls[0].function.arguments, "{\"p\":\"a\"}")
  // the client pays for the cache reads whether or not it can see them
  assert.equal(out.usage.prompt_tokens, 100)
  assert.equal(out.usage.completion_tokens, 5)
  assert.equal(out.usage.prompt_tokens_details.cached_tokens, 90)
})

test("the stream translator emits OpenAI chunks, usage, then the sentinel", () => {
  const tr = new StreamTranslator({ model: "glm-4.6" })
  const events = [
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_9\",\"model\":\"glm-4.6\",\"usage\":{\"input_tokens\":7}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
  ]
  // feed it in ragged pieces: a proxy sees bytes, not events
  let out = ""
  for (const e of events) {
    const half = Math.floor(e.length / 2)
    out += tr.feed(e.slice(0, half))
    out += tr.feed(e.slice(half))
  }
  out += tr.finish()
  const lines = out.split("\n\n").filter(Boolean).map((l) => l.replace(/^data: /, ""))
  const objects = lines.filter((l) => l !== "[DONE]").map((l) => JSON.parse(l))
  const text = objects.map((o) => o.choices?.[0]?.delta?.content || "").join("")
  assert.equal(text, "Hello")
  assert.equal(objects[0].id, "msg_9")
  const last = objects[objects.length - 1]
  assert.equal(last.choices[0].finish_reason, "stop")
  assert.equal(last.usage.prompt_tokens, 7)
  assert.equal(last.usage.completion_tokens, 2)
  assert.equal(lines[lines.length - 1], "[DONE]")
})

test("kindOf reads the vendor's refusals as claims a client can act on", () => {
  assert.equal(kindOf(200, 0), "ok")
  assert.equal(kindOf(200, 1005), "spent")
  assert.equal(kindOf(200, 3008), "busy")
  assert.equal(kindOf(200, 3010), "busy")
  assert.equal(kindOf(200, 3012), "risk")
  assert.equal(kindOf(429, 0), "rate")
  assert.equal(kindOf(200, 1302), "rate")
  assert.equal(kindOf(405, 0), "edge")
  assert.equal(kindOf(401, 0), "auth")
  assert.equal(kindOf(500, 0), "other")
})

test("the pool gives an account its slots, then refuses rather than piling on", async () => {
  const pool = new Pool([target()], { inflight: 2, rpm: 0, queueMs: 150 })
  const a = await pool.acquire()
  const b = await pool.acquire()
  assert.equal(a.target.inflight, 2)
  await assert.rejects(pool.acquire(), (e) => e.kind === "busy")
  a.release()
  a.release() // idempotent: a double release must not free someone else's slot
  assert.equal(a.target.inflight, 1)
  b.release()
  assert.equal(a.target.inflight, 0)
})

test("a busy refusal narrows an account, and five of them make it rest", () => {
  const pool = new Pool([target()], { inflight: 3, rpm: 0 })
  const t = pool.targets[0]
  t.inflight = 2
  pool.note(t, { status: 200, code: 3008 })
  assert.equal(pool.limitOf(t), 2)
  t.inflight = 0
  for (let i = 0; i < 5; i++) pool.note(t, { status: 200, code: 3008 })
  assert.ok(t.holdUntil > Date.now())
  assert.match(t.holdWhy, /3008/)
})

test("an edge refusal rests the whole address, not one account", async () => {
  const pool = new Pool([target(), target({ accountId: "a2", label: "acct2 · start" })], {})
  pool.note(pool.targets[0], { status: 405, text: "blocked due to abuse" })
  assert.ok(pool.edgeFor() > 0)
  assert.equal(pool.pick(), null)
  await assert.rejects(pool.acquire(), (e) => e.kind === "edge")
})

// fakePlan stands in for the vendor: it answers the way a plan does, with the
// refusals and the event stream a real one uses.
async function fakePlan(behaviour) {
  const seen = []
  const server = createServer(async (req, res) => {
    const chunks = []
    for await (const c of req) chunks.push(c)
    const body = JSON.parse(Buffer.concat(chunks).toString("utf8") || "{}")
    seen.push({ headers: req.headers, body })
    const answer = behaviour(body, seen.length)
    if (answer.stream) {
      res.writeHead(200, { "content-type": "text/event-stream" })
      for (const e of answer.events) res.write(e)
      res.end()
      return
    }
    const text = JSON.stringify(answer.json)
    res.writeHead(answer.status || 200, { "content-type": "application/json", "content-length": Buffer.byteLength(text) })
    res.end(text)
  })
  const base = await listening(server)
  return { base, seen, server }
}

const textEvents = (said) => [
  "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"glm-4.6\",\"usage\":{\"input_tokens\":3}}}\n\n",
  "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
  "event: content_block_delta\ndata: " + JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: said } }) + "\n\n",
  "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
  "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
]

test("the gateway answers an OpenAI request with an OpenAI stream", async (t) => {
  const plan = await fakePlan(() => ({ stream: true, events: textEvents("Hello") }))
  const pool = new Pool([target({ base: plan.base })], { rpm: 0 })
  const gateway = createGateway({ pool, token: "tok", log: () => {}, version: "test" })
  const base = await listening(gateway)
  t.after(() => {
    gateway.close()
    plan.server.close()
  })

  const res = await fetch(base + "/v1/chat/completions", {
    method: "POST",
    headers: { authorization: "Bearer tok", "content-type": "application/json" },
    body: JSON.stringify({ model: "glm-4.6", stream: true, messages: [{ role: "user", content: "hi" }] }),
  })
  assert.equal(res.status, 200)
  const text = await res.text()
  assert.match(text, /"content":"Hello"/)
  assert.match(text, /data: \[DONE\]/)

  // the plan got a dressed request: three system blocks, and the device the
  // account pretends to be, which is what the vendor checks
  const body = plan.seen[0].body
  assert.equal(body.system.length, 3)
  // the plan carries no X-Device-Mid (the app's calls do not); the device is
  // named in the body, which is what lets the vendor find the prompt cache
  const named = JSON.parse(body.metadata.user_id)
  assert.equal(named.device_id, "device-1")
  assert.equal(named.session_id, plan.seen[0].headers["x-session-id"])
  assert.match(plan.seen[0].headers["user-agent"], /^ZCode\/3\.17\.2 ai-sdk\/anthropic\//)
  assert.equal(plan.seen[0].headers.authorization, "Bearer jwt." + "x".repeat(40))
})

test("the gateway answers a refused account by trying the next one", async (t) => {
  const plan = await fakePlan((body, n) => (n === 1 ? { json: { code: 3008, msg: "user concurrency limit" } } : { json: messagesAnswer("second answered") }))
  const pool = new Pool([target({ base: plan.base, label: "first · start" }), target({ accountId: "a2", base: plan.base, label: "second · start" })], { rpm: 0 })
  const gateway = createGateway({ pool, token: "", log: () => {}, version: "test", busyWaits: [10, 20] })
  const base = await listening(gateway)
  t.after(() => {
    gateway.close()
    plan.server.close()
  })

  const res = await fetch(base + "/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ model: "glm-4.6", messages: [{ role: "user", content: "hi" }] }),
  })
  assert.equal(res.status, 200)
  const j = await res.json()
  assert.equal(j.choices[0].message.content, "second answered")
  const stats = pool.stats()
  assert.equal(stats.accounts.reduce((n, a) => n + a.refused, 0), 1)
  assert.equal(stats.accounts.reduce((n, a) => n + a.served, 0), 1)
})

test("the gateway passes an Anthropic request through untouched", async (t) => {
  const plan = await fakePlan(() => ({ json: messagesAnswer("as it came") }))
  const pool = new Pool([target({ base: plan.base })], { rpm: 0 })
  const gateway = createGateway({ pool, token: "tok", version: "test" })
  const base = await listening(gateway)
  t.after(() => {
    gateway.close()
    plan.server.close()
  })

  const res = await fetch(base + "/v1/messages", {
    method: "POST",
    headers: { "content-type": "application/json", "x-api-key": "tok" },
    body: JSON.stringify({ model: "glm-4.6", max_tokens: 16, messages: [{ role: "user", content: "hi" }] }),
  })
  assert.equal(res.status, 200)
  const j = await res.json()
  assert.equal(j.content[0].text, "as it came")
  // Anthropic's shape, not OpenAI's: no choices, and max_tokens untouched
  assert.equal(j.choices, undefined)
  assert.equal(plan.seen[0].body.max_tokens, 16)
})

test("a plan is addressed by its own spelling of the model", async (t) => {
  // the plan declares GLM-5.3-Flash; a client asking for glm-5.3-flash must
  // reach it under the plan's spelling, which is the only one the vendor takes
  const plan = await fakePlan(() => ({ json: messagesAnswer("spelled right") }))
  const pool = new Pool([target({ base: plan.base, models: ["GLM-5.3-Flash"] })], { rpm: 0 })
  const gateway = createGateway({ pool, token: "tok", log: () => {}, version: "test" })
  const base = await listening(gateway)
  t.after(() => {
    gateway.close()
    plan.server.close()
  })

  const res = await fetch(base + "/v1/chat/completions", {
    method: "POST",
    headers: { authorization: "Bearer tok", "content-type": "application/json" },
    body: JSON.stringify({ model: "glm-5.3-flash", messages: [{ role: "user", content: "hi" }] }),
  })
  assert.equal(res.status, 200)
  assert.equal(plan.seen[0].body.model, "GLM-5.3-Flash")

  const models = await (await fetch(base + "/v1/models", { headers: { authorization: "Bearer tok" } })).json()
  assert.deepEqual(models.data.map((m) => m.id), ["GLM-5.3-Flash"])
})

test("a model no plan serves is refused before a request is spent", async (t) => {
  const plan = await fakePlan(() => ({ json: messagesAnswer("should not be reached") }))
  const pool = new Pool([target({ base: plan.base, models: ["GLM-5.3-Flash"] })], { rpm: 0 })
  const gateway = createGateway({ pool, token: "tok", log: () => {}, version: "test" })
  const base = await listening(gateway)
  t.after(() => {
    gateway.close()
    plan.server.close()
  })

  const res = await fetch(base + "/v1/chat/completions", {
    method: "POST",
    headers: { authorization: "Bearer tok", "content-type": "application/json" },
    body: JSON.stringify({ model: "gpt-4o", messages: [{ role: "user", content: "hi" }] }),
  })
  assert.equal(res.status, 400)
  const j = await res.json()
  assert.equal(j.error.code, "model_not_found")
  assert.match(j.error.message, /GLM-5\.3-Flash/)
  // nothing was sent: a wrong name costs a request, and requests are the scarce thing
  assert.equal(plan.seen.length, 0)
})

// jwt makes a token shaped like the plan's: three parts, the subject in the
// middle one, and a signature nobody checks here.
const jwt = (sub, salt = "a") =>
  "eyJhbGciOiJIUzI1NiJ9." +
  Buffer.from(JSON.stringify({ sub, user_id: sub, iat: 1, salt })).toString("base64url") +
  ".c2lnbmF0dXJl"

test("one ZCode account saved under several names is one target", async (t) => {
  // the app saves a sign-in again with a fresh token, so the same account
  // appears under two names with two different keys; counting it twice would
  // let the pool run twice the traffic the account's cap allows
  const dir = mkdtempSync(join(tmpdir(), "zsg-store-"))
  const accounts = join(dir, "accounts")
  mkdirSync(accounts, { recursive: true })
  const save = (id, name, token) =>
    writeFileSync(
      join(accounts, id + ".json"),
      JSON.stringify({
        id,
        name,
        updated_at: "2026-11-13T00:00:00Z",
        virtual_device_mid: "device-" + id,
        config: {
          provider: {
            "builtin:zai-start-plan": {
              enabled: true,
              models: { "GLM-5.3-Flash": {} },
              options: { apiKey: token, baseURL: "https://zcode.z.ai/api/v1/zcode-plan/anthropic" },
            },
          },
        },
      }),
    )
  save("a1", "first name", jwt("user-1", "one"))
  save("a2", "second name", jwt("user-1", "two"))
  save("a3", "other user", jwt("user-2"))
  process.env.ZCODE_SWITCH_STORE_DIR = dir
  t.after(() => {
    delete process.env.ZCODE_SWITCH_STORE_DIR
    rmSync(dir, { recursive: true, force: true })
  })

  assert.equal(jwtSubject(jwt("user-9")), "user-9")
  assert.equal(jwtSubject("enc:v2:os:sealed"), "")

  const targets = loadTargets({ includeLive: false })
  assert.equal(targets.length, 2)
  const folded = targets.find((x) => x.subject === "user-1")
  assert.equal(folded.aliases.length, 1)
  // the kept one is the first after sorting, and it remembers the other name
  assert.ok([folded.accountName, folded.aliases[0]].includes("first name"))
})

test("the token is required, and /healthz is not behind it", async (t) => {
  const pool = new Pool([target()], { rpm: 0 })
  const gateway = createGateway({ pool, token: "tok", version: "test" })
  const base = await listening(gateway)
  t.after(() => gateway.close())

  const health = await fetch(base + "/healthz")
  assert.equal(health.status, 200)
  assert.equal((await health.json()).ok, true)
  const denied = await fetch(base + "/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ messages: [{ role: "user", content: "hi" }] }),
  })
  assert.equal(denied.status, 401)
  const models = await fetch(base + "/v1/models", { headers: { authorization: "Bearer tok" } })
  assert.equal(models.status, 200)
  assert.ok(Array.isArray((await models.json()).data))
})

function messagesAnswer(said) {
  return {
    id: "msg_1",
    type: "message",
    role: "assistant",
    model: "glm-4.6",
    stop_reason: "end_turn",
    content: [{ type: "text", text: said }],
    usage: { input_tokens: 1, output_tokens: 1 },
  }
}
