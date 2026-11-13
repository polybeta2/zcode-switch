# gateway — a test-build reverse proxy

A local, OpenAI- and Anthropic-shaped HTTP endpoint in front of the ZCode plans
this machine is already signed in to. Start it with `npm run gateway` and point
any client that speaks the OpenAI API at `http://127.0.0.1:8787/v1`.

It is the "测试版反代" half of the fork: the accounts, the wire and the refusals
come from Z·SWITCH, the HTTP surface is new. Nothing here writes to your ZCode
install — it reads the saved accounts and one live sign-in, and sends what the
desktop app would have sent.

**This Node build is the reference implementation.** The app now ships its own
copy in Rust (`src-tauri/src/gateway`, same pool/translate/dress behaviour, a
settings switch and `--cli gateway`); this directory is where the behaviour was
worked out and where it can be measured without building Tauri. When the two
disagree, the Rust one is what users run — fix both.

    npm run gateway                # listen on 127.0.0.1:8787
    npm run gateway -- --list      # what it would send with, then exit
    npm run gateway -- --port 9000 --quiet
    npm run test:gateway           # 21 tests, no network

## Endpoints

| method | path | what it is |
| --- | --- | --- |
| POST | `/v1/chat/completions` | OpenAI Chat Completions, translated to Messages, streaming or not |
| POST | `/v1/messages` | Anthropic Messages, dressed and forwarded as it came |
| GET | `/v1/models` | the models the plans here actually declare |
| GET | `/healthz` | no token needed: accounts, in-flight counts, rests |

    curl http://127.0.0.1:8787/v1/chat/completions \
      -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
      -d '{"model":"GLM-5.3-Flash","messages":[{"role":"user","content":"ping"}]}'

Anthropic-style clients use `x-api-key` instead of `Authorization`; both are
accepted, and `/v1/messages` can be pointed at directly (Claude Code, for
instance, needs `ANTHROPIC_BASE_URL=http://127.0.0.1:8787`).

## The token

`--token`, else `ZCODE_GATEWAY_TOKEN`, else `~/.zcode-switch/gateway.json`,
which is created on first run with a random `zsg_…` token. It is not a vendor
credential: it only guards the loopback port, so that another local program
cannot quietly spend your quota. Rotate it with `--new-token`.

## Accounts and models

Every saved account contributes one target per plan that has a usable key:

- `builtin:zai-start-plan` / `builtin:bigmodel-start-plan` — the free Start Plan
  JWT. Requests are **dressed**: the three system blocks, the session id, the
  query id, the metadata user id and the `ZCode/3.17.2` header fingerprint the
  desktop app sends.
- `builtin:zai-coding-plan` / `builtin:bigmodel-coding-plan` — a coding plan API
  key, sent plainly to `/api/anthropic`.
- `builtin:zai` / `builtin:bigmodel` — a plain API key, sent plainly.

Targets are tried Start Plan first. `--no-live` drops the ZCode sign-in this
machine is currently using, so the gateway never competes with the app itself.

**The model name is the plan's, not yours.** Each plan entry in an account
declares the models it serves (`GLM-5.3-Flash`, …), and the vendor refuses any
other name with code 3006 — a wrong name still costs a request, which is the
scarce thing here. So a request is matched case-insensitively against what the
account declares and sent under the plan's own spelling; a name no plan here
serves is refused with `400 model_not_found` before anything is sent. `GET
/v1/models` lists what is actually on offer.

## How a request is placed

Per account: **one request at a time**, starts spaced by 800 ms, and `--rpm` (12)
starts a minute per address. A request that finds no slot waits up to
`--queue-ms` (120 s) and is then answered `429` rather than left hanging.

That number is measured, not assumed. `../probe` fired requests 1.2 s apart on a
live Start Plan (23:00, *ZCODE WEEKEND BUILD*) and recorded this timeline:

```
i=0  ok   (1222 ms)
i=1  ok   (1200 → 4635 ms)      ← still running
i=2  busy (153 ms, code 3008)   ← refused while i=1 ran
i=3  busy (130 ms, code 3008)
i=4  ok   (859 ms)              ← i=1 had finished
i=5  ok   (1247 ms)
```

Nothing overlapped without a 3008, and a 3008 arrives in ~150 ms — before any
answer starts, so it is an admission refusal, not a slow one. The plugin ships
`maxConcurrency: 3`, but that is its own client-side gate: it admits three and
then answers each 3008 by waiting a second and asking again, which is why its log
reads "up to ~10 in flight against a cap of about three". Spacing the starts is
what makes a queue look like three at once.

| the vendor said | the pool does | the client sees |
| --- | --- | --- |
| 3008 / 3009 / 3010 (busy) | waits 1 s, then 2 s, and asks the **same** account again (it said "not this instant", not "no"); five in a minute rest it for 3 min | the next account if one is free, else 429 |
| 1005 / 1113 (spent, no plan) | rests that account for 10 min | the next account, else 402 |
| 1302 / 1303 / 3002, or 429 (rate) | rests it 2 min | the next account, else 429 |
| 3012 (risk) | rests it 5 min | the next account, else 403 |
| 405 / an Aliyun refusal (edge) | **every** account rests 3 min: the firewall refuses by address | 503 |
| 401 / 403 (auth) | rests it 10 min | the next account, else 401 |
| timeout / a broken stream | releases the slot, tries the next account | 504 after three tries |

`GET /healthz` shows what is resting and why, so a `429` can be told apart from
a spent plan.

## What it does not do

- **One address.** Every account leaves through this machine's IP. The vendor's
  concurrency cap is per account; the firewall's is per address, and it refuses
  the whole address (405 for one to two hours) when the traffic looks wrong. With
  one request per account in flight, five planned accounts are five at once — the
  number that matters is how many accounts have a plan, not `--inflight`.
- **Not in the app.** This is a CLI beside the app, not a panel inside it. The
  Rust integration (menu entry, start/stop, a copyable base URL) is next.
- **Nothing is invented for a plan we have no key for.** An account with no Start
  Plan key is not a target; an account whose plan has not started answers 1005
  and is rested.
