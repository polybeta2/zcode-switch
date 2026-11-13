// dress.test.mjs — the request ZCode sends, as this repo sends it.
//
//   node --test gateway/test/
//
// The hashes below are the vendor's own prompt texts as ZCode 3.17.2 carries
// them (and as the magpie plugin records them). A paragraph reworded here is
// the difference between a request the plan answers and one its risk control
// reads as another client, so a changed text fails a test rather than a
// request. When the vendor moves its version, APP_VERSION and these hashes
// move together.
import test from "node:test"
import assert from "node:assert/strict"
import { createHash } from "node:crypto"
import PROMPT from "../prompt.mjs"
import { APP_VERSION, dress, marked, queryOf, sessionOf, startHeaders, userIdOf } from "../dress.mjs"

const hash = (s) => createHash("sha256").update(String(s)).digest("hex").slice(0, 16)

const EXPECTED = {
  cliPrefix: "7779642ea44a4696",
  "stableSections[0]": "aac4ba4cc7b555b3",
  "stableSections[1]": "86e0396775d5ed0e",
  dynamicBehavior: "7d843753f3adae93",
  sessionGuidance: "6596a0131800c779",
  contextManagement: "406a0480d88086cd",
  poweredBy: "c858af0dee78d974",
  "environment.heading": "31a5545a3645cdbe",
  "environment.invokedLine": "faba11c0d742e231",
  "environment.cwdLabel": "9764930603840a85",
  "environment.gitLabel": "60e9724fc28abb16",
  "environment.gitYes": "b5bea41b6c623f7c",
  "environment.gitNo": "fcbcf165908dd18a",
  "environment.platformLabel": "c78ffe19571018fb",
  "environment.shellLabel": "a733285486d54383",
  "environment.osVersionLabel": "055a3d19928f5644",
  "environment.untrustedNote": "2eb4e85cb1768f1b",
  "context.currentDateLine": "11038172be331538",
}

// pick reads a dotted path out of the prompt object.
function pick(path) {
  return path.split(".").reduce((v, k) => (v == null ? v : v[k]), PROMPT)
}

test("the prompt texts are the ones ZCode 3.17.2 carries", () => {
  assert.equal(APP_VERSION, "3.17.2")
  for (const [path, want] of Object.entries(EXPECTED)) {
    const value = path.startsWith("stableSections[")
      ? PROMPT.stableSections[Number(path.slice(15, path.indexOf("]"))) ]
      : pick(path)
    assert.equal(hash(value), want, path + " is not the vendor's text any more")
  }
})

const body = () => ({
  model: "glm-4.6",
  max_tokens: 64,
  system: "the agent's own system text",
  messages: [
    { role: "user", content: "first question" },
    { role: "assistant", content: "first answer" },
    { role: "user", content: [{ type: "text", text: "second question", cache_control: { type: "ephemeral" } }] },
  ],
})

