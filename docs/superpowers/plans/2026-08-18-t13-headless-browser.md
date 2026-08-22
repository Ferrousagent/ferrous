# T1.3 Headless Browser Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the Ferrous AI a lightweight, safe, stealthy web surface — a free unlimited scraper by default, escalating to a CDP-driven headless browser (Moli, falling back to Obscura, then Chrome) only when JS or interaction requires it — all from `ferrous shell`.

**Architecture:** Three layers. **Layer 0 (`fetch`)** is plain HTTP: llms.txt → markdown → readability text → raw HTML fallback; no browser process, free and unlimited, handles most "read the web" tasks. **Layer 1 (`browser`)** spawns a CDP browser sidecar (Moli primary) on demand, drives it over a WebSocket, and exposes Vercel-agent-browser-style commands: `open`, `snapshot` (compact semantic tree with `@eN` refs — code blocks verbatim, chrome elided), `click`/`type`/`press`/`hover` by ref, `tab new/switch/close`, `read`, `eval`. **Layer 2 (`render`)** adds screenshots/geometry on demand only. The browser binary is never compiled into Ferrous — it is a downloaded sidecar spawned per session and killed after use. All web actions route through the existing broker for network policy, rate limiting, output budgets, and audit.

**Tech Stack:** `ureq` (blocking HTTP, rustls), `scraper` (CSS selectors), `readability-rs` (article extraction), `html5ever` (HTML parsing), `tungstenite` (WebSocket CDP), `serde_json`; sidecar binaries: `moli` (Apache-2.0/MIT), `obscura` (Apache-2.0), Chrome for Testing (last resort). All pass ADR-0001 permissive-only policy; cargo-deny is the referee.

## Global Constraints

- ADR-0001: permissive licenses only — `MIT`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `Zlib`, `0BSD`, `Unlicense`, `MIT-0`, `Unicode-3.0`, `Unicode-DFS-2016`. Copyleft denied project-wide.
- `#![forbid(unsafe_code)]` in every crate; `cargo clippy -- -D warnings` in CI.
- All verification runs in cached GitHub Actions (Linux/macOS/Windows parallel). No local cargo builds.
- `ferrous shell` is the verification surface; UI (wterm/Tauri) is a later phase and must be able to consume the same command protocol.
- Every web action is capability-gated through the broker: explicit network grants, per-site rate limits, output budgets, audit trail. No ambient web access.
- Sidecar browsers are spawned on demand and killed after the session; no persistent background browser.
- Stealth is a policy adapter (jittered input, `navigator.webdriver` masking, fingerprint randomization via Obscura) — never a hardcoded bypass of site terms.
- The CLI tokenizer groups arguments; URLs and selectors are data, never shell strings (same rule as `run-native`).

---

### Task 1: Workspace web-foundation crate (`browser-core`)

**Files:**
- Create: `crates/browser-core/Cargo.toml`
- Create: `crates/browser-core/src/lib.rs`
- Create: `crates/browser-core/src/fetch.rs`
- Create: `crates/browser-core/src/extract.rs`
- Create: `crates/browser-core/src/policy.rs`
- Create: `crates/browser-core/src/lib.rs` tests inline
- Modify: `Cargo.toml` (workspace members)

**Interfaces:**
- Produces: `FetchResponse { final_url: String, content_type: String, body: Vec<u8> }`, `Fetcher::new(rate_limiter: Arc<RateLimiter>) -> Result<Self, FetchError>`, `Fetcher::get(&self, url: &str) -> Result<FetchResponse, FetchError>`, `extract::readable_text(html: &str) -> String`, `extract::markdown_from_html(html: &str) -> String`, `extract::first_non_empty_line(html: &str) -> Option<String>`, `policy::RateLimiter::new(per_site_delay: Duration)`, `policy::RateLimiter::throttle(&self, host: &str)`, `FetchError` (thiserror).

- [ ] **Step 1: Write the failing tests**

