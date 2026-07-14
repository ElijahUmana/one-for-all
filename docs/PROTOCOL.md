# one-for-all — PROTOCOL.md

**Audience:** anyone implementing a client, debugging the wire, or reading frames off a socket.
**Scope.** This document is the normative description of the wire protocol. Section identifiers and requirement IDs refer to the project's internal specification, which is not published.
**Version:** 1.0.0

---

## Three transports, three framings

```
claude CLI  ⇄  MCP server  ⇄  broker  ⇄  Chromium
       stdio        unix socket    pipe (fd 3 / fd 4)
       LSP framing  newline-delim  NUL-delim
       JSON-RPC 2.0 JSON-RPC 2.0   CDP JSON
```

Every claim below is enforced in code; the file/line where each cap is set is named so you can verify.

### 1. claude ⇄ MCP server (stdio)

- **Framing:** LSP-style, exactly as the upstream MCP stdio spec defines it.
- **Wire shape:**
  ```
  Content-Length: <decimal-bytes>\r\n
  \r\n
  <body>
  ```
- **Encoding:** UTF-8.
- **Header cap:** 8 KiB (`crates/mcp-server/src/mcp.rs:128`). A header longer than that returns `-32700 ParseError`.
- **Body cap:** none beyond the OS pipe buffer.
- **Headers we emit:** only `Content-Length`. Any others received are tolerated and ignored.
- **Reader/writer:** `read_lsp_frame` and `write_lsp_frame` in `crates/mcp-server/src/mcp.rs`.

Example (one full request frame on the wire — `\r\n` shown literally):

```
Content-Length: 92\r\n
\r\n
{"jsonrpc":"2.0","id":1,"method":"tab.open","params":{"url":"https://example.com"}}
```

### 2. MCP server ⇄ broker (unix socket)

- **Path:** `~/.one-for-all/broker.sock`. Created by the broker; permission 0600.
- **Framing:** newline-delimited JSON. One JSON-RPC envelope per `\n`. No length prefix, no header.
- **Encoding:** UTF-8. `\n` inside string values must be JSON-escaped (`\\n`).
- **Line cap:** 16 MiB (`crates/broker/src/server.rs:24` — `LINE_CAP_BYTES = 16 * 1024 * 1024`). Lines exceeding the cap close the connection with `-32700`.
- **Why not LSP framing here?** The broker only speaks to local trusted MCP clients; framing simplicity matters more than streaming-large-bodies efficiency. If you need to push a 14MB screenshot, base64-encode it and send it as one JSON line.

### 3. broker ⇄ Chromium (`--remote-debugging-pipe`)

- **Framing:** NUL-delimited JSON per Chromium's `--remote-debugging-pipe`. One JSON object per `\0`-terminated record.
- **File descriptors:** `fd 3` is read-from-broker / write-to-Chromium; `fd 4` is write-by-Chromium / read-by-broker. (Standard `--remote-debugging-pipe` conventions.)
- **Encoding:** UTF-8.
- **Frame cap:** 100 MiB (`crates/cdp-client/src/framing.rs:21` — `DEFAULT_MAX_FRAME_BYTES = 100 * 1024 * 1024`). Frames exceeding the cap drop the connection.
- **Multiplexing:** every frame includes an optional top-level `sessionId`. Root session = `""`. The reader actor demuxes by `sessionId` and fans out to per-target mpsc channels (cap 1024 per D16).

---

## Envelope shape (JSON-RPC 2.0)

All three transports speak JSON-RPC 2.0 with identical envelope rules. CDP is JSON-RPC 2.0 too, just with Chromium-specific `method` namespaces.

### Request

```json
{"jsonrpc": "2.0", "id": <number-or-string>, "method": "<dotted.name>", "params": <object-or-array>}
```

### Success response

```json
{"jsonrpc": "2.0", "id": <matching-id>, "result": <object>}
```

### Error response

```json
{"jsonrpc": "2.0", "id": <matching-id>, "error": {"code": <int>, "message": "<symbol>", "data": <object?>}}
```

### Notification (server → client push)

```json
{"jsonrpc": "2.0", "method": "event/notify", "params": {"topic": "<topic>", "session_id": "<id>", "tab_id": "<id?>", "payload": <object>}}
```

Notifications carry no `id`. Clients must not respond.

---

## Handshake

The first call after a fresh broker-socket connection MUST be `session.register`. The broker rejects every other method on an unregistered connection with `-32011 BrokerUnavailable`.

### `session.register`

