# Update Checker + Release Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** GUI shows an update strip when GitHub has a newer release, and pushing a `v*` tag publishes Windows/macOS/Linux VST3+CLAP zips as a GitHub release.

**Architecture:** A new `update_check` module holds the pure version/tag logic and the `ureq` fetch, both unit-testable without a server. The editor wires it up as its own vizia `Model` (mirroring `ExportUiModel`) that receives a fetch-done event from a `cx.spawn` background thread and mounts a CSS-overlay strip over the header. A new `release.yml` reuses `build.yml`'s three-leg matrix and one final job attaches the zips to the release.

**Tech Stack:** Rust (nih_plug fork, Vizia), `ureq` 3.x + serde_json, GitHub Actions (`actions/checkout`, `dtolnay/rust-toolchain`, `softprops/action-gh-release`).

**Spec:** `docs/superpowers/specs/2026-10-23-update-checker-release-pipeline-design.md`

## Global Constraints

- Build/lint env for EVERY cargo command in this repo: `env PATH=/home/a/.cargo/bin:/home/a/local-x11/usr/bin:/usr/local/bin:/usr/bin:/bin LD_LIBRARY_PATH=/home/a/local-x11/usr/lib/aarch64-linux-gnu LIBRARY_PATH=/home/a/local-x11/usr/lib/aarch64-linux-gnu PKG_CONFIG_LIBDIR=/home/a/local-x11/usr/lib/pkgconfig:/home/a/local-x11/usr/share/pkgconfig:/home/a/local-x11/usr/lib/aarch64-linux-gnu/pkgconfig cargo +1.92.0 …` (default nightly toolchain breaks the framework).
- GitHub repo constant is exactly `resname/plugin.nih.polarity-sc-AUv3` (origin); endpoint `https://api.github.com/repos/resname/plugin.nih.polarity-sc-AUv3/releases/latest`, release page `https://github.com/resname/plugin.nih.polarity-sc-AUv3/releases/latest`. Never derived from other sources.
- `GUI_WIDTH`/`GUI_HEIGHT` (1360×690) MUST NOT change; the strip overlays, nothing reflows (auv3/CMakeLists.txt stays at "1360x690").
- Header Panic/Delta/Bypass buttons stay fully clickable while the strip is visible; strip covers only the title/match-status region.
- All network failures (offline, 4xx/5xx, rate-limit, malformed JSON, malformed tag) leave the banner absent and must never log-spam or panic.
- Dismiss (✕) is per-GUI-session only; no persistence anywhere.
- Strip text shows the version string from the fetched release (e.g. "v0.7.0"), never a constant.
- auv3.yml stays untouched. No release signing/notarization.
- Asset names: `Spectral-Compressor-Linux-vst3+clap.zip`, `Spectral-Compressor-Windows-vst3+clap.zip`, `Spectral-Compressor-macOS-universal-vst3+clap.zip`.

## Review Focus

