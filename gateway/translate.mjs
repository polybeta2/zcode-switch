// translate.mjs — the OpenAI shape this gateway speaks, and the Anthropic shape
// the vendor does.
//
// ZCode's plans take Messages requests and nothing else, so a client that
// speaks OpenAI (chat/completions, streamed or not) has to be translated on the
// way in and on the way out. The translation is deliberately thin: what OpenAI
// has no field for is dropped, and what it has and Anthropic has not (n > 1) is
// refused rather than silently answered once.
//
// Everything here is pure: no socket, no clock beyond created_at, no state
// between calls, so translate.test.mjs can check it without a network.

// STOP_MAP is Anthropic's stop_reason as OpenAI's finish_reason.
const STOP_MAP = {
  end_turn: "stop",
  stop_sequence: "stop",
  max_tokens: "length",
  tool_use: "tool_calls",
  refusal: "content_filter",
  pause_turn: "stop",
}

export const finishReason = (stop) => STOP_MAP[stop] || "stop"

// textOf is the text an OpenAI content value carries, whether it is a string or
// a list of parts. Parts that are not text (an image, an audio clip) are left
// out here and handled by partsToAnthropic.
const textOf = (content) => {
  if (typeof content === "string") return content
  if (!Array.isArray(content)) return ""
  return content
    .map((p) => (typeof p === "string" ? p : p && typeof p.text === "string" ? p.text : ""))
    .join("")
}

// partsToAnthropic is an OpenAI content list as Anthropic blocks. Images are
// the one multimodal part both sides have: a data: URL becomes base64 source, a
// plain URL becomes a url source. Anything else is dropped, because sending a
// part the vendor does not know is a 400 for the whole request.
function partsToAnthropic(content, warn) {
  const out = []
  for (const p of content) {
    if (typeof p === "string") {
      if (p) out.push({ type: "text", text: p })
      continue
    }
    if (!p || typeof p !== "object") continue
    if (p.type === "text" && typeof p.text === "string") {
      if (p.text) out.push({ type: "text", text: p.text })
      continue
    }
    if (p.type === "image_url") {
      const url = typeof p.image_url === "string" ? p.image_url : p.image_url && p.image_url.url
      if (typeof url !== "string" || !url) continue
      const m = /^data:([^;,]+);base64,(.*)$/.exec(url)
      if (m) out.push({ type: "image", source: { type: "base64", media_type: m[1], data: m[2] } })
      else out.push({ type: "image", source: { type: "url", url } })
      continue
    }
    if (p.type === "input_audio" || p.type === "file") warn("dropped an " + p.type + " part: the vendor takes text and images only")
  }
  return out
}

// toolResultBlock is one tool message as Anthropic's tool_result.
function toolResultBlock(m) {
  const content = m.content
  if (Array.isArray(content)) {
    const parts = partsToAnthropic(content, () => {})
    return { type: "tool_result", tool_use_id: m.tool_call_id, content: parts.length ? parts : "" }
  }
  return { type: "tool_result", tool_use_id: m.tool_call_id, content: textOf(content) }
}

