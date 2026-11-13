# probe — what a ZCode Start Plan actually serves

A Start Plan is free, and its edge has two limits that are not documented
anywhere: how many requests one account may have in flight, and how fast this
address may send them before the firewall (errors.aliyun.com) refuses the whole
IP with a 405 for one to two hours. The plugin this repo studies learned both
the hard way — its source records "up to ~10 in flight against a cap of about
three", "sustained ~18 requests/min", and "3–7.5 minutes later the edge
firewall refuses the whole IP" (`packages/zcode/index.mjs:2229`).

This folder measures them here, on this account and this address, before the
gateway design has to commit to numbers.

Nothing here writes to `~/.zcode`. Every attempt lands in
`probe/results/run-*.jsonl` and a summary beside it.

## where the token comes from

| file | key | state |
|---|---|---|
| `~/.zcode/v2/config.json` | `provider["builtin:zai-start-plan"].options.apiKey` | plaintext JWT — this is the one read |
| `~/.zcode/v2/credentials.json` | `zcodejwttoken` | sealed `enc:v2:os:…`; the key is held by the OS, so it is unreadable here |
| `~/.zcode/v2/telemetry-state.json` | `deviceMid` | the device every request names in `metadata.user_id` |

The same `config.json` entry carries the base URL, so a moved endpoint is
followed rather than assumed. If ZCode ever stops writing that key, the probe
falls back to `credentials.json` and says plainly when that file is sealed.

## the commands

```bash
node probe/probe.mjs --plan           # is the plan on yet? (reads its balance; no model request)
node probe/probe.mjs --check          # one request: is this account served at all?
node probe/probe.mjs --dry-run        # the exact request, token redacted
node probe/probe.mjs                  # the ramp: 1, 2, 3, 4, 6 at once, twice each
node probe/probe.mjs --levels 1,2,3 --rounds 2 --gap 45000    # the safe cut
node probe/probe.mjs --mode models    # same model x3  vs  three models x1
node probe/probe.mjs --mode rate --rate 12 --minutes 4 --yes-risk   # the wall (see below)
```

`--plan` is the one to run first every evening: it reads the plan's balance and
costs no model request at all. `--check` then costs exactly one and tells you
whether the account is served, whether the token is still good, and whether this
address is currently blocked.

### what each mode answers

**ramp** — how many requests one account serves at once. At each level it fires
that many simultaneously, each on its own session, and records what came back.
The last level where every request was served is the account's ceiling; the
first level that produced `busy` is the cap. Two rounds per level, so one lucky
answer cannot be mistaken for a ceiling.

`--stagger <ms>` spreads the launches inside a level instead of firing them in
the same instant. That distinction is the whole finding below: a plan refuses a
burst of arrivals, not an overlap — the same requests 1.2 s apart overlap and are
served.