```json
→ {"jsonrpc":"2.0","id":1,"method":"session.register","params":{
     "client_name":"one-for-all-mcp",
     "client_version":"1.0.0",
     "capabilities":["tools","events","binary-topics","storage_state"]
   }}

← {"jsonrpc":"2.0","id":1,"result":{
     "session_id":"s_b3f8a",
     "broker_version":"1.0.0",
     "supported_methods":["browser.context.create", …],
     "supported_events":["network.request", …]
   }}
```

`session_id` is opaque and broker-assigned. The MCP server stores it for the lifetime of the connection.

### `session.unregister`

Advisory. Idle teardown is driven by socket disconnect (D18), not by this call.

```json
→ {"jsonrpc":"2.0","id":2,"method":"session.unregister","params":{}}
← {"jsonrpc":"2.0","id":2,"result":{"closed":true}}
```

---

## Six representative round-trips

These are the exact frames the broker emits. Use them for fixture tests.

### 1. `tab.open` — happy path

```json
→ {"jsonrpc":"2.0","id":3,"method":"tab.open","params":{
     "url":"https://example.com",
     "wait_until":"load",
     "timeout_ms":30000
   }}

← {"jsonrpc":"2.0","id":3,"result":{
     "tab_id":"t_91",
     "target_id":"E2A1F4…",
     "frame_id":"F00…",
     "url":"https://example.com/",
     "title":"Example Domain"
   }}
```

### 2. `page.snapshot` — AX tree, indexed

```json
→ {"jsonrpc":"2.0","id":4,"method":"page.snapshot","params":{"tab_id":"t_91"}}

← {"jsonrpc":"2.0","id":4,"result":{
     "snapshot_seq":1,
     "url":"https://example.com/",
     "title":"Example Domain",
     "elements":[
       {"index":0,"ref":"e0","role":"link","name":"More information...","bbox":{"x":146,"y":182,"w":151,"h":18},"interactable":true},
       {"index":1,"ref":"e1","role":"heading","name":"Example Domain","bbox":{"x":120,"y":80,"w":400,"h":40},"interactable":false}
     ],
     "tree":{"…":"condensed"},
     "console":[],
     "exceptions":[],
     "network":{"in_flight":0,"completed_since_last":4,"failed_since_last":0},
     "focused_ref":null,
     "viewport":{"w":1280,"h":800,"scroll_x":0,"scroll_y":0,"device_scale_factor":2.0}
   }}
```

The `console`/`exceptions`/`network`/`focused_ref`/`viewport` fields are SPEC §10 M1 augmentations. They are always present.

For the delta path (M2), pass `since_seq`:

```json
→ {"jsonrpc":"2.0","id":4,"method":"page.snapshot","params":{"tab_id":"t_91","since_seq":3}}
```

The reply has the same shape but `elements[]` only contains entries whose `ref` mutated since `snapshot_seq=3`. `snapshot_seq` is monotonic across calls and never reused per `(session_id, tab_id)`.

### 3. `page.click` — by `ref` from snapshot

```json
→ {"jsonrpc":"2.0","id":5,"method":"page.click","params":{
     "tab_id":"t_91",
     "ref":"e0",
     "button":"left",
     "click_count":1,
     "realistic":false
   }}

← {"jsonrpc":"2.0","id":5,"result":{
     "ok":true,
     "navigation":{"frame_id":"F00…","url":"https://www.iana.org/help/example-domains"}
   }}
```

`ref` scope is `(tab_id, snapshot_seq)`. Reusing a `ref` from a snapshot earlier than the latest one for that tab returns `-32004 ElementStale`. The client is expected to re-snapshot and retry.

`realistic` defaults: headless = `false` (fast — single `Input.dispatchMouseEvent`), headed = `true` (Bezier-path mouse traversal). Per SPEC §10 M6.

### 4. `browser.context.create`

```json
→ {"jsonrpc":"2.0","id":6,"method":"browser.context.create","params":{
     "label":"work",
     "persist":true,
     "stealth":true,
     "trace":false
   }}

← {"jsonrpc":"2.0","id":6,"result":{
     "context_id":"c_4d2",
     "label":"work",
     "persist":true
   }}
```

In v1 (per SPEC D2) `context_id == session_id`: contexts are 1:1 with Chromium processes. The API preserves a future shared-Chromium mode where contexts can vary per call.

`stealth` and `trace` are session-wide toggles in v1. `trace:true` takes effect immediately for the bound session; `stealth` remains fixed from session launch, so passing a different value here is accepted for forward compatibility but does not reconfigure already-launched pages.

### 5. Error — `tab.open` when Chromium fails to launch