// toAnthropic turns an OpenAI chat.completions request into a Messages body.
//
// The order matters: OpenAI puts system turns anywhere, Anthropic has one
// system string; OpenAI's assistant tool_calls and the tool results that follow
// are two messages, Anthropic's tool_use and tool_result blocks live inside
// ordinary turns. Getting that wrong is what makes a tool call come back as a
// 400 about a missing tool_result.
export function toAnthropic(body, { defaultMaxTokens = 4096, warn = () => {} } = {}) {
  if (!body || typeof body !== "object") throw new Error("a request body is required")
  if (typeof body.model !== "string" || !body.model.trim()) throw new Error("model is required")
  const messages = Array.isArray(body.messages) ? body.messages : []
  if (!messages.length) throw new Error("messages must not be empty")
  if (Number.isFinite(body.n) && body.n > 1) throw new Error("n > 1 is not supported: ZCode's plans answer one choice per request")

  const system = []
  const out = []
  for (const m of messages) {
    if (!m || typeof m !== "object") continue
    if (m.role === "system" || m.role === "developer") {
      const t = textOf(m.content)
      if (t.trim()) system.push(t)
      continue
    }
    if (m.role === "tool") {
      // consecutive tool results are one user turn, as Anthropic wants them
      const block = toolResultBlock(m)
      const prev = out[out.length - 1]
      if (prev && prev.role === "user" && Array.isArray(prev.content) && prev.content.every((b) => b.type === "tool_result")) prev.content.push(block)
      else out.push({ role: "user", content: [block] })
      continue
    }
    if (m.role === "assistant") {
      const blocks = []
      const t = textOf(m.content)
      if (t) blocks.push({ type: "text", text: t })
      const calls = Array.isArray(m.tool_calls) ? m.tool_calls : []
      for (const c of calls) {
        const fn = c && c.function ? c.function : null
        if (!fn || typeof fn.name !== "string") continue
        let input = {}
        try {
          input = fn.arguments ? JSON.parse(fn.arguments) : {}
        } catch {
          // an unparsable argument string is the client's bug, but a thrown
          // request here would lose the whole turn: send what it meant
          warn("tool call " + fn.name + " had arguments that are not JSON; sent as an empty object")
          input = {}
        }
        blocks.push({ type: "tool_use", id: c.id || "call_" + Math.random().toString(36).slice(2, 10), name: fn.name, input })
      }
      if (!blocks.length) continue
      out.push({ role: "assistant", content: blocks.length === 1 && blocks[0].type === "text" ? blocks[0].text : blocks })
      continue
    }
    if (m.role === "user") {
      const parts = Array.isArray(m.content) ? partsToAnthropic(m.content, warn) : typeof m.content === "string" ? (m.content ? [{ type: "text", text: m.content }] : []) : []
      if (!parts.length) continue
      // what the client sent right after a tool result belongs to the same
      // turn: Anthropic reads a turn as one block list, and splitting it makes
      // the model answer the tool result and the question separately
      const prev = out[out.length - 1]
      if (prev && prev.role === "user" && Array.isArray(prev.content) && prev.content.every((b) => b.type === "tool_result")) {
        prev.content.push(...parts)
        continue
      }
      out.push({ role: "user", content: parts.length === 1 && parts[0].type === "text" ? parts[0].text : parts })
      continue
    }
    warn("dropped a message with role " + String(m.role))
  }
  if (!out.length) throw new Error("messages must carry at least one user or assistant turn")
  // Anthropic keeps one system string; several arrive as one, in order
  const sys = system.length === 1 ? system[0] : system.join("\n\n")

  const req = {
    model: body.model,
    max_tokens: Number.isFinite(body.max_tokens) && body.max_tokens > 0 ? Math.floor(body.max_tokens) : defaultMaxTokens,
    messages: out,
    stream: Boolean(body.stream),
  }
  if (sys) req.system = sys
  if (Number.isFinite(body.temperature)) {
    // OpenAI allows 0..2, Anthropic 0..1: clamping loses less than a 400
    req.temperature = Math.min(1, Math.max(0, body.temperature))
    if (body.temperature > 1) warn("temperature " + body.temperature + " clamped to 1")
  }
  if (Number.isFinite(body.top_p)) req.top_p = Math.min(1, Math.max(0, body.top_p))
  const stop = body.stop
  if (typeof stop === "string" && stop) req.stop_sequences = [stop]
  else if (Array.isArray(stop) && stop.length) {
    req.stop_sequences = stop.filter((s) => typeof s === "string" && s).slice(0, 8)
    if (stop.length > 8) warn("kept the first 8 stop sequences")
  }
  if (Array.isArray(body.tools) && body.tools.length) {
    const tools = []
    for (const t of body.tools) {
      const fn = t && t.type === "function" ? t.function : null
      if (!fn || typeof fn.name !== "string") {
        warn("dropped a tool that is not a function")
        continue
      }
      tools.push({
        name: fn.name,
        ...(typeof fn.description === "string" && fn.description ? { description: fn.description } : {}),
        input_schema: fn.parameters && typeof fn.parameters === "object" ? fn.parameters : { type: "object", properties: {} },
      })
    }
    if (tools.length) req.tools = tools
    const choice = body.tool_choice
    if (tools.length && choice) {
      if (choice === "auto") req.tool_choice = { type: "auto" }
      else if (choice === "required") req.tool_choice = { type: "any" }
      else if (choice === "none") delete req.tools // Anthropic has no "none": no tools is the same thing
      else if (choice && choice.type === "function" && choice.function && typeof choice.function.name === "string")
        req.tool_choice = { type: "tool", name: choice.function.name }
    }
  }
  return req
}

// toOpenAI turns a Messages response into a chat.completion.
export function toOpenAI(message, { model, id, created = Math.floor(Date.now() / 1000) } = {}) {
  const blocks = Array.isArray(message?.content) ? message.content : []
  let text = ""
  let reasoning = ""
  const calls = []
  for (const b of blocks) {
    if (!b || typeof b !== "object") continue
    if (b.type === "text" && typeof b.text === "string") text += b.text
    else if (b.type === "thinking" && typeof b.thinking === "string") reasoning += b.thinking
    else if (b.type === "tool_use") {
      calls.push({
        id: b.id || "call_" + calls.length,
        type: "function",
        function: { name: b.name || "", arguments: JSON.stringify(b.input ?? {}) },
      })
    }
  }
  const usage = usageOf(message?.usage)
  const msg = { role: "assistant", content: text || (calls.length ? null : "") }
  if (reasoning) msg.reasoning_content = reasoning
  if (calls.length) msg.tool_calls = calls
  return {
    id: id || message?.id || "chatcmpl-" + Math.random().toString(36).slice(2, 12),
    object: "chat.completion",
    created,
    model: model || message?.model || "",
    choices: [{ index: 0, message: msg, finish_reason: finishReason(message?.stop_reason), logprobs: null }],
    usage: usage.openai,
  }
}