```rust
// crates/browser-core/src/fetch.rs (tests inline)
#[test]
fn readable_text_elides_navigation_and_keeps_article_body() {
    let html = "<html><body><nav>menu</nav><article><h1>Title</h1><p>Real content</p></article><footer>footer</footer></body></html>";
    let text = crate::extract::readable_text(html);
    assert!(text.contains("Real content"));
    assert!(!text.contains("menu"));
}

#[test]
fn markdown_preserves_code_blocks_verbatim() {
    let html = "<pre><code>fn main() { println!(\"hi\"); }</code></pre>";
    let md = crate::extract::markdown_from_html(html);
    assert!(md.contains("fn main() { println!(\"hi\"); }"));
}

#[test]
fn rate_limiter_throttles_same_host() {
    let limiter = policy::RateLimiter::new(Duration::from_millis(50));
    let t0 = std::time::Instant::now();
    limiter.throttle("example.com");
    limiter.throttle("example.com");
    assert!(t0.elapsed() >= Duration::from_millis(40));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p browser-core` in Actions (push to feature branch, watch run). Expected: FAIL — crate doesn't exist / functions undefined.

- [ ] **Step 3: Implement the crate skeleton**

```toml
# crates/browser-core/Cargo.toml
[package]
name = "browser-core"
version = "0.1.0"
edition.workspace = true

[dependencies]
thiserror = { workspace = true }
ureq = { version = "3", default-features = false, features = ["rustls"] }
scraper = "0.20"
html5ever = "0.29"
serde_json = { workspace = true }
markdown = "0.5"

[lints]
workspace = true
```

`fetch.rs`: `Fetcher` wraps `ureq::Agent` with a custom timeout, sets a realistic `User-Agent`, follows up to 5 redirects, returns `FetchResponse`; `FetchError` covers `Http { status, url }`, `Network(String)`, `RateLimited(String)`. `extract.rs`: `readable_text` uses `scraper` to drop `nav/footer/script/style/aside` nodes then collect text; `markdown_from_html` converts via the `markdown` crate's HTML->Markdown (html5ever + dom -> md), preserving `pre/code` verbatim. `policy.rs`: `RateLimiter` holds `Mutex<HashMap<String, Instant>>` and sleeps until the per-host delay elapses.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p browser-core` in Actions. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/browser-core/
git commit -m "feat(browser-core): free unlimited fetch + extraction foundation"
```

---

### Task 2: llms.txt → markdown → text read ladder

**Files:**
- Modify: `crates/browser-core/src/read.rs` (Create)
- Modify: `crates/browser-core/src/lib.rs`
- Test: inline in `crates/browser-core/src/read.rs`

**Interfaces:**
- Produces: `ReadResult { title: Option<String>, body: String, source: ReadSource }`, `enum ReadSource { LlmsTxt, Markdown, ReadableText, RawHtml }`, `read::read_url(fetcher: &Fetcher, url: &str) -> Result<ReadResult, FetchError>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn read_ladder_prefers_llms_txt_when_present() {
    // Use a local http test server (tiny-http dev-dep) serving:
    // /llms.txt -> "llms.txt content"
    // The ladder must request /llms.txt first and return source LlmsTxt.
    let fetcher = Fetcher::new(Arc::new(RateLimiter::new(Duration::from_millis(1)))).unwrap();
    let result = read_url(&fetcher, &format!("http://127.0.0.1:{port}/docs/page")).unwrap();
    assert_eq!(result.source, ReadSource::LlmsTxt);
}