**models** — whether a 3009 (the model's own in-flight cap) is per model or per
account. It fires three at once on a single model, then three at once on three
different models, and compares. If the second set is served where the first was
refused, the cap is the model's and a gateway can spread load across models
instead of waiting.

**rate** — the arrival-rate wall, and the only mode that can cost the address
its access. Opt-in behind `--yes-risk` for that reason. It raises requests per
minute until a 405 appears, then stops. **While the address is blocked, this
machine gets nothing from zcode.z.ai — the ZCode app's own quota reads and
check-ins included.**

## reading the run

| kind | means | what to do |
|---|---|---|
| `ok` | served | — |
| `busy` | 3008 account cap, 3009 model cap, 3010 turn not yet | the ceiling, not a failure |
| `spent` | 1113/1005, the plan has no quota | stop; nothing about concurrency is measurable |
| `rate` | 429 / 1302 / 1303 / 3002, arrival rate | slow down; do not raise concurrency |
| `risk` | 3012, the vendor's risk control | stop for the day |
| `edge` | 405 or a block page | the address is blocked, 1–2 hours: stop |
| `auth` | 401/403 | sign in to ZCode again |
| `timeout`/`net` | the request did not arrive | check the network, not the plan |

The probe stops on its own when it sees an `edge` answer, when a level serves
nothing after the level before it also served nothing, and when `--max-requests`
is reached. A preflight request runs before every measuring mode: a plan with no
quota left answers everything the same way, and a table of one number is worse
than no table. `--skip-preflight` measures anyway.

## the rules the design has to respect

1. **The address is the shared budget.** Two accounts do not double the safe
   rate; they halve the margin. The plugin sets `maxAccounts: 2` and notes that
   three accounts from one address got the address refused.
2. **Cap is per account, block is per address.** Gates are independent; a 405 is
   not.
3. **Anything else on this machine counts.** The ZCode app itself, and any other
   probe, is in the same caps and the same address.
4. **A refusal is not a reason to retry harder.** The plugin answers a 3008 by
   waiting and, after five sustained double-refusals in a minute, holding off
   for three minutes process-wide. Copy that, not a retry loop.
5. **Rest between rounds.** `--gap 45000` is the default for a reason.

## what the first evening already showed

Measured on 2026-11-13, before the free plan was live, so that tonight starts
from a known state rather than from a guess:

- `--plan` on this account answered `{"plans":[],"balances":[]}` — no Start Plan
  attached at that moment. A model request against the same token answered
  **200** with `{"code":1005,"msg":"exceed quota limit"}`: the wire is right, the
  plan is simply not on. That is the shape of a "before" answer and it is
  distinguishable from `auth` (401) and from `edge` (405).
- `credentials.json` is sealed `enc:v2:os:…` throughout; the readable Start Plan
  JWT lives in `config.json`. A gateway must read the provider list, not the
  credential store, or it will send a sealed blob and get a 401 with an empty
  body (which is exactly what this probe did first).
- The token carries `iat` and no `exp`, so it does not lapse on a clock — what
  stops it is a spent or absent plan, and re-signing-in.

## with a live plan (2026-11-13, 23:00–23:30)

The free *ZCODE WEEKEND BUILD* plan came on for five of the saved accounts
(`zcode-v3-start-plan-1113-wk`, 23:00 → 03:00). Against one of them:

- **A plan serves one request at a time.** Three, four and five fired in the same
  instant each served exactly one and refused the rest with `3008` in ~150 ms —
  22 refusals over the ladder, nothing else. `3009`/`3010` never appeared.
- **What it refuses is the burst, not the overlap.** Fired 1.2 s apart, requests
  are served while earlier ones are still running: `i=0 ok (1222 ms)`,
  `i=1 ok (1200→4635 ms)`, `i=2 busy 153 ms`, `i=3 busy 130 ms`,
  `i=4 ok (859 ms)`, `i=5 ok (1247 ms)`. Every refusal lines up with a second
  request already in flight; the successes line up with the account being free.
- So the plugin's `maxConcurrency: 3` is a client-side gate, not the vendor's
  number: it admits three at once and answers each `3008` by waiting a second and
  asking again. A gateway that paces its starts gets the same throughput without
  spending refusals — see `../gateway` (`--inflight 1`, 800 ms start gap).
- Every successful answer still costs ~1076 prompt tokens of dressing, and a
  thinking model spends `max_tokens` on `reasoning_content` first: with 48 tokens
  the content is empty, with 64 it is a few words.

## tonight

The free Start Plan resets, so the order matters:

```bash
node probe/probe.mjs --plan                                   # is it on yet?
node probe/probe.mjs --check                                  # one request served?
node probe/probe.mjs --levels 1,2,3 --rounds 2 --gap 45000    # how many at once
node probe/probe.mjs --mode models                            # is 3009 per model?
```

Read the first table's `ok` column: the level where it stops matching `level` is
the account's ceiling. That number, plus the rest interval it took to keep it,
is what the gateway's `inflight` and spacing are built from. `--mode rate` is
there for the day someone wants to know the arrival wall on purpose — it is not
part of tonight.

## shape

```
probe/
  probe.mjs           the CLI: ramp, models, rate
  lib/creds.mjs       reads the sign-in; prints it without the token
  lib/wire.mjs        one dressed Start Plan request, from the plugin's own dress()
  lib/report.mjs      what an answer means; the jsonl and the summary
  results/            runs, one folder, not committed
```

`wire.mjs` imports the reference plugin's `_internal` (dress, PROMPT, sessionOf,
queryOf) from `../_ref/magpie-plugins/packages/zcode/index.mjs` rather than
re-deriving the dressing. When the gateway is written in Rust, its output can be
diffed against that one source of truth instead of against a memory of it.