// usageOf is Anthropic's usage as OpenAI's, both tokens and cache detail.
function usageOf(u) {
  const input = Number(u?.input_tokens) || 0
  const output = Number(u?.output_tokens) || 0
  const cached = Number(u?.cache_read_input_tokens) || 0
  const written = Number(u?.cache_creation_input_tokens) || 0
  return {
    anthropic: { input_tokens: input, output_tokens: output, cache_read_input_tokens: cached, cache_creation_input_tokens: written },
    openai: {
      prompt_tokens: input + cached + written,
      completion_tokens: output,
      total_tokens: input + cached + written + output,
      ...(cached || written ? { prompt_tokens_details: { cached_tokens: cached } } : {}),
    },
  }
}

// sseText is one server-sent event, shaped as OpenAI's clients read them.
export const sseText = (obj) => "data: " + (typeof obj === "string" ? obj : JSON.stringify(obj)) + "\n\n"

// StreamTranslator turns the vendor's event stream into OpenAI chunks as it
// arrives: feed it whatever bytes are in hand, write what it returns, and call
// finish() once the upstream is done.
export class StreamTranslator {
  constructor({ model, id, created = Math.floor(Date.now() / 1000) }) {
    this.model = model
    this.id = id || "chatcmpl-" + Math.random().toString(36).slice(2, 12)
    this.created = created
    this.buffer = ""
    this.blockTypes = new Map() // block index -> "text" | "tool_use" | "thinking"
    this.toolIndex = new Map() // block index -> OpenAI tool_calls index
    this.nextTool = 0
    this.finishReason = null
    this.usage = null
    this.started = false
  }

  chunk(delta, finish_reason = null, extra = {}) {
    return sseText({
      id: this.id,
      object: "chat.completion.chunk",
      created: this.created,
      model: this.model,
      choices: [{ index: 0, delta, finish_reason, logprobs: null }],
      ...extra,
    })
  }

  // feed takes the bytes in hand and gives back the events they complete.
  feed(text) {
    this.buffer += text
    const out = []
    let at
    while ((at = this.buffer.indexOf("\n\n")) >= 0) {
      const raw = this.buffer.slice(0, at)
      this.buffer = this.buffer.slice(at + 2)
      out.push(...this.event(raw))
    }
    return out.join("")
  }

  event(raw) {
    let name = ""
    const data = []
    for (const line of raw.split("\n")) {
      if (line.startsWith("event:")) name = line.slice(6).trim()
      else if (line.startsWith("data:")) data.push(line.slice(5).trim())
    }
    if (!data.length) return []
    let j
    try {
      j = JSON.parse(data.join("\n"))
    } catch {
      return []
    }
    if (!name && typeof j.type === "string") name = j.type
    const out = []
    if (name === "message_start") {
      const message = j.message || {}
      if (message.id) this.id = message.id
      if (message.model) this.model = message.model
      if (message.usage) this.usage = message.usage
      this.started = true
      out.push(this.chunk({ role: "assistant", content: "" }))
      return out
    }
    if (name === "content_block_start") {
      const block = j.content_block || {}
      this.blockTypes.set(j.index, block.type)
      if (block.type === "tool_use") {
        const idx = this.nextTool++
        this.toolIndex.set(j.index, idx)
        out.push(this.chunk({ tool_calls: [{ index: idx, id: block.id || "call_" + idx, type: "function", function: { name: block.name || "", arguments: "" } }] }))
      }
      return out
    }
    if (name === "content_block_delta") {
      const d = j.delta || {}
      if (d.type === "text_delta" && typeof d.text === "string") out.push(this.chunk({ content: d.text }))
      else if (d.type === "thinking_delta" && typeof d.thinking === "string") out.push(this.chunk({ reasoning_content: d.thinking }))
      else if (d.type === "input_json_delta" && typeof d.partial_json === "string") {
        const idx = this.toolIndex.get(j.index)
        if (idx !== undefined) out.push(this.chunk({ tool_calls: [{ index: idx, function: { arguments: d.partial_json } }] }))
      }
      return out
    }
    if (name === "message_delta") {
      const d = j.delta || {}
      if (d.stop_reason) this.finishReason = finishReason(d.stop_reason)
      if (j.usage) this.usage = { ...(this.usage || {}), ...j.usage }
      return out
    }
    if (name === "message_stop") return out
    if (name === "error") {
      return [sseText({ error: { message: j.error?.message || "upstream error", type: j.error?.type || "upstream_error", code: j.error?.type || null } })]
    }
    return out
  }

  // finish closes the stream: the last chunk carries the finish reason and the
  // usage, then the sentinel OpenAI clients stop on.
  finish() {
    const out = []
    const u = this.usage ? usageOf(this.usage).openai : null
    out.push(this.chunk({}, this.finishReason || "stop", u ? { usage: u } : {}))
    out.push(sseText("[DONE]"))
    return out.join("")
  }
}