test("a dressed request is three system blocks, the context prefix, and one cache mark", () => {
  const out = JSON.parse(dress(JSON.stringify(body()), "zai", "device-abc", {}, "session-1"))
  assert.equal(out.system.length, 3)
  assert.equal(out.system[0].text, PROMPT.cliPrefix)
  assert.equal(out.system[1].text, PROMPT.stableSections.join("\n\n"))
  // the third block is the agent's own text, appended to ZCode's dynamic block
  assert.match(out.system[2].text, /the agent's own system text/)
  assert.ok(out.system.every((b) => b.cache_control?.type === "ephemeral"))

  // the first turn is ZCode's context prefix: the day in a system-reminder,
  // as a user turn, before anything the agent sent
  assert.equal(out.messages[0].role, "user")
  assert.equal(out.messages[0].content[0].type, "text")
  assert.match(out.messages[0].content[0].text, /^<system-reminder>Today's date is \d{4}-\d{2}-\d{2}\.<\/system-reminder>$/)
  // and the conversation the agent sent follows it, unfiltered
  assert.equal(out.messages.length, 4)

  // one cache mark, on the last turn's last block, and none in the tools
  const marks = []
  for (const m of out.messages) {
    const blocks = Array.isArray(m.content) ? m.content : [{ text: m.content }]
    for (const b of blocks) if (b && b.cache_control) marks.push(b)
  }
  assert.equal(marks.length, 1)
  assert.equal(marks[0].text, "second question")

  const named = JSON.parse(out.metadata.user_id)
  assert.equal(named.device_id, "device-abc")
  assert.equal(named.session_id, "session-1")
})

test("a body that is not a Messages request goes as it came", () => {
  const raw = JSON.stringify({ messages: [], nothing: true })
  assert.equal(dress("not json at all", "zai", "d", {}, "s"), "not json at all")
  const empty = JSON.parse(dress("{\"foo\":1}", "zai", "d", {}, "s"))
  assert.deepEqual(empty, { foo: 1 })
  // contextPrefix is the only turn added to an empty conversation, which the
  // vendor needs: a request with no turns is not a request it can cache
  const kept = JSON.parse(dress(raw, "zai", "d", {}, "s"))
  assert.equal(kept.messages.length, 1)
})

test("marked names the device on a coding plan, and leaves the rest alone", () => {
  const out = JSON.parse(marked(JSON.stringify({ messages: [{ role: "user", content: "hi" }] }), "device-9", "session-9"))
  assert.equal(JSON.parse(out.metadata.user_id).device_id, "device-9")
  assert.equal(out.system, undefined)
  // a body that already names a user is left as the agent sent it
  const own = JSON.parse(marked(JSON.stringify({ messages: [{ role: "user", content: "hi" }], metadata: { user_id: "agent-named" } }), "device-9", "s"))
  assert.equal(own.metadata.user_id, "agent-named")
})

test("the session and query ids are stable per conversation, not per request", () => {
  const a = sessionOf("agent-session", "device-1")
  assert.equal(a, sessionOf("agent-session", "device-1"))
  assert.notEqual(a, sessionOf("another-session", "device-1"))
  assert.match(a, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/)
  // a body with no session still gets one, from the device
  assert.ok(sessionOf("", "device-1").length === 36)

  const text = JSON.stringify({ messages: [{ role: "user", content: "what is this" }] })
  assert.equal(queryOf(text, a), queryOf(text, a))
  assert.notEqual(queryOf(text, a), queryOf(text, sessionOf("other", "device-1")))
  // nothing to name means no header at all
  assert.equal(queryOf("{\"messages\":[]}", a), "")
  assert.equal(queryOf("not a body", a), "")
})

test("startHeaders are ZCode's, and carry none of the agent's", () => {
  const h = startHeaders({ jwt: "jwt.value", sessionId: "s-1", queryId: "q-1", appVersion: "3.17.2" })
  assert.equal(h.Authorization, "Bearer jwt.value")
  assert.equal(h["X-Title"], "Z Code@electron")
  assert.equal(h["X-Release-Channel"], "production")
  assert.equal(h["x-session-id"], "s-1")
  assert.equal(h["x-query-id"], "q-1")
  assert.match(h["User-Agent"], /^ZCode\/3\.17\.2 ai-sdk\/anthropic\//)
  // the desktop app's agent calls send no device id; the body names it
  assert.equal(h["X-Device-Mid"], undefined)
  for (const k of Object.keys(h)) assert.doesNotMatch(k, /^x-stainless|^anthropic-beta|^x-app/i)
  // no session, no session header
  const bare = startHeaders({ jwt: "j", sessionId: "", queryId: "" })
  assert.equal(bare["x-session-id"], undefined)
  assert.equal(bare["x-query-id"], undefined)
})

test("userIdOf is the JSON string the vendor reads the device out of", () => {
  assert.equal(userIdOf("d-1", "s-1"), JSON.stringify({ device_id: "d-1", account_uuid: "", session_id: "s-1" }))
})
