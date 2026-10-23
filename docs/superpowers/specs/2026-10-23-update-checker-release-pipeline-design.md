# Update Checker + GitHub Release Pipeline — Design

**Intent.** Users find updates themselves right now. The GUI should tell them when
a newer version exists, and pushing a tag should publish a download that bundles
VST3 + CLAP for all three desktop platforms.

**Goals.**
- When the GUI opens, fetch the latest release of
  `resname/plugin.nih.polarity-sc-AUv3` from GitHub and, if it is strictly newer
  than the running plugin, show an update strip in the header area.
- "Open ↗" opens the release page in the user's browser (`open::that`, already a
  dependency and already used by the Export IR flow).
- Pushing tag `v*` builds Windows/macOS/Linux VST3+CLAP in GitHub Actions and
  attaches them as release assets.

**Non-goals / anti-goals.**
- No network call anywhere outside the GUI-open fetch; no on-demand fetching.
- No user-facing error when the check fails (offline, 4xx/5xx, malformed tag):
  the banner simply never appears. GitHub errors are the expected common case
  for offline hosts, so silence is correct, not suppression.
- No persistence for dismissal, no analytics, no update *downloading* — the
  button only opens the release page.
- The AUv3 build (`auv3.yml`) stays untouched; Apple ID distribution is a
  separate concern.

## Update checker (GUI)

- **Fetch trigger:** once per editor open. In the editor constructor, spawn a
  background thread; it does the blocking HTTP GET so the GUI opens instantly
  and never waits on the network. The fetch is per-open, not cached across
  opens.
- **Endpoint:** `GET https://api.github.com/repos/resname/plugin.nih.polarity-sc-AUv3/releases/latest`.
  Send a `User-Agent` (GitHub API requires one). Unauthenticated API: one
  request per GUI open is far below the rate limits.
- **Comparison:** parse the tag's leading numeric version (strip an optional
  leading `v`/`V`, parse up to three `.`-separated integers, missing
  components = 0) and compare against `CARGO_PKG_VERSION` the same way. Show
  the banner only when the release version is strictly greater. A tag like
  `v10-rc1` compares as `10.0.0`; suffix text is ignored.
- **HTTP client:** `ureq` with `rustls` (new dependency — deliberately not
  reusing `open`/`rfd`, neither does HTTP). Sync, tiny, works on a
  background thread on all three platforms. Limit the total request+parse
  work to a single response; no pagination, no redirects beyond ureq's
  defaults.
- **Threading:** the thread writes its result
  (`Option<UpdateAvailability>`, holding the release tag/URL/version) into an
  `Arc<Mutex<…>>`/`AtomicBool`-style shared slot and keeps the editor's
  existing update-notify mechanism (vizia `Observer`/`Scheduler` or the same
  `Arc` + timer pattern the analyzer already uses) in play: the editor is
  built before the fetch returns, so the result must reach an already-mounted
  view. If no such polling/lens mechanism fires after mount, poll the shared
  slot with a vizia timer (~1 s cadence, stopped once the banner resolves).
- **UI, once available:** a dismissable strip directly under the header bar,
  full width, styled to the existing theme: thin gold outline (`#e6b23a`),
  small gold dot, left text "v<version> is available", right-aligned
  "Open ↗" button and a ✕ dismiss control. The strip is inserted as a child
  of `main_column` between `header_bar` and `globals_bar`; the existing
  layout (`globals-bar` etc.) must reflow cleanly — on Linux/X11 verify in
  the standalone that the strip's insertion does not clip the column content.
- **Dismissal:** ✕ removes the strip for this GUI session only. It reappears
  on the next editor open as long as the (re)fetch still says "newer".
- **Version display honesty:** the strip text shows the *release* version
  found on GitHub, not a hardcoded one.

## Release workflow (CI)

- **Trigger:** push of tags matching `v*` (keep the existing `auv3.yml`
  tag workflow untouched). New workflow file, `.github/workflows/release.yml`.
- **Build matrix,** derived from the (currently deactivated) `build.yml`
  matrix: ubuntu (Linux), macos-latest building the universal binary,
  windows. Each leg runs `cargo xtask bundle --release` (macOS leg:
  `bundle-universal`, producing the single merged `.clap`) so both VST3 and
  CLAP land in `target/bundled/`.
- **Naming/packaging:** each leg zips its `bundled/` bundles (VST3 + CLAP
  together) with the existing platform readme
  (`.github/workflows/readme-<platform>.txt`) into
  `Spectral-Compressor-<platform>-vst3+clap.zip`, e.g.
  `Spectral-Compressor-Windows-vst3+clap.zip`,
  `Spectral-Compressor-macOS(-universal)-vst3+clap.zip`,
  `Spectral-Compressor-Linux-vst3+clap.zip`.
- **Release creation:** one job (after the matrix) uploads all zips to a
  GitHub release for the tag, e.g. via `softprops/action-gh-release`.
  Release body: short summary + release-notes text from the changelog/commit
  range is fine — keep it simple, no external changelog system.
- **Non-goals for CI:** no notarization/signing, no AUv3 in the assets, no
  auto-update installer, no beta/prerelease channel (a `v*` tag is a
  release).

## Left to the builder

- Exact strip layout/CSS (class names, spacing) in `theme.css`.
- The update-strip's exact shared-state type; it need only be cheaply
  cloneable and readable from both the fetch thread and the GUI thread.
- Whether the fetch thread checks for editor-kill (e.g. when the editor
  closes before the fetch returns) via the existing close-handling pattern;
  a detached thread that writes into an `Arc` slot owned by the param
  context is acceptable if that is simpler.
- `--release` tag parsing edge cases (e.g. `v0.06.1`) — compare numerically,
  never lexically.