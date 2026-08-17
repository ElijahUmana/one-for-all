# one-for-all — ARCHITECTURE.md

**Audience:** new contributors, system reviewers, anyone deciding whether to deploy this on a fleet.
**Requirement IDs.** Identifiers such as `D16`, `M1`, `V4`, `R10`, and `N34` used throughout this document and in code comments refer to the project's internal specification, which is not published. They are stable and appear verbatim at the call sites they constrain.
**Version:** 1.0.0

---

## What this is

A machine-control plane for AI agents on macOS, owned end to end. Not a browser driver — a broker that spans six control planes behind one JSON-RPC contract.

| Plane | Mechanism | Families |
|---|---|---|
| Browser | CDP over `--remote-debugging-pipe`, one Chromium child per session | `browser` `tab` `page` `net` |
| Native apps | macOS Accessibility API, plus AppKit/CGS surfaces AX alone cannot reach | `app` `clipboard` `drag` |
| Terminal | Real PTY with a parser-maintained screen model | `term` |
| System | CoreAudio, IOKit, AVFoundation, FSEvents, SystemConfiguration, libproc | `system` |
| Vision | Continuous capture, diffing, detection over a shared-memory frame ring | `vision` |
| Isolation | APFS clone per session under `sandbox-exec`, three-way merge back | — |

The invariants that hold across all six:

- **Multiple concurrent agent sessions run truly in parallel.** Sessions never block on each other.
- **Zero focus-steal from the user's foreground app, by construction** — five layers of defence, with the focus-stealing AppKit surface confined to one crate and enforced at compile time.
- **Per-session OS-level isolation.** Cookies, IndexedDB, service workers, GPU shader cache, PTYs and spawned processes do not cross session boundaries, because they do not cross process boundaries.
- **Nothing is lost silently.** Every skipped facet, dropped event, truncated walk, degraded capability and stale observation is reported to the caller rather than rendered as a plausible-looking empty result. See "No silent loss" below.
- **Browser tabs persist across session exits**, via real `--user-data-dir` persistence rather than cooperative state restore.
- **Install once** → every agent session in any terminal gets the full surface, with no pairing and no per-project configuration.

### Where the isolation boundary actually ends

The isolation boundary is genuine for the filesystem, browser storage, processes, PTYs, frame rings, traces, and accessibility scope.

It is **not** genuine — and cannot be made genuine on macOS — for the frontmost application, the hardware cursor, the general pasteboard, the WindowServer window list, the audio output device, or TCC grants. Those are global and mutable. They are not isolated; they are **arbitrated**. Any claim of "full isolation" that does not carve them out is false.

### No silent loss

The failure this design treats as worst is not an error but a convincing empty result: an agent handed `[]` concludes the screen is empty and acts on it.

- A capture that could not be taken is reported as an explicit skip with a reason — never omitted.
- A bounded channel dropping under backpressure counts the drop at the producer **and** surfaces it on the next delivered item.
- A capability is supported, unsupported with a reason, or degraded with a remedy. There is no fourth state and no silence.
- A surface that died returns an explicit gone marker, not an empty success.
- An AX walk hitting its depth or node ceiling sets `truncated_at` and warns.
- Image and structure in one snapshot share a generation and one atomic publish point; a facet that could not be captured at that instant is marked skipped rather than stitched in from another moment.