```json
→ {"jsonrpc":"2.0","id":7,"method":"tab.open","params":{"url":"https://example.com"}}

← {"jsonrpc":"2.0","id":7,"error":{
     "code":-32008,
     "message":"ChromiumLaunchFailed",
     "data":{"reason":"ProcessSingleton lock held","retry_after_ms":1000}
   }}
```

`message` is always the symbol name; `data` carries the structured detail. Clients should pattern-match on `code`, never on `message` text.

### 6. Notification — `network.request` (server → client push)

```json
← {"jsonrpc":"2.0","method":"event/notify","params":{
     "topic":"network.request",
     "session_id":"s_b3f8a",
     "tab_id":"t_91",
     "payload":{
       "subscription_id":"s_1",
       "request_id":"R_55",
       "url":"https://example.com/style.css",
       "method":"GET",
       "timestamp":1718450123.456
     }
   }}
```

### 7. Terminal PTY round-trip — `term.spawn` → `term.read`

```json
→ {"jsonrpc":"2.0","id":8,"method":"term.spawn","params":{
     "shell":"/bin/sh",
     "cwd":"/Users/elijah/Documents",
     "cols":80,
     "rows":24,
     "env":{"TERM":"xterm-256color"}
   }}

← {"jsonrpc":"2.0","id":8,"result":{
     "session_id":"term_1",
     "rows":24,
     "cols":80
   }}

→ {"jsonrpc":"2.0","id":9,"method":"term.read","params":{
     "session_id":"term_1",
     "max_bytes":4096
   }}

← {"jsonrpc":"2.0","id":9,"result":{
     "bytes":18,
     "data_base64":"aGVsbG8gZnJvbSBwdHkNCg==",
     "text":"hello from pty\r\n",
     "eof":false,
     "dropped_bytes":0
   }}
```

`term.read` always returns `data_base64`; `text` is present only when the buffered bytes decode cleanly as UTF-8.

---

## Error codes (SPEC D17 / §2)

The broker uses the JSON-RPC server-error band `-32000..-32099`. Stable codes mean clients pattern-match without parsing strings.

| Code | Symbol | Meaning |
|------|--------|---------|
| -32001 | `SessionNotFound` | `session_id` unknown |
| -32002 | `TabNotFound` | `tab_id` unknown or closed |
| -32003 | `ContextNotFound` | `context_id` unknown |
| -32004 | `ElementStale` | `ref` from older snapshot than current |
| -32005 | `ElementNotActionable` | element hidden, disabled, or zero-area |
| -32006 | `NavigationFailed` | `net::ERR_*`, timeout, or blocked |
| -32007 | `Timeout` | wait predicate exceeded `timeout_ms` |
| -32008 | `ChromiumLaunchFailed` | spawn or pipe handshake failed |
| -32009 | `PermissionDenied` | tool gated; user opted out |
| -32010 | `ProtocolError` | malformed params or schema violation |
| -32011 | `BrokerUnavailable` | MCP couldn't reach broker after backoff |
| -32012 | `SessionLimitExceeded` | concurrent session cap reached |

Standard JSON-RPC codes (`-32600` invalid request, `-32601` method not found, `-32602` invalid params, `-32603` internal error, `-32700` parse error) apply normally — the broker emits them when it sees them.

The mapping is implemented exactly once, at `crates/broker/src/protocol.rs:78-111`. Adding a new code requires editing that file and this table together.

---

## Event topics

Pushed by the broker as `event/notify`. Subscribers register implicitly via `session.register` capabilities `["events"]`.

| Topic | Trigger | `payload` shape |
|-------|---------|------------------|
| `console.message` | `Runtime.consoleAPICalled` | `{level, text, source, ts_ms, stack?}` |
| `network.request` | `Network.requestWillBeSent` | `{subscription_id, request_id, url, method, timestamp, synthetic?}` |
| `network.response` | `Network.responseReceived` | `{subscription_id, request_id, status, headers, mime_type, timestamp}` |
| `network.websocket` | `Network.webSocket*` | `{subscription_id, request_id, kind, ts_ms, payload_base64?, url?, error?}` |
| `network.eventsource` | `Network.eventSourceMessageReceived` | `{subscription_id, request_id, event_name, event_id, data, ts_ms}` |
| `page.exception` | `Runtime.exceptionThrown` | `{text, stack, ts_ms}` |
| `session.recovered` | M4 — Chromium crashed within activity window, broker respawned | `{}` |
| `broker.shutdown` | broker draining; clients should disconnect | `{}` |
| `vision.frame` | continuous vision screencast event | `{session_id, tab_id, seq, captured_us, viewport, frame, changed_tiles, ocr_delta, stability?, state?}` |
| `term.output` | PTY output chunk buffered for a terminal session | `{term_session_id, seq, bytes, data_base64, text?, dropped_bytes, eof}` |
| `term.exit` | PTY child exited or was closed | `{term_session_id, exit_code?, signal?, exited}` |
| `app.event` | AX subscription update from `app.subscribe` | `{subscription_id, app_id, event, element?, ts_ms}` |
| `system.fsevents` | filesystem event from `system.fsevents.watch` | `{watch_id, path, flags, event_id, ts_ns}` |

