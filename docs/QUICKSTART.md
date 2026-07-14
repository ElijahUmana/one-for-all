# one-for-all — QUICKSTART.md

**Audience:** developer who just installed and wants to know what they can do in 12 lines.

---

## Install

```bash
git clone https://github.com/ElijahUmana/one-for-all ~/one-for-all
cd ~/one-for-all
./installer/install.sh
```

The installer:
- builds the workspace in release mode (`cargo build --release --workspace`)
- copies `one-for-all-broker` and `one-for-all-mcp` to `~/.one-for-all/bin/`
- installs `~/Library/LaunchAgents/io.github.elijahumana.one-for-all.plist`
- atomically merges an `mcpServers."one-for-all"` entry into `~/.claude.json` (no clobber — see R10)
- runs `doctor.sh` to verify everything before exiting

`./installer/uninstall.sh` reverses all of the above.
`./installer/doctor.sh` re-runs the post-install verification at any time.

---

## First call

Open any terminal. Run `claude`. Then ask:

> Open https://example.com and tell me what's on the page.

That's it. No pairing, no copy-paste, no per-session config. The MCP server in your `claude` process discovers the broker via `~/.one-for-all/broker.sock`, registers a session, gets a fresh Chromium child with its own `--user-data-dir`, opens the URL, snapshots the AX tree, and hands the result back to the model.

A second `claude` in another terminal does the same, with its own Chromium, its own cookies, no contention.

---

## The 12-line shape

This is what an agent loop looks like, end-to-end:

```
1. tab.open  {url: "https://example.com", wait_until: "load"}
2. page.snapshot {tab_id}                       # full AX + viewport + console + exceptions
3. page.click {tab_id, ref: "e7"}               # ref came from step 2's elements[]
4. page.snapshot {tab_id, since_seq: <prev>}    # delta — only changed elements
5. page.read_text {tab_id, ref: "e3"}
6. page.type {tab_id, ref: "e10", text: "..."}
7. tab.wait {tab_id, predicate: "networkidle"}
8. page.eval {tab_id, expression: "document.title"}   # requires eval capability (e.g. ONE_FOR_ALL_CAPABILITIES=eval)
9. page.cookies {tab_id, action: "get"}
10. page.screenshot {tab_id, format: "png"}
11. tab.close {tab_id}
12. session.unregister                          # advisory; disconnect would also do it
```

Every call is JSON-RPC 2.0 over the MCP stdio transport. Wire details: `docs/PROTOCOL.md`.

---

## What you get for free

- **Tabs persist across `claude` exits.** The next session attaches to the same `--user-data-dir`; tabs restore via Chrome's session-restore.
- **Crashes auto-recover** within a 30 s activity window — the broker respawns Chromium against the same UDD and pushes `event/notify { topic: "session.recovered" }`.
- **No focus-steal.** `--headless=new` by default → no NSWindow → zero contention. Headed mode adds offscreen `--window-position`, frontmost save+restore, and Layer E broker accessory-policy. See `ARCHITECTURE.md` §Focus.
- **Stealth defaults on.** `navigator.webdriver`, plugins, languages, canvas/WebGL noise, RTC IP-leak prevention. The current session is launched with stealth enabled by default; `browser.context.create` accepts `stealth` for future compatibility but does not reconfigure already-open pages in v1.
- **Per-session OS-level isolation.** Cookies, IndexedDB, and persisted service-worker state stay inside that session's Chromium profile — nothing crosses session boundaries. This is storage isolation, not the out-of-scope service-worker debugging surface listed below.

---

## When something goes wrong

```bash
~/.one-for-all/bin/ofa-status     # broker health, session count, last activity
~/.one-for-all/bin/ofa-tail       # tail every broker + MCP log
~/.one-for-all/bin/ofa-test-live  # round-trip a smoke session against a live broker
./installer/doctor.sh              # verify install integrity (paths, perms, plist, JSON merge)
```

Logs land in `~/.one-for-all/logs/`. Session trace captures land in `~/.one-for-all/sessions/<session_id>/trace/`; `ofa logs --session <id>` follows the latest trace JSONL there. Each line is structured (`tracing` JSON layer); pipe through `jq` for filtered views.

If `doctor.sh` flags `~/.claude.json` drift, restore from `~/.claude.json.bak.<timestamp>` — the installer leaves one before every merge.

---

## Resource shape

- **Per session:** ~80 MB RAM Chromium baseline + page memory.
- **Cap:** 16 concurrent sessions by default. Beyond cap → `-32012 SessionLimitExceeded`. Override via `one-for-all-broker --max-sessions N`.
- **Idle:** 5 minutes after socket disconnect, the per-session Chromium is killed (`Browser.close` → 5 s wait → `child.kill()`). UDD on disk persists.
- **Disk:** `~/.one-for-all/chromium/<rev>/` ≈ 200 MB. `~/.one-for-all/sessions/<id>/` grows with cookies, cache, IndexedDB.

---

## Configuration

Almost none. Defaults are right for almost everyone.

- `--max-sessions <N>`: concurrent session cap. Default 16.
- `--idle-shutdown-secs <N>`: per-session idle window. Default 300.
- `--headless-default <bool>`: forces headed mode for sessions that don't pass `headed: true`. Default true.

The MCP server takes no flags; it auto-discovers the broker.

---

## What's not in v1.0.0

Out of scope but documented so we don't paint into a corner (tracked internally as "SHOULD-HAVE for v1.1"):

- Trace viewer UI (web-based replay against the M10 trace JSONL).
- HAR export from `net.observe` history.
- Codegen mode (record agent actions as Playwright/Puppeteer script).
- WebAuthn virtual authenticator.
- Service worker debugging surface.
- Headed↔headless live swap on a running session.

Pull requests welcome. Read `ARCHITECTURE.md` first.

---

**END QUICKSTART.md v1.0.0**
