# one-for-all — ARCHITECTURE.md

**Audience:** new contributors, system reviewers, anyone deciding whether to deploy this on a fleet.
**Requirement IDs.** Identifiers such as `D16`, `M1`, `V4`, `R10`, and `N34` used throughout this document and in code comments refer to the project's internal specification, which is not published. They are stable and appear verbatim at the call sites they constrain.
**Version:** 1.0.0

---

## What this is

A standalone, owned-end-to-end browser-automation stack for Claude Code on macOS.

- Multiple concurrent `claude` sessions each own their own Chromium tabs.
- True parallel — sessions never block on each other.
- Zero focus-steal from the user's foreground app, by construction.
- Per-session full OS-level isolation: cookies, IndexedDB, service workers, GPU shader cache — nothing crosses session boundaries because nothing crosses process boundaries.
- Tabs persist across session exits.
- No dependence on the Anthropic Chrome extension. No copy-paste pairing.
- Innate integration: install once → every `claude` session in any terminal automatically gets browser tools.

---

## High-level shape

```
┌────────────────┐    LSP framed JSON-RPC    ┌─────────────────┐
│   claude CLI   │◀──────── stdio ────────▶ │   MCP server    │
│   (per term)   │                           │ (per CLI session)│
└────────────────┘                           └────────┬─────────┘
                                                      │ unix socket
                                                      │ ~/.one-for-all/broker.sock
                                                      │ JSON-RPC, line-delim
                                                      ▼
                                             ┌─────────────────┐
                                             │     broker      │
                                             │ (singleton,     │
                                             │  flock-elected) │
                                             └────────┬─────────┘
                                                      │ in-process
                                                      ▼
                              DashMap<SessionId, Arc<Browser>>
                                  │           │           │
                          ┌───────┘     ┌─────┘     ┌─────┘
                          ▼             ▼           ▼
                    ┌─────────┐   ┌─────────┐ ┌─────────┐
                    │ Browser │   │ Browser │ │ Browser │   ← per-session
                    │ (sess A)│   │ (sess B)│ │ (sess C)│
                    └────┬────┘   └────┬────┘ └────┬────┘
                         │ pipe        │           │
                         ▼             ▼           ▼
                    ┌─────────┐   ┌─────────┐ ┌─────────┐
                    │Chromium │   │Chromium │ │Chromium │   ← own UDD,
                    │  (UDD A)│   │  (UDD B)│ │  (UDD C)│     own --remote-
                    └─────────┘   └─────────┘ └─────────┘     debugging-pipe
                         ▲
                         │   side bus: focus-guardian actor (NSWorkspace observer)
```

Three transports. Three framings. One `BrowserContext` per session = one Chromium child = one OS user-data-dir.

For framing details see `docs/PROTOCOL.md`.

---

## Component map

| Crate | Owns | Threading | Lifecycle |
|---|---|---|---|
| `chromium-fetcher` | CfT version manifest, downloaded zip, extracted binary at `~/.one-for-all/chromium/<rev>/` | sync API; internal tokio for parallel chunks | called once per cold broker start |
| `cdp-client` | Pipe transport per Chromium child, codegened CDP bindings, per-target sessions | reader actor + writer actor + per-session demux | lifetime of one Chromium process |
| `ax-engine` | Snapshot algorithm: AX+DOM merge, stable refs, iframe recursion, MutationObserver delta | pure async fns, no global state | per `page.snapshot` call |
| `focus-manager` | macOS spawn-without-focus-steal, frontmost save+restore actor | guardian actor on a tokio task | per Chromium spawn |
| `browser-engine` | Browser singleton-per-session, BrowserContext, Page, action dispatch, wait predicates, stealth bundle, network/locale emulation | per-Browser tokio runtime sub-tree | per session |
| `terminal-control` | PTY-backed terminal sessions, bounded raw output ring, parser-maintained screen snapshots, scrollback, resize/signal/mouse injection | blocking PTY reader + async exit watcher per terminal | per broker session |
| `broker` | unix socket server, `SessionRegistry`, JSON-RPC router, event sink, lifecycle FSM, crash recovery, trace recorder | accept loop + per-conn task + per-session task + crash watcher | daemon |
| `mcp-server` | stdio MCP loop, tool dispatch, `broker_client` (auto-spawn-on-missing) | per stdio: 1 reader + 1 writer + N tool tasks | per CLI session |
| `installer` | `install.sh`, plist, atomic `~/.claude.json` merge, `doctor.sh` | shell scripts | one-shot |
| `observability` | `tracing-subscriber`, log dirs, metrics registry | sync init, async metric writers | embedded in broker + mcp-server |