---

## Lifecycle

### Connect

1. Client opens `~/.one-for-all/broker.sock` (loopback if remote-broker mode is enabled — see `Roadmap`).
2. Client sends `session.register`. Broker responds with `session_id`.
3. Client may now call any method in `supported_methods`.

### Idle

Per SPEC D18, the broker shuts a session down when its socket disconnects, **not** when `session.unregister` is called. There is a 5-minute draining window before the per-session Chromium child is killed. A client that reconnects with `session.register {session_id: "..."}` during that window rebinds to the live session; a fresh `session.register` with no `session_id` creates a new session id instead. Any PTY sessions owned by that broker session are torn down alongside Chromium on `session.unregister`, idle drain, or broker shutdown.

### Tab persistence (D11)

Tabs survive every kind of broker-side death because `--user-data-dir=~/.one-for-all/sessions/<session_id>/` persists them at the OS level. On crash recovery the broker respawns Chromium against the same UDD and re-enumerates targets via `Target.getTargets`; on a normal client reconnect, the live broker session is rebound by passing `session.register {session_id}`.

### Crash recovery (M4)

If the Chromium child exits non-cleanly within 30 s of last activity, the broker respawns against the same UDD, restores tabs via Chrome's session-restore, and pushes `event/notify { topic: "session.recovered" }`. Implemented in `crates/broker/src/recovery.rs`.

### Drop order on broker shutdown (SPEC §3)

1. Stop accepting new connections.
2. Send `event/notify { topic: "broker.shutdown" }` to every session.
3. For each `Browser`: graceful CDP `Browser.close` → 5 s wait → `child.kill()`.
4. fsync log files; flush metrics.
5. Release flock on `~/.one-for-all/broker.lock`; unlink socket.

---

## Wire-level guarantees

- **Ordering.** Within a single connection, requests are matched to responses by `id`. The broker may interleave responses; the client must not assume reply-order matches request-order.
- **Notifications never block requests.** The writer task drains a single mpsc; requests have higher priority on backpressure (drop-oldest on the notification channel before slowing requests).
- **Backpressure (D16, §10).** Per-target mpsc cap = 1024; network-observe = 4096; page-lifecycle = 64; console = 512. When full, oldest entries drop with a `tracing::warn!` and a metric increment; the broker never blocks the CDP reader.
- **Cancellation.** Every `pub async fn` on the wire path is cancellation-safe — the client may drop a future at any `.await` without leaving the broker in a wedged state. Per-fn annotation: `// CANCELLATION: safe | unsafe | conditional` (SPEC §10).

---

## How a client should fail

- **`-32004 ElementStale`** → re-snapshot, then retry. Never retry the same `ref` blindly.
- **`-32007 Timeout`** → re-evaluate the wait predicate. Possibly raise `timeout_ms`, or switch to event-driven via `event/notify` topics.
- **`-32008 ChromiumLaunchFailed` with `data.reason == "ProcessSingleton lock held"`** → wait `data.retry_after_ms` then retry. The broker will sweep stale singleton locks at next start (R3).
- **`-32011 BrokerUnavailable`** → reconnect with exponential backoff (50/100/200/400/800/1600 ms, total ~3s). The MCP server's `broker_client` does this for you (see `crates/mcp-server/src/broker_client.rs::CONNECT_BACKOFF_MS`).
- **`-32012 SessionLimitExceeded`** → bound your concurrent sessions to `max_sessions` (default 16). There is no auto-queue; the call returns immediately.

---

## Roadmap (out of scope for v1.0.0)

- HAR export from `net.observe` history.
- Codegen mode (record agent actions as Playwright/Puppeteer script).
- WebAuthn virtual authenticator.
- Service worker debugging surface.
- Headed↔headless live swap on a running session.
- Trace viewer UI (web-based replay against the M10 trace JSONL).

---

**END PROTOCOL.md v1.0.0**