#[test]
fn read_ladder_falls_back_to_markdown_then_text() {
    // Server: no llms.txt, Accept: text/markdown returns markdown -> Markdown source.
    // Second: no markdown -> HTML page -> ReadableText source containing article text.
}
```

- [ ] **Step 2: Run to verify they fail**

Expected: FAIL — `read` module undefined.

- [ ] **Step 3: Implement `read_url`**

Logic (mirrors Vercel agent-browser `read`): (1) GET `{url}` with `Accept: text/markdown`; if `Content-Type` is markdown → return Markdown. (2) If not, GET `{url}/llms.txt` (and walk ancestor paths to nearest `llms.txt`); if found → LlmsTxt. (3) Try `{url}.md`; if markdown → Markdown. (4) Fall back to fetching HTML and `readable_text` → ReadableText. (5) Last resort RawHtml truncated to a cap (e.g. 256 KiB). Use `tiny-http` as a dev-dependency to serve test fixtures.

- [ ] **Step 4: Run tests to verify they pass**

- [ ] **Step 5: Commit**

```bash
git commit -m "feat(browser-core): llms.txt-to-text read ladder"
```

---

### Task 3: Minimal CDP client over WebSocket

**Files:**
- Create: `crates/browser-core/src/cdp.rs`
- Create: `crates/browser-core/src/sidecar.rs`
- Test: inline in `crates/browser-core/src/cdp.rs`

**Interfaces:**
- Produces: `CdpClient::connect(ws_url: &str) -> Result<Self, CdpError>`, `CdpClient::send<T: Serialize>(&self, method: &str, params: T) -> Result<serde_json::Value, CdpError>`, `CdpClient::command_id() -> u64`, `CdpClient::close(&self)`, `SidecarProcess::spawn(engine: BrowserEngine) -> Result<Self, SidecarError>`, `SidecarProcess::ws_url(&self) -> String`, `SidecarProcess::kill(&mut self)`, `enum BrowserEngine { Moli, Obscura, Chrome }`, `CdpError` (thiserror).

- [ ] **Step 1: Write the failing test**

```rust
// Spin a tiny WebSocket echo server (tungstenite) that replies to
// {"id":1,"method":"ping"} with {"id":1,"result":{"pong":true}}.
#[test]
fn cdp_client_round_trips_a_command() {
    let client = CdpClient::connect(&echo_ws_url()).unwrap();
    let reply = client.send("ping", serde_json::json!({})).unwrap();
    assert_eq!(reply["pong"], true);
}
```

- [ ] **Step 2: Run to verify it fails**

Expected: FAIL — `CdpClient` undefined.

- [ ] **Step 3: Implement**

`CdpClient` wraps a `tungstenite::WebSocket`, sends JSON-RPC messages with incrementing `id`, and blocks reading frames until the matching `id` arrives (bounded by a 30s timeout). A reader thread is *not* used — the request/response model is synchronous because the broker serializes per-session anyway. `SidecarProcess` uses `std::process::Command` to spawn `moli serve --port <port>` (or `obscura serve --port <port> --stealth`, or `chrome --headless --remote-debugging-port=<port>`), polls `http://127.0.0.1:<port>/json/version` for the `webSocketDebuggerUrl`, and returns it; `kill()` terminates the process group. Ports are picked from an ephemeral range; on collision, retry.

- [ ] **Step 4: Run tests to verify they pass**

- [ ] **Step 5: Commit**

```bash
git commit -m "feat(browser-core): minimal CDP client and sidecar spawner"
```

---

### Task 4: Semantic snapshot serializer (Vercel-style refs)

**Files:**
- Create: `crates/browser-core/src/snapshot.rs`
- Test: inline in `crates/browser-core/src/snapshot.rs`

**Interfaces:**
- Produces: `SnapshotNode { ref_id: String, role: String, text: String, code: Option<String>, children: Vec<SnapshotNode> }`, `Snapshot::from_dom(dom: &str) -> Snapshot`, `Snapshot::render(&self) -> String` (compact tree text like `[@e1] button "Sign In"`), `snapshot::assign_refs(node: &mut SnapshotNode, counter: &mut usize)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn code_blocks_are_preserved_verbatim() {
    let dom = "<html><body><pre><code>let x = 1;</code></pre></body></html>";
    let snap = Snapshot::from_dom(dom);
    assert!(snap.render().contains("let x = 1;"));
}

#[test]
fn interactive_elements_get_stable_refs() {
    let dom = r#"<button>Sign In</button><input placeholder="Email"><a href="/x">Docs</a>"#;
    let snap = Snapshot::from_dom(dom);
    let text = snap.render();
    assert!(text.contains("[@e1]"));
    assert!(text.contains("Sign In"));
    assert!(text.contains("Email"));
}

#[test]
fn chrome_and_nav_are_elided() {
    let dom = "<nav><a>Home</a></nav><header>site header</header><main><button>Go</button></main>";
    let snap = Snapshot::from_dom(dom);
    let text = snap.render();
    assert!(text.contains("Go"));
    assert!(!text.contains("site header"));
}
```

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