---

## Why per-session Chromium (D2 / D3 / D11)

The user requirement "tabs persist across session exits" forces real Chromium-level persistence via `--user-data-dir`. `BrowserContext` + storage-state-restore can re-create cookies, but cannot reproduce open tabs at next attach. So one Chromium child per session is the only design that meets the spec.

Consequences:

- **Storage isolation is OS-level, not cooperative.** Cookies, IndexedDB, CacheStorage, Service Workers, permissions, downloads, certificate cache, font cache, GPU shader cache — none of them cross process boundaries.
- **RAM cost is ~80MB per session** — accepted under the "highest quality, no shortcuts" mandate. With the default cap of 16 concurrent sessions that's ~1.3GB upper bound.
- **Idle shutdown is by socket disconnect** (D18). 5-minute draining window before the Chromium child is killed. Robust to crashed MCP servers, dropped pipes, OS cleanup.
- **No shared Chromium**, but the API (`browser.context.create`, `browser.context.list`, `browser.context.destroy`) is shaped to support a future shared-Chromium mode without breaking clients.

---

## Why broker-via-flock (D7)

Multiple `claude` CLI sessions in different terminals all want to drive Chromium. Their MCP servers can't each spawn their own Chromium pool — that's no parallelism, that's chaos.

Solution: opportunistic singleton.

1. The first MCP server tries to acquire `LOCK_EX | LOCK_NB` on `~/.one-for-all/broker.lock`.
2. If it wins, it becomes the broker — same binary, different runtime role.
3. If it loses, it connects to `~/.one-for-all/broker.sock` as a client.

No separate launchd entity is required, but the installer wires a launchd plist for graceful daemon mode. No port collisions, no localhost firewall, no WS framing.

---

## Focus discipline (SPEC §5)

Five layers. Each holds even if the next fails.

**Layer A — Headless by default.** D1: `--headless=new`. No `NSWindow` is ever created. Zero focus contention by definition. This handles the common case.

**Layer B — Headed launch flags.** When the caller explicitly asks for headed:
- `/usr/bin/open -gj /path/to/Chromium.app --args <flags>` — `-g` opens without bringing the app to foreground; `-j` hides on launch.
- Chromium flags: `--no-startup-window` (we open windows ourselves via `Target.createTarget`), `--silent-launch`, `--window-position=-32000,-32000` (offscreen until raised), `--window-size=1280,800`.