- Network hangs mid-open: fetch runs in a background thread; the GUI must paint the header/strip area immediately with no strip, no blocking main/thread loop (pinned by Task 3's spawn wiring + manual offline check).
- Latest release tag is garbage (`"release-demo"`, `"v0.06.1"`, `"v10-rc1"`): compare numerically or return None — never panic on parse (pinned by `parse_version_tag` tests, Task 1).
- Fetched version equals the running version (user just updated): no banner (pinned by `version_is_newer` tests, Task 1).
- Strip overlaps the live header buttons: buttons remain clickable while strip is up (pinned by Task 3's manual X11 screenshot step).
- `release.yml` with one failed matrix leg or a hand-made `v*` tag with no workflow run: no partial release, and a release created without assets can never happen because only the single final `create-release` job publishes (pinned by workflow-read check, Task 4).

---

### Task 1: Version/tag comparison module

**Files:**
- Create: `plugin/src/update_check.rs`
- Modify: `plugin/src/lib.rs` (add `mod update_check;` near the other `mod` declarations)

**Interfaces:**
- Produces: `pub fn parse_version_tag(tag: &str) -> Option<(u64, u64, u64)>`; `pub fn version_is_newer(latest_tag: &str, running: &str) -> bool` — `running` is a `MAJOR.MINOR.PATCH`-style string from `CARGO_PKG_VERSION` (e.g. `"0.6.0"`); tag may carry an optional leading `v`/`V` and arbitrary suffix (`v10-rc1` → `(10,0,0)`). Both return `Option`/`false` on garbage, never panic.

- [ ] **Step 1: Write the failing tests** in `update_check.rs` (unit tests in a `#[cfg(test)] mod tests` at the bottom; module lives beside the other plugin modules, not inside a submodule dir):

```rust
#[test]
fn parse_version_tag_basic() {
    assert_eq!(parse_version_tag("v0.6.0"), Some((0, 6, 0)));
    assert_eq!(parse_version_tag("0.5.22"), Some((0, 5, 22)));
    assert_eq!(parse_version_tag("V1.2.3"), Some((1, 2, 3)));
}

#[test]
fn parse_version_tag_garbage() {
    assert_eq!(parse_version_tag("release-demo"), None);
    assert_eq!(parse_version_tag(""), None);
    assert_eq!(parse_version_tag("v"), None);
}

#[test]
fn parse_version_tag_suffixes() {
    // Suffix text after the numeric core is ignored.
    assert_eq!(parse_version_tag("v10-rc1"), Some((10, 0, 0)));
    // Leading zeros parse numerically, never lexically.
    assert_eq!(parse_version_tag("v0.06.1"), Some((0, 6, 1)));
}

#[test]
fn version_is_newer_strictly_greater() {
    assert!(version_is_newer("v0.7.0", "0.6.0"));
    assert!(!version_is_newer("v0.6.0", "0.6.0"));
    assert!(!version_is_newer("v0.5.9", "0.6.0"));
    assert!(!version_is_newer("notaversion", "0.6.0"));
    assert!(version_is_newer("v1", "0.99.99"));
}

- [ ] **Step 2: Run the tests to verify they fail**

Run: `env PATH=/home/a/.cargo/bin:/home/a/local-x11/usr/bin:/usr/local/bin:/usr/bin:/bin LD_LIBRARY_PATH=/home/a/local-x11/usr/lib/aarch64-linux-gnu LIBRARY_PATH=/home/a/local-x11/usr/lib/aarch64-linux-gnu PKG_CONFIG_LIBDIR=/home/a/local-x11/usr/lib/pkgconfig:/home/a/local-x11/usr/share/pkgconfig:/home/a/local-x11/usr/lib/aarch64-linux-gnu/pkgconfig cargo +1.92.0 test -p polarity_sc_dark --lib update_check`
Expected: FAIL with cannot-find-function/macro errors (functions not defined yet)

- [ ] **Step 3: Implement** `parse_version_tag` + `version_is_newer` — split the tag on the first non-`0-9.` character (or `.` boundaries), parse each leading numeric group with `str::parse::<u64>()`, short-circuit `None` on any non-numeric piece; missing groups count as 0; compare numerically, strictly greater than running.

- [ ] **Step 4: Run the tests to verify they pass** (same command as Step 2). Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add plugin/src/update_check.rs plugin/src/lib.rs
git commit -m "feat: version/tag comparison for the update checker"
```

### Task 2: Latest-release fetch

**Files:**
- Modify: `plugin/src/update_check.rs`
- Modify: `plugin/Cargo.toml` (add `ureq`)

**Interfaces:**
- Consumes: `parse_version_tag` from Task 1.
- Produces:
```rust
pub struct LatestRelease { pub tag: String, pub version: (u64, u64, u64), pub page_url: String }
pub const RELEASES_LATEST_PAGE_URL: &str =
    "https://github.com/resname/plugin.nih.polarity-sc-AUv3/releases/latest";
pub fn fetch_latest_release(running_version: &str) -> Option<LatestRelease>
```
`fetch_latest_release` performs one GET, parses `tag_name` + `html_url` out of the JSON, and returns `None` on every failure (non-200, parse error, not newer). No retry, no pagination, one response.

- [ ] **Step 1: Write the failing test** — JSON parsing must be testable without a server, so factor it: `fn parse_release_json(json: &str, running_version: &str) -> Option<LatestRelease>` and test it with a recorded response body:

```rust
#[test]
fn parse_release_json_extracts_tag_and_url() {
    let json = r#"{"tag_name":"v0.7.0","html_url":"https://github.com/resname/plugin.nih.polarity-sc-AUv3/releases/tag/v0.7.0"}"#;
    let release = parse_release_json(json, "0.6.0").expect("should parse");
    assert_eq!(release.tag, "v0.7.0");
    assert_eq!(release.version, (0, 7, 0));
    assert_eq!(release.page_url, "https://github.com/resname/plugin.nih.polarity-sc-AUv3/releases/tag/v0.7.0");
}

#[test]
fn parse_release_json_older_is_none() {
    let json = r#"{"tag_name":"v0.6.0","html_url":"https://github.com/x/y/releases/tag/v0.6.0"}"#;
    assert!(parse_release_json(json, "0.6.0").is_none());
}
```
(The test's page URL expectation is what proves `html_url`, not the constant, is used when present.)

- [ ] **Step 2: Run to verify it fails** (same test command shape as Task 1, filter `update_check`)
Expected: FAIL, `parse_release_json` not defined

- [ ] **Step 3: Implement** — add `serde_json = "1.0"` use (already a dependency) and `ureq` (`cargo +1.92.0 add`-equivalent by editing Cargo.toml; latest 3.x line, keep its default features — rustls, no OpenSSL). `fetch_latest_release` does: `ureq::get(API_URL).header("User-Agent", concat!("SpectralCompressor-", env!("CARGO_PKG_VERSION"))).call()` → read body string → `parse_release_json`. The `fetch_latest_release` network path itself is verified in the standalone smoke (Task 3), not by a unit test.

- [ ] **Step 4: Run to verify it passes** — same command. Expected: PASS; also run clippy (`cargo +1.92.0 clippy --no-deps -p polarity_sc_dark --lib --tests`) expecting no new warnings
- [ ] **Step 5: Commit**
```bash
git add plugin/src/update_check.rs plugin/Cargo.toml plugin/Cargo.lock
git commit -m "feat: fetch GitHub latest release for the update checker"
```

### Task 3: Editor wiring + update strip

**Files:**
- Modify: `plugin/src/editor.rs` (new `UpdateModel` mirroring `ExportUiModel`; strip built inside `main_column`, editor.rs:651-658)
- Modify: `plugin/src/editor/theme.css`

**Interfaces:**
- Consumes: `update_check::{fetch_latest_release, RELEASES_LATEST_PAGE_URL, LatestRelease}` from Task 2.
- Produces: vizia model `UpdateModel` with lens `UpdateModel::available` (`Option<LatestRelease>`) and `UpdateModel::dismissed` (`bool`); event enum `UpdateEvent { FetchDone(Option<LatestRelease>), Dismiss }`. Strip markup: HStack `.class("update-strip")` holding a gold dot div, `Label`s "v<version> available", an "Open ↗" button, and a "✕" button.

Details the executor must not have to guess:
- Build the model BEFORE spawning the fetch: `UpdateModel::build(cx)` sets `available: None, dismissed: false`, then `cx.spawn` starts the fetch — the thread computes `fetch_latest_release(env!("CARGO_PKG_VERSION"))` and emits `UpdateEvent::FetchDone(result)` through the spawn's proxy, exactly like the curve-preset save flow (`cx.spawn(move |proxy| { … proxy.emit(...); })`, editor.rs:301). The strip mounts inside `main_column`, AFTER `header_bar(cx);`, as a new element with `.position(Position::Absolute)` and CSS `left: 250px; right: 10px; top: 14px; height: 32px;` (250px clears the ~240px header-button group at the header's left; Panic/Bypass stay clickable). Both `UpdateEvent::FetchDone` and `UpdateEvent::Dismiss` are handled in `UpdateModel::event` (Dismiss sets `dismissed = true`).
- Strip visibility: `.display(UpdateModel::strip_visible.map(|strip| …))` — visible only when `available.is_some() && !dismissed`; vizia's `Display::None` removes it from layout so nothing else needs gating.
- "Open ↗" opens `RELEASES_LATEST_PAGE_URL` via `open::that` on a background thread, matching the title-click handler (editor.rs:753-780) rather than blocking the UI thread.
- CSS additions in theme.css: `.update-strip` (gold outline `#e6b23a` 1px, background `#e6b23a22`, white text, `col-between: 6px`), `.update-strip .update-dot` (10px gold square/round), `.update-open-button` and reuse `param-button` element styling for the ✕.

- [ ] **Step 1: Build + clippy green first**

Run: full suite + clippy as in Global Constraints (test step first: `cargo +1.92.0 test -p polarity_sc_dark --lib update_check`).
Expected: baseline PASS before touching editor.rs

- [ ] **Step 2: Implement** `UpdateModel` + event enum + strip + CSS per the details above.
- [ ] **Step 3: Verify compile + suite** `cargo +1.92.0 test --workspace` and clippy. Expected: PASS, no new warnings
- [ ] **Step 4: Visual + interaction smoke on the standalone** (X11 harness from the split-up-down work: start the standalone, screenshot with the `/tmp/x_capture.py` ctypes harness). Two screenshots:

1. normal GUI (no strip visible, header unchanged at 1360×690 layout);
2. strip forced visible — for the smoke only, temporarily set `UpdateModel::build`'s initial `available` to `Some(LatestRelease { tag: "v0.9.9-debug".into(), version: (0, 9, 9), page_url: RELEASES_LATEST_PAGE_URL.into() })`, rebuild, screenshot, then revert the seed and rebuild.

Expected: strip overlays the header's title/status region only; Delta/Match/Bypass stay clickable; ✕ hides the strip for the session; "Open ↗" opens `https://github.com/resname/plugin.nih.polarity-sc-AUv3/releases/latest` in a browser. (Network-dependent banner on a real open is verified by the fetch smoke: with the seed reverted and a working connection, if the GitHub latest release is `v0.6.0` itself the banner stays hidden — record what was observed.)

- [ ] **Step 5: Commit** (after removing the debug seed!)
```bash
git add plugin/src/editor.rs plugin/src/editor/theme.css
git commit -m "feat(editor): update-available strip with open-in-browser button"
```

### Task 4: Release workflow

**Files:**
- Create: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: existing `build.yml` matrix legs (ubuntu/macos/windows), `bundle-universal` usage from `auv3.yml`, readme files `.github/workflows/readme-{Linux,Windows,macOS}.txt`.
- Produces: on tags `v*`, one release per tag with the three zips named per Global Constraints.

Details:
- Workflow env `CARGO_TERM_COLOR: always`; `permissions: contents: write`; `on: push: tags: ['v*']`.
- Matrix job `package` copying `build.yml`'s legs (ubuntu-22.04 incl. the Linux apt-get package list, macos with `cross-target: x86_64-apple-darwin`, windows) + `actions/checkout@v4` `submodules: recursive` + `dtolnay/rust-toolchain@stable` (plus the aarch64-apple-darwin target on macOS, as in auv3.yml). macOS leg runs `cargo xtask bundle-universal polarity_sc_dark --release`; the other legs `cargo xtask bundle polarity_sc_dark --release`.
- Each leg then zips `target/bundled/*` plus its readme into the asset name from Global Constraints (restore the Linux executables' exec bit when zipping — the pattern is already in build.yml's packaging steps).
- Final job `create-release`, `needs: package`, on ubuntu: download all three zips as artifacts, generate the body from a static "unsigned build" note plus the commit log between the previous `v*` tag and this one (`git tag --list 'v*' --merge HEAD --sort=-v:refname`, take the newest tag below this one off that list; fall back to the static note alone when no previous tag exists), create the release via `softprops/action-gh-release` with the zips attached.
- No signing, no notarization, no AUv3, no prerelease handling (any `v*` tag is a release).

- [ ] **Step 1: Create** `release.yml` per the details above.
- [ ] **Step 2: Verify** — YAML parses and shape is sane:
```bash
python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml')); print('ok')"
```
and a human read-through diffing the matrix/package steps line-by-line against `build.yml` (expected: identical toolchain/dependency steps, only the packaging + release steps differ).
- [ ] **Step 3: Commit**
```bash
git add .github/workflows/release.yml
git commit -m "ci: release workflow attaching vst3+clap zips on v* tags"
```

### Task 5: Final verification + bump handoff note

- [ ] **Step 1: Full sweep** — `cargo +1.92.0 test --workspace`, `cargo +1.92.0 fmt --all`, clippy. Expected: green
- [ ] **Step 2: End-to-end dry run of the checker against the real API** (network permitting):
```bash
curl -sS -H 'User-Agent: SpectralCompressor-0.6.0' https://api.github.com/repos/resname/plugin.nih.polarity-sc-AUv3/releases/latest | head -20
```
Expected either a `tag_name` of an existing release (must be strictly newer than 0.6.0 to banner) or GitHub's 404 "not found" JSON — both are correct outcomes; record which.
- [ ] **Step 3: Commit any fmt fallout** (`git commit -m "chore: fmt"` only if the sweep changed files).

---

## Notes for the executor

- The strip appears in the AUv3 build too (shared `editor.rs`) — acceptance criterion unchanged for it.
- No real GitHub release exists yet (`v0.6.0` is expected to be its first), so a live banner test needs a published release first; the standalone smoke of Task 3 proves UI behavior with a seeded value, and `curl` proves the endpoint.
- If vizia in this checkout lacks `Position::Absolute`, use the checkout's nearest equivalent (e.g. anchoring the strip inside `main_column`'s VStack with negative top offset) — do not raise `GUI_HEIGHT` and do not reflow `globals-bar`.
- Version bump: NOT in scope of this plan. The release for `v0.6.0` is pushed by the human when ready (this repo's convention: tags are pushed, workflow publishes).