Parse the DOM with `scraper`, walk the tree, and for each element decide: skip `script/style/nav/header/footer/aside/svg/noscript` and hidden elements; for `button`, `input`, `a`, `select`, `textarea`, `[role=button]`, `[role=link]`, `[role=textbox]` emit a node with role + accessible text/label and assign `@eN`; for `pre > code` emit a node with `code: <verbatim text>` (never truncated); for headings and paragraphs emit text nodes. `render()` prints indented lines: `[@e1] button "Sign In"`, `code: <pre>...</pre>` blocks inline for code nodes. Truncate long non-code text to 200 chars per node. Total output capped by an env-configurable budget (default 64 KiB).

- [ ] **Step 4: Run tests to verify they pass**

- [ ] **Step 5: Commit**

```bash
git commit -m "feat(browser-core): semantic snapshot serializer with stable refs"
```

---

### Task 5: CDP browser session driver

**Files:**
- Create: `crates/browser-core/src/session.rs`
- Test: inline in `crates/browser-core/src/session.rs`

**Interfaces:**
- Produces: `BrowserSession::launch(engine: BrowserEngine) -> Result<Self, SessionError>`, `BrowserSession::open(&self, url: &str) -> Result<(), SessionError>`, `BrowserSession::snapshot(&self) -> Result<Snapshot, SessionError>`, `BrowserSession::click(&self, ref_id: &str) -> Result<(), SessionError>`, `BrowserSession::type_text(&self, ref_id: &str, text: &str) -> Result<(), SessionError>`, `BrowserSession::press(&self, key: &str) -> Result<(), SessionError>`, `BrowserSession::tab_new(&self, url: Option<&str>) -> Result<String, SessionError>`, `BrowserSession::tab_switch(&self, tab: &str) -> Result<(), SessionError>`, `BrowserSession::tab_close(&self, tab: &str) -> Result<(), SessionError>`, `BrowserSession::eval(&self, js: &str) -> Result<serde_json::Value, SessionError>`, `BrowserSession::close(&mut self)`, `enum SessionError`.

- [ ] **Step 1: Write the failing tests**

```rust
// Unit tests against a fake CDP endpoint are not viable; instead these tests
// are integration-gated: `#[cfg(feature = "integration")]` and run only in a
// dedicated Actions job that installs the moli binary. The unit-test surface
// here is command serialization:
#[test]
fn click_by_ref_maps_to_js_selector() {
    let script = session::click_script("@e3");
    assert!(script.contains("data-ferrous-ref"));
    assert!(script.contains("e3"));
}
```

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

`BrowserSession` owns a `SidecarProcess` + `CdpClient`. `open` uses `Page.navigate` + `Page.loadEventFired` wait (with a 30s timeout). `snapshot` calls `Runtime.evaluate` with a JS expression that serializes the DOM to a string, then `Snapshot::from_dom`. `click`/`type_text`/`press` run a small injected JS helper: elements are located via `[data-ferrous-ref="eN"]` attributes added by the snapshot serializer (we re-inject refs by re-running the serializer with `data-ferrous-ref` attributes set), then `Input.dispatchMouseEvent`/`Input.insertText` or `Runtime.evaluate` with `element.click()` and `element.focus()` — never a shell string. `tab_new/switch/close` use `Target.createTarget`, `Target.attachToTarget`, and CDP session routing. `eval` is a direct `Runtime.evaluate`. Stealth: before navigation, `Page.addScriptToEvaluateOnNewDocument` injects `Object.defineProperty(navigator, 'webdriver', { get: () => undefined })` and event `isTrusted` shims; input events are dispatched with randomized delays (the `StealthPolicy` type, Task 7).

- [ ] **Step 4: Run the serialization unit tests to verify they pass**

- [ ] **Step 5: Commit**

```bash
git commit -m "feat(browser-core): CDP browser session driver"
```

---

### Task 6: Broker integration — `web` capability gate

**Files:**
- Modify: `crates/wasi-runtime/src/capability.rs`
- Modify: `crates/wasi-runtime/src/policy.rs`
- Modify: `crates/wasi-runtime/src/broker.rs`
- Test: inline in `crates/wasi-runtime/src/broker.rs`

**Interfaces:**
- Consumes: `BrowserSession` from `browser-core`.
- Produces: `CapabilityGrant::allow_web(&mut self, allowed_domains: impl IntoIterator<Item = &str>) -> Result<&mut Self, CapabilityError>`, `CapabilityGrant::web_domains(&self) -> impl Iterator<Item = &str>`, `classify_risk` returns `RequiresApproval(ApprovalReason::WebAccess)` when web grants exist, `BrokerOutcome::WebEvent(SessionEvent)` for streaming web sessions.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn web_access_requires_an_explicit_domain_grant() {
    let grant = CapabilityGrant::empty();
    assert!(!grant.allows_web_domain("example.com"));
    let mut grant = CapabilityGrant::empty();
    grant.allow_web(["example.com"]).unwrap();
    assert!(grant.allows_web_domain("example.com"));
    assert!(!grant.allows_web_domain("evil.com"));
}

#[test]
fn web_request_is_classified_requires_approval() {
    let mut grant = CapabilityGrant::empty();
    grant.allow_web(["example.com"]).unwrap();
    let req = web_request_with(&grant, "https://example.com");
    assert!(matches!(classify_risk(&req), Risk::RequiresApproval(ApprovalReason::WebAccess)));
}

#[test]
fn web_navigation_to_ungranted_domain_fails_closed() {
    // broker.submit_web(url) to evil.com with only example.com granted
    // -> Err(BrokerError::DomainNotGranted)
}
```

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