Ring writes go to a temp path and `rename()` into place — atomic on APFS — so a reader never sees a partial write, and the trailing raw history stays on disk as evidence. Truncation in the hot payload is a deferral, not a deletion.

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
| `native-control` | macOS Accessibility surface: windows, menus, Dock, Spaces, Spotlight, Notification Center, status menu, IME, AppleScript/JXA, clipboard, cross-app drag | per-subscription run-loop thread; sync AX calls with an explicit messaging timeout | per watched app |
| `system-control` | Audio in/out and capture, mic, camera, battery, Bluetooth, USB, displays and region capture, network interfaces/routes/connections, process list/info/signal, FSEvents, Spotlight metadata | sync framework calls; FSEvents stream thread; permission-gated with a stub backend where unavailable | per call, plus long-lived watches |
| `vision` | Continuous capture, SIMD tile-hash diffing, OCR, detection and classification over an `mmap` frame ring | capture task + bounded work queue + pipeline task | per subscribed surface |
| `sandbox` | Per-session APFS clone, `sandbox-exec` profile generation, retained base snapshot, three-way merge back to host | sync clone; merge is an explicit operator action | per session |
| `broker` | unix socket server, `SessionRegistry`, JSON-RPC router, event sink, lifecycle FSM, crash recovery, trace recorder | accept loop + per-conn task + per-session task + crash watcher | daemon |
| `mcp-server` | stdio MCP loop, tool dispatch, `broker_client` (auto-spawn-on-missing) | per stdio: 1 reader + 1 writer + N tool tasks | per CLI session |
| `installer` | `install.sh`, plist, atomic `~/.claude.json` merge, `doctor.sh` | shell scripts | one-shot |
| `ofa-cli` | Operator CLI — `spawn`, `list`, `attach`, `merge`, `kill`, `logs` | one-shot, talks to the broker over the socket | per invocation |
| `bench` | Latency and throughput SLO gates, asserted rather than reported | criterion harness | CI |
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

## The native plane

Accessibility is the ground truth for a well-behaved AppKit or SwiftUI app: roles, values, available actions, hierarchy, geometry, text ranges, and change notifications. Where an element is semantically actionable, performing its action beats a synthetic click on every axis — no coordinates, no occlusion sensitivity, no focus steal, and it works on background and partially-offscreen windows.

Input therefore uses a ladder, not a single mechanism, ordered by focus impact:

1. **Perform the element's own accessibility action** — no coordinates, no focus change. The default.
2. **Per-pid event posting** — targeted input that does not activate the application.
3. **Global event tap** — moves the real cursor and is visible to the user. Last resort, and gated.

Several surfaces are not reachable from the accessibility API at all — Spaces and Mission Control, global window z-order, some menu-bar extras. Those go through documented AppKit and window-server queries, with AppleScript and JavaScript-for-Automation as the escape hatch for applications that expose a scripting dictionary and nothing else.

Every AX call carries an explicit messaging timeout. The default is long enough to wedge an agent loop, and a wedged target application blocks the calling thread with no cancellation primitive — so the timeout is set, always, and a timeout is reported as a timeout rather than as an empty tree.

## The terminal plane

A real PTY, not a pipe: programs that check `isatty` behave normally and full-screen applications work.

A blocking reader feeds a bounded raw ring while a parser maintains the screen. Callers ask for the rendered screen — cells, attributes, cursor, alternate-screen state — instead of replaying escape sequences themselves. Scrollback is a ring the parser maintains; resize goes through `TIOCSWINSZ`; signals are delivered to the foreground process group; xterm mouse sequences can be injected.

Two properties fall out of owning the PTY rather than scraping a terminal window:

- **A TUI becomes a first-class surface.** `vim`, `htop`, `less`, an interactive rebase and a curses installer are user interfaces, and the parsed grid is what makes them addressable rather than a wall of bytes.
- **Echo state is a reliable secret detector.** When the terminal disables echo, the prompt is a password prompt. That classification is free and no screen-scraping approach can match it.

Every PTY session inherits its session's sandbox policy.

## The system plane

Device and OS state has **no elements facet, and that is deliberate**. An agent querying an audio output device sees no click verb and no element tree, so it never tries. Declaring a surface non-UI is what keeps the unified contract from flattening into a lie.

Each domain maps to a specific framework — CoreAudio for devices and volume, AVFoundation for capture, IOKit for USB and power, CoreBluetooth for radios, SystemConfiguration and `libproc` for network and processes, FSEvents for filesystem watches, and the metadata query API for Spotlight. Screen capture uses the current ScreenCaptureKit path; the older CoreGraphics capture entry points are obsoleted, not merely deprecated, and do not build against a modern deployment target.

Permission-gated capabilities — camera, microphone, screen recording, Bluetooth — are checked without prompting where the platform allows it, and prompt on first use otherwise. One subtlety governs how permissions are granted at all: the system attributes a permission not to the process that called, but to the *responsible* process up the launch chain. A binary launched from a terminal inherits that terminal as responsible, so the grant lands on the terminal rather than on the tool.

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