**Layer C — Frontmost save + restore.** Implemented in `focus-manager` via `objc2` + `objc2-app-kit`:
1. Pre-spawn: capture `(pid, bundle_id)` from `NSWorkspace.sharedWorkspace.frontmostApplication`.
2. Spawn child Chromium.
3. Post-spawn guardian task: for 3 seconds at 50ms ticks, if `frontmostApplication.processIdentifier != captured_pid`, call `NSRunningApplication.runningApplicationWithProcessIdentifier(captured_pid).activateWithOptions(0)` to restore. Options = 0 — explicitly NOT `NSApplicationActivateAllWindows` (that's a forbidden API).
4. Bounded retries; gives up gracefully (logged) if the user has switched apps deliberately.
5. Runs as a `tokio::spawn` task; never blocks the caller.

**Layer D — Tab activation discipline.** `Page.bringToFront` is the ONLY way to surface a tab. `Target.activateTarget` is forbidden codebase-wide; `clippy::disallowed_methods` enforced in CI.

**Layer E — Broker process activation policy.** The broker's `main.rs` calls `NSApp.setActivationPolicy(NSApplicationActivationPolicy::Accessory)` at startup so the broker itself never appears in the Dock or steals focus.

### Forbidden APIs (lint-enforced)

- `NSApp.activateIgnoringOtherApps:YES`
- `NSApp.activate(options: NSApplicationActivateAllWindows)` on **our own** process
- `Target.activateTarget` (CDP)
- `NSWindow.makeKeyAndOrderFront:` on **our own** windows

The lint config lives in `clippy.toml` at the workspace root (`disallowed-methods = [...]`). Adding any of these without removing the lint is a CI fail.

---

## Snapshot algorithm (SPEC D14, M1, M2)

`page.snapshot` is the agent's primary perception primitive. Two-stage:

### Full snapshot

1. Call `Accessibility.getFullAXTree` → AX tree with names, roles, states.
2. Call `DOMSnapshot.captureSnapshot` with `includeDOMRects: true` → bounding boxes and computed styles.
3. Merge: each AX node enriched with its DOM bbox + interactability hints from `pointer-events`, `display`, `visibility`.
4. Walk iframes recursively (via `IO.read` on the OOPIF doc). Each iframe contributes nodes with their own `frame_id`.
5. Stable ref per element: `ref = "e" + sha256(role | 0x1F | name | 0x1F | parent_role | 0x1F | sibling_index_within_same_role)[..6]`. Hash collisions are allowed; the `index` field disambiguates within a single snapshot. The `0x1F` separator (US in ASCII) avoids collisions when names contain `|`.
6. Augment with M1 fields: `console` (last 50 messages since previous snapshot), `exceptions` (since previous snapshot), `network` (`{in_flight, completed_since_last, failed_since_last}`), `focused_ref`, `viewport`.

### Delta snapshot (M2)

`page.snapshot {since_seq: N}` returns only elements that changed since `snapshot_seq=N`.

1. A `MutationObserver` is bootstrapped on every new document via `Page.addScriptToEvaluateOnNewDocument`. It writes records into `window.__oneForAllMutationLog`.
2. The broker drains the log on each delta call via `Runtime.evaluate`, scoped per `(tab_id, snapshot_seq)`.
3. The result is a `Snapshot` with the same shape as a full one, but `elements[]` only contains nodes whose `ref` mutated.
4. `snapshot_seq` is monotonic. The broker stores per-`(session, tab)` last-seq for tombstone resolution (closed elements between snapshots).

Implementation: `crates/ax-engine/src/{merge.rs, mutation.rs}` and `crates/browser-engine/src/snapshot.rs`.

---

## Click resolution (SPEC D15)

LLM passes `ref:"e<N>"` from the latest snapshot.

1. Broker validates ref scope `(tab_id, snapshot_seq)`. Stale → `-32004 ElementStale`.
2. Resolves `{x, y}` from snapshot bbox center (with paint-order tie-break — the spec mandates click order, so element Z-order is consulted via `DOMSnapshot.captureSnapshot {includePaintOrder: true}`).
3. Verifies actionability: not disabled, not zero-area, not occluded. Failure → `-32005 ElementNotActionable`.
4. Dispatches via `Input.dispatchMouseEvent`.
   - `realistic: false` → single mouse event at the point. Default headless.
   - `realistic: true` → Bezier-path mouse traversal across multiple `Input.dispatchMouseEvent` frames. Default headed. Defeats bot detection that watches mouse paths.
5. If the click triggers navigation, returns `{ok: true, navigation: {frame_id, url}}`; otherwise `{ok: true}`.

---

## Stealth bundle (SPEC §10 M3)

Injected at every new document via `Page.addScriptToEvaluateOnNewDocument`. Per-context seed keys deterministic noise.

Patches:
- `navigator.webdriver` → `undefined`
- `navigator.plugins` → realistic populated array
- `navigator.languages` → from request
- `chrome.runtime` → present
- `Notification.permission` → `default`
- canvas / WebGL noise injection (per-context seed)
- `RTCPeerConnection` IP leak prevention (force `relay`-only)

Toggle: `browser.context.create {stealth: true}` (default `true`).

Implementation: `crates/browser-engine/src/{stealth.rs, context.rs}`.

---

## Crash recovery (SPEC §10 M4)

The broker subscribes to each Chromium child's exit signal via `tokio::process::Child::wait`.

1. If the child exits non-cleanly within 30 s of last activity (`RESPAWN_ACTIVITY_WINDOW_MS = 30_000`), trigger respawn.
2. Respawn against the same `--user-data-dir` so tabs restore via Chrome's session-restore.
3. Re-enumerate targets via `Target.getTargets`; remap existing tabs to new internal `tab_id`s.
4. Push `event/notify { topic: "session.recovered", session_id, payload: {} }`.
5. If the child exited cleanly (or outside the activity window), drain the session — the user is presumed done.

Implementation: `crates/broker/src/recovery.rs`.

---

## Trace recording (SPEC §10 M10)

When `browser.context.create {trace: true}` is set, the broker writes structured replay material to `~/.one-for-all/sessions/<id>/trace/<seq>.jsonl`:

- Every CDP request and response (with frame timestamps).
- A screenshot per agent-initiated action.
- A DOM snapshot every 500 ms during agent activity.

The format is JSONL — one record per line, `{ts_ms, kind, payload}`. Replay tools can rebuild the page state and the agent's view at any timestamp.

Implementation: `crates/broker/src/trace.rs` (post-fix).

---

## Resource limits (SPEC §10 M9)

Each Chromium child is wrapped at spawn with:
- `setrlimit(RLIMIT_AS, {soft: 4 GiB, hard: 4 GiB})`
- `setrlimit(RLIMIT_CPU, {soft: 3600 s})`

The hard cap prevents an agent loop from OOMing the host; the soft CPU cap means a runaway tab raises `SIGXCPU` rather than starving the system. Implementation: `crates/browser-engine/src/browser.rs:284-300`.

The broker itself does NOT set RLIMIT — it's the parent and needs to live.

---

## Threading and channels (SPEC D16)

Tokio multi-thread runtime. One actor per concern:

- pipe reader (per Chromium child)
- pipe writer (per Chromium child)
- per-target router (one per CDP session)
- per-session broker handler
- MCP stdio reader + writer
- focus guardian
- crash watcher
- (when M10 enabled) trace recorder

Channel discipline (all bounded — `mpsc::unbounded_channel` is forbidden):

| Use | Cap |
|---|---|
| per-target CDP fan-out | 1024 |
| network observe push | 4096 |
| page lifecycle events | 64 |
| console messages | 512 |
| broker → MCP push (per session) | 256 |
| MCP → broker outbound | 256 |

Backpressure policy: drop-oldest with a `tracing::warn!` and a metric increment. Never block the CDP reader.

---

## Drop order on broker shutdown (SPEC §3)

1. Stop accepting new connections.
2. Send `event/notify { topic: "broker.shutdown" }` to every session.
3. For each `Browser`: graceful CDP `Browser.close` → 5 s wait → `child.kill()`.
4. fsync log files; flush metrics.
5. Release flock on `~/.one-for-all/broker.lock`; unlink socket.

Implementation: `crates/broker/src/lifecycle.rs`.

---

## On-disk layout

```
~/.one-for-all/
├── broker.lock                    # flock-elected singleton
├── chromium/
│   └── <rev>/                     # per-revision install
│       └── .complete              # marker; presence == fully extracted
├── sessions/
│   └── <session_id>/              # one Chromium UDD per session
│       └── trace/
│           └── <seq>.jsonl        # M10 trace records (when {trace: true})
└── log/
    ├── broker.log
    ├── mcp-<pid>.log
    └── metrics.prom
```

The installer creates `~/.one-for-all/`, writes the launchd plist to `~/Library/LaunchAgents/io.github.elijahumana.one-for-all.plist`, and atomically merges the MCP server entry into `~/.claude.json` via `jq → tmp → mv -f` (R10). The installer never touches `NativeMessagingHosts` (R11).

---

## Risks and mitigations (SPEC §8)

| # | Risk | Mitigation |
|---|---|---|
| R1 | `--headless=new` ≠ legacy `headless_shell` for some PDF/print paths | Stick with `=new`; document; fallback to old shell only if a tool requires it. |
| R2 | macOS 14+ may ignore `NSRunningApplication.activate` without user-initiated context | Layer A + offscreen `--window-position` are primary; Layer C restore is best-effort. |
| R3 | `ProcessSingleton` lock leakage on Chromium crash | Sweep stale `SingletonLock` files in UDDs at broker start; on session-attach if held by dead pid, remove. |
| R4 | CfT lags Stable by 1-2 versions | Pin to last-known-good Stable; allow `rev` override. |
| R5 | Builder name-drift across crates | SPEC is byte-exact; reviewer-finisher hunts drift in T7. |
| R6 | `Target.activateTarget` accidentally called → focus regression | `clippy::disallowed_methods` lint in CI. |
| R7 | AX-tree refs stale across navigation | Scoped `(tab_id, snapshot_seq)`; reuse → `-32004`. |
| R8 | Pipe write half-buffer overflow if Chromium stalls reading | Bounded per-target=1024 mpsc; drop-oldest logged; surface `-32007` after timeout. |
| R9 | 16 sessions × ~80MB = 1.3GB RAM | Document; idle-shutdown 5min after disconnect; `-32012` cap. |
| R10 | `~/.claude.json` clobber by installer | Atomic merge: jq → tmp → python3 json.tool validate → `mv -f`. `doctor.sh` checks before/after diff. |
| R11 | `NativeMessagingHosts` entries for unrelated extensions corrupted | Installer never touches those paths. |
| R12 | Broker single-instance race | `flock(broker.lock)` with `LOCK_EX \| LOCK_NB`; loser becomes client. |
| R13 | Stuck-Write process bricks (lost ~3h on architect run) | Builders chunk Write calls; max ~1500 lines per call. T7 reviewer inspects file lengths. |

---

## How a contributor adds a new tool

1. Add the dotted method name to SPEC §7 with params, result, and error codes.
2. Add the dispatch arm in `crates/broker/src/router.rs::dispatch`.
3. Add the implementing free function (e.g. `page_my_tool`) below.
4. Add the schema in `crates/mcp-server/src/schema.rs` (schemars-derive).
5. Wire it in the `desc!` registry in `crates/mcp-server/src/tools.rs`.
6. Add a fixture round-trip to the e2e suite at `installer/e2e-smoke.sh`.
7. Add a docs row to `docs/PROTOCOL.md` event/method tables.

The reviewer-finisher checklist hunts for tools that exist in one of the seven sites but not the rest — drift between mcp-server's schema and broker's dispatch is the most common bug.

---

## Why we beat each named competitor (SPEC §10 architectural moats)

| vs | Their weakness | Our improvement |
|---|---|---|
| Playwright | Locator re-resolves every action — slow, races on dynamic DOM | Stable `ref` from snapshot, `ElementStale -32004` on drift — agent gets explicit failure not silent retarget |
| Puppeteer | Single-browser, cooperative isolation only | Per-session Chromium = OS-level isolation |
| chromedp | Single-target focus, no AX surfacing | Full AX+DOM merge with stable refs |
| browser-use | Index shifts on every snapshot | sha256-stable refs across snapshots |
| Anthropic ext | WebSocket bridge, copy-paste pairing, cooperative tabs | Native pipe, zero pairing, OS-isolated sessions |
| Selenium BiDi | W3C-portable but no AX-tree primitive | Tool surface uses BiDi-style names but exposes richer AX |
| browserless | Container-per-session = heavy | Process-per-session = lightweight |
| OpenAI Operator | Cloud-VM-per-task = no local control | Local Chromium-per-session = full control |
| ChatGPT Atlas | Forked Chromium, opaque | Stock Chrome-for-Testing, transparent |

---

**END ARCHITECTURE.md v1.0.0**
