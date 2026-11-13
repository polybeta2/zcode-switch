# gateway — a test-build reverse proxy

A local, OpenAI- and Anthropic-shaped HTTP endpoint in front of the ZCode plans
this machine is already signed in to. Start it with `npm run gateway` and point
any client that speaks the OpenAI API at `http://127.0.0.1:8787/v1`.

It is the "测试版反代" half of the fork: the accounts, the wire and the refusals
come from Z·SWITCH, the HTTP surface is new. Nothing here writes to your ZCode
install — it reads the saved accounts and one live sign-in, and sends what the
desktop app would have sent.

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

Per account: at most `--inflight` (3) requests at once and `--rpm` (12) starts a
minute per address. A request that finds no slot waits up to `--queue-ms`
(120 s) and is then answered `429` rather than left hanging.

| the vendor said | the pool does | the client sees |
| --- | --- | --- |
| 3008 / 3009 / 3010 (busy) | re-asks after 1 s, then 2 s; two refusals in a row narrow that account's gate for 5 min; five in a minute rest it for 3 min | the next account, else 429 |
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
  the whole address (405 for one to two hours) when the traffic looks wrong. Two
  accounts at 3 in flight is the honest ceiling — see `../probe`.
- **Not in the app.** This is a CLI beside the app, not a panel inside it. The
  Rust integration (menu entry, start/stop, a copyable base URL) is next.
- **Nothing is invented for a plan we have no key for.** An account with no Start
  Plan key is not a target; an account whose plan has not started answers 1005
  and is rested.
