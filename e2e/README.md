# e2e — Playwright E2E against the mock engine

Browser-mode end-to-end tests for the Angular client. No Rust build, no
WhatsApp connection, no phone: `mock-engine.mjs` is a dependency-free Node HTTP
server that implements the subset of `API-WHATSAPP.md` the client uses today
(handshake + capability token, `GET /events` SSE, chats, search, groups,
`POST /send/message`), and Playwright runs the real `ng serve` client against it.

## Run

```bash
npm run e2e:install      # once: e2e deps + the Playwright chromium build
npm run e2e              # from the repo root (== npm --prefix e2e test)
cd e2e && npx playwright test      # from inside e2e/ (config is found by cwd)
npm --prefix e2e run test:headed   # watch it happen in a window
npm --prefix e2e run test:ui       # Playwright UI mode
```

Playwright starts both servers itself (and reuses them if already running):

| server | port | command |
|---|---|---|
| mock engine | 8788 | `node mock-engine.mjs --port 8788 --token e2e-token` |
| Angular dev server | 4200 | `npm --prefix client start -- --port 4200` |

There is no dev-server proxy in this repo: the client talks to the engine
directly, so each spec navigates to `/?engine=<BEBOK_READY …>` — the same
handshake `npm run full-build-dev` prints (`tauri.service.ts`). Overrides:
`E2E_CLIENT_PORT`, `E2E_MOCK_PORT`, `E2E_MOCK_TOKEN`.

## Flows

| spec | status | what it proves |
|---|---|---|
| `tests/qr.spec.ts` | passing | `?engine=` handshake → `GET /version` over HTTP with `Authorization: Bearer`; a bad token surfaces `unauthorized`; `WaEvent::Qr` reaches the client over SSE; **the pairing screen renders the active QR, advances the rotation on its own, restarts it on a new `qr` event, and only claims "paired" after `pair_success`**. |
| `tests/send-message.spec.ts` | passing | `POST /send/message` (contract of `API-WHATSAPP.md` §3.2) records the message with the capability token, the engine echo `message` event arrives over SSE, and the chat list preview shows the new text; an empty text is a 400. |
| `tests/group-create.spec.ts` | passing | full UI flow: chat list → "New group" → form → `POST /group/create` with normalised, de-duplicated JIDs; whitespace-only names never reach the engine; an engine `{ error }` is shown in the form. |

One `test.fixme` placeholder documents a missing client feature (reported as
*skipped*, it does not fail the run):

* **Composer** — `chat.component.ts` says the message view arrives in Phase 4,
  and `WhatsappService` has no `sendMessage` method / `bebok-server` has no
  `/send/message` route (`docs/PROMPT-PHASE-4.md`: "No send/message routes yet").

## QR / pairing flow — the path a test must walk

Verified by hand against the real engine (`npm run full-build-dev`) and encoded
in `tests/qr.spec.ts`. Selectors are stable `data-testid` hooks in
`client/src/app/features/auth/login.component.ts`.

```
1. open   http://localhost:4200/?engine=BEBOK_READY%20http://127.0.0.1:8787/?token=<TOKEN>
          (Playwright: engineUrl('/login') — the app strips `engine` after storing it)
2. the engine is online           → [data-testid=pair-status] contains "Engine: online"
3. click                          → [data-testid=connect]      (POST /connect)
4. engine → SSE                   → WaEvent::Qr { codes: [c0, c1, …] }   (all rotations at once)
5. screen shows ONE code          → [data-testid=qr-code]  (280×280 <img>, src = qrserver.com + raw payload)
                                    [data-testid=qr-raw]   (the exact string, e.g.
                                                            https://wa.me/settings/linked_devices#…)
                                    [data-testid=qr-status] "Code 1 of N · rotates in 20s"
6. every 20 s the screen advances → [data-testid=qr-status] "Code 2 of N" and [qr-raw] changes,
   (QR_CHANNEL_TIMEOUT in wa-core)  no click needed
7. a fresh `qr` event restarts it → back to "Code 1 of N"
8. phone scans → `pair_success`   → [data-testid=paired] is visible, [qr-code] disappears
   (never on connected+loggedIn=false: a live socket without a linked device still shows the QR)
```

Non-UI half of the same flow (no component needed): `POST /connect` → poll
`GET /events` for `{ type: 'qr', codes }`, add the capability token as
`Authorization: Bearer`. `MOCK_ORIGIN`/`mock.pair()` finish the (fake) pairing so
step 8 is reachable without a phone.

The `mock` fixture is `auto`: it `POST /__mock/reset`s before **every** test, so
the pairing state of one spec cannot leak into the next.

Remove the `test.fixme` (composer) and that flow is complete too. When the
pairing UI changes, keep step 5–8 selectors in sync — they are the contract this
path is written against.

## Mock engine

`mock-engine.mjs` is also usable standalone — handy when hacking on the client
without the Rust engine:

```bash
node e2e/mock-engine.mjs --port 8788 --token dev
# → MOCK_READY http://127.0.0.1:8788/?token=dev   (paste the URL in Settings)
```

Test-control routes (not part of the engine contract, never authenticated):

| route | purpose |
|---|---|
| `GET /__mock/requests` | every request seen: `{ method, path, query, authorization, body, status }` |
| `GET /__mock/state` | `{ sse, connected, requests, chats, groups }` |

Besides the control routes, the mock serves `GET /status` in the same shape as
`bebok-server`'s `ConnectionStatus` (`state`/`connected`/`loggedIn`/`error`/`attempts`)
— the login screen polls it every 2 s while pairing.
| `POST /__mock/emit` | publish one `WaEvent` to all SSE subscribers |
| `POST /__mock/pair` | mark the device paired + publish `pair_success` (the "scan succeeded" branch) |
| `POST /__mock/reset` | drop recorded requests + messages, back to fixtures |

Fixtures: one contact chat (`48123123123@s.whatsapp.net`, "Hello from the mock
engine") and one group (`120363000000000001@g.us`). `POST /group/create` fails
with 400 when the name contains `reject`, so error paths are testable.
`POST /send/message` answers `{ id, to, text }` — the wire shape is this mock's
choice because the Rust route does not exist yet; adjust `mock-engine.mjs` and
the spec together when it lands.

## Layout

```
e2e/
├── playwright.config.ts   # two webServers + chromium project, serial workers
├── fixtures.ts            # readyLine()/engineUrl(), mock fixture, SSE instrumentation
├── mock-engine.mjs        # the mock engine (standalone usable)
├── package.json           # @playwright/test only
└── tests/                 # qr, send-message, group-create
```

Artifacts (`test-results/`, `playwright-report/`) are gitignored.