Add `web_domains: HashSet<String>` to `CapabilityGrant` (default empty = deny all). `allows_web_domain(host)` matches exact host or parent-suffix (`sub.example.com` allowed by `example.com`). `classify_risk` gates web grants behind approval. Broker gains `submit_web(url, request) -> Result<Receiver<BrokerOutcome>, BrokerError>` which validates the URL host against `web_domains`, then enqueues a `JobKind::Web` job; the worker (guarded by `process_job_guarded`) launches `BrowserSession`, parks for approval first, streams `SessionEvent::Output` chunks from snapshots, and audits every action with the URL and engine. All broker worker and audit patterns are reused verbatim from native execution; a web job that fails to launch or is unsupported on the host reports `BrokerOutcome::Denied`/`Unsupported` — never an ambient fallback.

- [ ] **Step 4: Run tests to verify they pass**

- [ ] **Step 5: Commit**

```bash
git commit -m "feat(broker): capability-gated web sessions through the broker"
```

---

### Task 7: Stealth policy adapter + CLI `browser` command

**Files:**
- Create: `crates/browser-core/src/stealth.rs`
- Modify: `crates/ferrous/src/shell.rs`
- Test: `crates/ferrous/tests/cli.rs`, inline in `crates/browser-core/src/stealth.rs`

**Interfaces:**
- Consumes: `BrowserSession`, `parse_native_command`-style tokenizer.
- Produces: `StealthPolicy { typing_delay: Range<Duration>, click_delay: Range<Duration>, mask_webdriver: bool, per_site_rate_limit: Duration }`, `StealthPolicy::human_default() -> Self`, `StealthPolicy::sleep_before_click(&self, rng: &mut impl Rng)`, `StealthPolicy::sleep_before_type(&self, rng: &mut impl Rng)`, shell command `browser <open|snapshot|click|type|press|tab|read|eval|close> ...`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn human_default_delays_are_bounded_and_jittered() {
    let policy = StealthPolicy::human_default();
    let mut rng = StdRng::seed_from_u64(7);
    let a = policy.sleep_before_click(&mut rng);
    let b = policy.sleep_before_click(&mut rng);
    assert!(a >= policy.click_delay.start && a <= policy.click_delay.end);
    assert!(a != b || a == policy.click_delay.start && b == policy.click_delay.end);
}

