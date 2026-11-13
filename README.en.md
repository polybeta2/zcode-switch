# Z·SWITCH (zcode-switch)

[简体中文](README.md) ｜ **English**

A Tauri 2 desktop tool for one-click switching between multiple ZCode accounts, with live quota display. Only the login identity changes — projects, sessions, settings and plugins all stay untouched.

![screenshot](docs/screenshot.png)

## Features

- **Save / switch accounts**: one-click login switching; the current login is auto-preserved before any switch — accounts are never lost
- **Add accounts**: OAuth login for new accounts inside the tool (BigModel / z.ai entries), never touching the current login
- **Quota display**: inline plan quota and reset time per account row, multi-plan grouping
- **Claim promotions**: one-click claim for eligible promotions; "Auto claim" toggle (off by default) checks and claims periodically — manual actions take priority
- **Encrypted import / export**: `.zsb` bundle, PBKDF2(100k) + AES-256-GCM password encryption
- **Bilingual UI (中文 / English)**: one-click switch in Settings — main window, tray, error messages and CLI output all covered; first run follows your OS language
- **Tray / autostart / CLI automation**

## Security Design

- **Local first**: all data stays on your machine; no telemetry, no remote storage; quota queries go directly to official endpoints
- **WebView CSP**: the `script-src` baseline is `'self'`; minimal third-party script/image sources are allowed for the official promotion web component. UI events do not rely on dynamic execution — they go through a whitelist-based dispatcher
- **No account loss**: the current login is auto-preserved before switching if not yet saved; file writes go through a temp file + atomic rename
- **Path traversal protection**: account id whitelist (`[A-Za-z0-9-]`); delete/read cannot escape the account store directory
- **Encrypted export**: PBKDF2-HMAC-SHA256 (100k iterations) + AES-256-GCM, random salt/nonce; wrong password simply fails, no plaintext traces
- **Credentials decrypted locally only**: decryption is used solely to display username/email; export files are password-encrypted

## Local gateway (test build)

Turns the ZCode accounts saved on this machine into one OpenAI-compatible local endpoint: `http://127.0.0.1:8794/v1`. Flip the switch on the settings page (it shows a copyable base URL and token), or use the CLI.

- Endpoints: `GET /v1/models`, `POST /v1/chat/completions` (OpenAI), `POST /v1/messages` (Anthropic, forwarded byte for byte), `GET /healthz` (per-account plan and traffic)
- Model names are spelled by the plan itself: `GLM-5.3-Flash`, `GLM-5.3`
- Loopback only, and a Bearer token is required (minted on first start, kept in `~/.zcode-switch/gateway.json`)
- Concurrency: one request in flight per account (a Start Plan really serves one at a time — the "3" is the client plugin's own gate), accounts run in parallel, and two starts on one account are spaced by 800ms
- Retries: `3008`/`3009`/`3010` (concurrency) do not switch accounts — the same account is asked again after 1–2s; `1005`/`1113` (no plan / no quota) switch; a refused address (405) rests every account for three minutes
- Requests are dressed like the ZCode client (three system blocks, session id, device fingerprint), so every request costs about 1070 extra prompt tokens

Known limits: this is a test build; the vendor rate-limits by address, so sustained high volume can earn a 405 for a while (the gateway backs off on its own); and a Start Plan sign-in does not refresh itself — when it expires, sign in again in ZCode.

## CLI

```
zcode-switch.exe --cli state|list
zcode-switch.exe --cli quota [--id <account-id>]
zcode-switch.exe --cli claim-preview [--id <account-id>]
zcode-switch.exe --cli capture [--name <name>]
zcode-switch.exe --cli switch --id <id> [--force] [--restart|--no-restart] [--hot <bool>|--no-hot]
zcode-switch.exe --cli kill
zcode-switch.exe --cli export --id <id> --out <a.zsb>
zcode-switch.exe --cli export-all --out <all.zsb>
zcode-switch.exe --cli import --file <file.zsb>
zcode-switch.exe --cli rename|delete|update|behavior|setpath|launch
zcode-switch.exe --cli gateway status|start [--port <port>]|stop
zcode-switch.exe --cli --lang en state              # English output (--lang takes a space-separated value, works anywhere in the command line; defaults to the GUI/system language)
```

CLI password (export / import): prefer the `ZSW_PASSWORD` environment variable (keeps it out of process lists and command history); `--password <password>` also works.

## FAQ

### macOS asks for Microphone / Accessibility / Screen Recording permission?

Deny all of them — nothing breaks.
The embedded pages (such as the login page) are rendered by the system WebView, which relays their requests as system permission prompts attributed to the app; neither the app itself nor its embedded pages use any of these three capabilities.

## Build

```bash
npm install
npm run tauri dev      # development (HMR)
npm run tauri build    # NSIS installer
```

Windows-first (path detection / process management / tray are all Win32 semantics).

## License

[MIT](./LICENSE)