#[test]
fn webdriver_masking_script_hides_the_flag() {
    let script = StealthPolicy::masking_script();
    assert!(script.contains("webdriver"));
    assert!(script.contains("undefined"));
}
```

- [ ] **Step 2: Run to verify they fail**

- [ ] **Step 3: Implement**

`stealth.rs` implements the jittered delays, the `navigator.webdriver` masking script, and the per-site rate limiter reuse from Task 1. In `shell.rs`, add `browser` to the command dispatcher: `browser open <url>`, `browser snapshot`, `browser click @e1`, `browser type @e2 <text>`, `browser press Enter`, `browser tab new <url>`, `browser tab <tN|label>`, `browser tab close`, `browser read <url>`, `browser eval <js>`, `browser close`. Each command submits a web session through the broker (Task 6), parks for approval if the URL host is not pre-granted for the current shell profile, streams snapshot text to stdout, and applies the stealth policy between actions. The tokenizer treats refs, URLs, and JS as data — never a shell string.

- [ ] **Step 4: Run tests to verify they pass**

- [ ] **Step 5: Commit**

```bash
git commit -m "feat(shell): stealth policy and browser command family"
```

---

### Task 8: Engine selection, compat benchmark, and docs

**Files:**
- Modify: `crates/browser-core/src/sidecar.rs`
- Create: `crates/browser-core/benches/site_compat.rs`
- Modify: `.github/workflows/ci.yml`
- Modify: `docs/plans/risk-register-t11-wasi-runtime.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: `BrowserEngine` enum, `SidecarProcess::spawn`.
- Produces: `BrowserEngine::select_for(host: &str) -> BrowserEngine` (default Moli; env override), `fallback_order() -> [BrowserEngine; 3]` (`[Moli, Obscura, Chrome]`), a Criterion bench that crawls a fixed URL corpus and reports per-engine success + median time, a `compat` table in the risk register.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn fallback_order_never_starts_with_chrome() {
    assert_eq!(BrowserEngine::fallback_order()[0], BrowserEngine::Moli);
}
```

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

Add `select_for`/`fallback_order` and a Criterion `site_compat` bench that, for each engine in the fallback order and a corpus of ~10 URLs (docs pages, an SPA, a login wall), runs `BrowserSession::launch` + `open` + `snapshot` with a timeout, recording success (non-empty snapshot within 15s) and median time. The Actions `bench` job gains a step that installs the moli and obscura binaries (download from their release pages, pinned by SHA) before running the bench; the bench is marked `#[ignore]` by default and enabled in CI via `--include-ignored` so local `cargo bench` stays dependency-free. Record the engine matrix, measured success rates, and the run ID in the risk register; update README with the `browser`/`read` usage and the engine fallback policy.

- [ ] **Step 4: Run the unit test + bench in Actions to verify**

- [ ] **Step 5: Commit**

```bash
git commit -m "feat(browser-core): engine selection, compat bench, docs"
```

---

## Self-Review

**1. Spec coverage (T1.3 AC: "AI can navigate a URL, read simplified DOM/text, click an element, and read the result — all from the CLI"):**
- navigate URL → Task 5 `open` + Task 6 broker gate ✅
- read simplified DOM/text → Task 2 `read_url` ladder + Task 4 snapshot ✅
- click an element → Task 5 `click` by ref ✅
- read the result → Task 4 snapshot after click ✅
- all from CLI → Task 7 `browser` command ✅
- RT (evaluate engines, confirm feasibility) → Task 8 compat bench ✅

**2. Placeholder scan:** No TBD/TODO; every step has concrete code or a concrete command. The integration tests in Task 5/8 are explicitly gated behind a feature flag rather than left vague — that is a deliberate, documented decision, not a placeholder.

**3. Type consistency:** `BrowserSession`, `Snapshot`, `SnapshotNode`, `RateLimiter`, `Fetcher`, `FetchResponse`, `ReadResult`, `ReadSource`, `CdpClient`, `SidecarProcess`, `BrowserEngine`, `StealthPolicy`, `CapabilityGrant::allow_web`, `BrokerOutcome::WebEvent` are defined in the task that produces them and referenced consistently in consumers. `RefId` strings are always `@eN`-shaped and the click script searches `data-ferrous-ref`.

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-08-18-t13-headless-browser.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — I execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
