# AUv3 via clap-wrapper Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the existing Rust CLAP plugin as a macOS AUv3 `.appex` (plus the standalone HostApp wrapper) by wrapping the nih-plug-built CLAP with free-audio/clap-wrapper — no DSP or GUI code changes.

**Architecture:** clap-wrapper's `target_add_auv3_wrapper` CMake function wraps a built `.clap` bundle into an AUv3 app extension. The wrapper embeds the `.clap` into `appex/Contents/PlugIns/`, translates params/state/presets, and parents the CLAP GUI's Cocoa view (Vizia, via nih-plug's `CLAP_WINDOW_API_COCOA` support) into its `AUAudioUnitFactory` view controller. Rust builds and CMake scripting stay on this Linux box; Xcode build, signing, `auval`, and the Logic gate test run on a macOS machine (or GHA `macos-15` for the build, a user Mac for the test).

**Tech Stack:** Rust (nih-plug, existing), CMake ≥ 3.21 (Xcode generator), free-audio/clap-wrapper (MIT, pinned v0.16.0), GitHub Actions `macos-15`.

**Spec:** `docs/superpowers/specs/2026-10-21-auv3-design.md`

## Global Constraints

- AUv3 only — no AUv2 target anywhere (spec: user explicitly rejected AUv2).
- Do not modify anything under `plugin/src/` or `framework/` DSP/GUI code.
- `cargo xtask bundle polarity_sc_dark --release` CLAP/VST3 output must remain unchanged.
- Component codes (fixed everywhere, spec decision): INSTRUMENT_TYPE `aufx`, SUBTYPE_CODE `PscD`, MANUFACTURER_CODE `Plsc` (4 alphanumeric, contains a capital).
- Manufacturer name: `polarity-music`.
- Bundle identifier: `de.polarity-music.polarity-sc-dark.auv3` for the appex; standalone host gets `.auv3standalone` (distinct IDs required — identical IDs break appex registration).
- CLAP the wrapper wraps: `target/bundled/Polarity-SC-Dark.clap` (name from `bundler.toml`).
- clap-wrapper pinned to tag `v0.16.0` (AUv3 added v0.15.1, fixed v0.16.0).
- Distribution is user-only; notarization and public install docs deferred.
- CI (`macos-15`) may ad-hoc-sign; re-signing with a real Apple identity happens on the user's Mac (pluginkit rejects ad-hoc for registration).

## Review Focus

1. Wrong plugin type registration (`aumu` instead of `aufx`) would list the effect as an instrument in Logic — pinned by the CMake check in Task 1 (`grep` assertion on the CMakeLists: no `aumu`).
2. Subtype/manufacturer fourcc colliding with the vendored wrapper's defaults (`gWrq`/`clAd`) — Task 1 asserts our codes `PscD`/`Plsc` are the only ones in our files.
3. Ad-hoc-signed appex failing `pluginkit` registration on the user's Mac — Task 3's checklist step requires re-signing before install, every time.
4. Missing sidechain channel reporting if `target_add_auv3_wrapper` maps CLAP busses 1:1 only — Task 4 gate steps explicitly test sidechain input in Logic; failure routes to the spec's fallback rung (hand-rolled ObjC++ shell, separate plan).
5. Stale/renamed clap-wrapper API pin — Task 1 pins the submodule tag and greps for `target_add_auv3_wrapper` before use, so drift fails loudly at configure time on Linux.

---

### Task 1: Vendor clap-wrapper and add the AUv3 CMake super-project

**Files:**
- Create: `auv3/CMakeLists.txt`
- Create: (git submodule) `auv3/clap-wrapper` → `https://github.com/free-audio/clap-wrapper.git` at tag `v0.16.0`
- Modify: `.gitmodules` (via submodule add)

**Interfaces:**
- Consumes: existing xtask output `target/bundled/Polarity-SC-Dark.clap` (built on the CI runner / user Mac, not here).
- Produces: CMake project `auv3/` exposing target `polarity_sc_dark_as_auv3` (the `.appex`) and target `polarity_sc_dark_as_auv3_standalone` (the host `.app`), both configured by cache variable `POLARITY_CLAP_LOCATION` (absolute path to the built `.clap` bundle; required on Apple hosts).

- [ ] **Step 1: Add the clap-wrapper submodule at the pinned tag**

```bash
cd /home/a/plugin.nih.polarity-sc-dark
git submodule add -q https://github.com/free-audio/clap-wrapper.git auv3/clap-wrapper
git -C auv3/clap-wrapper fetch --tags -q
git -C auv3/clap-wrapper checkout -q v0.16.0
git add auv3/clap-wrapper .gitmodules
```

- [ ] **Step 2: Verify clap-wrapper still exposes the API this plan relies on, and our CMake pins the right codes**

Run: `grep -q "function(target_add_auv3_wrapper)" auv3/clap-wrapper/cmake/wrap_auv3.cmake && grep -q "aufx" auv3/CMakeLists.txt && ! grep -q "aumu" auv3/CMakeLists.txt && echo OK`
Expected: `OK`. The API to rely on is `target_add_auv3_wrapper`, NOT the top-level `CLAP_WRAPPER_BUILD_AUV3` flow, which hardcodes the wrong codes `aumu`/`gWrq`/`clAd` in `cmake/top_level_default.cmake`; no `aumu` may appear in our `auv3/CMakeLists.txt` (effect, not instrument).

- [ ] **Step 3: Write `auv3/CMakeLists.txt`**

```cmake
cmake_minimum_required(VERSION 3.21)
project(polarity_sc_auv3 LANGUAGES CXX OBJC OBJCXX)

set(CLAP_WRAPPER_DOWNLOAD_DEPENDENCIES TRUE CACHE BOOL "Download clap-headers for the wrapper")
set(POLARITY_CLAP_LOCATION "" CACHE PATH "Absolute path to the built Polarity-SC-Dark.clap bundle")

add_subdirectory(clap-wrapper EXCLUDE_FROM_ALL)

if (APPLE)
    add_executable(polarity_sc_dark_as_auv3)
    target_add_auv3_wrapper(
            TARGET polarity_sc_dark_as_auv3
            OUTPUT_NAME "Polarity-SC-Dark"
            BUNDLE_IDENTIFIER "de.polarity-music.polarity-sc-dark.auv3"
            BUNDLE_VERSION "1.0"
            INSTRUMENT_TYPE "aufx"
            MANUFACTURER_NAME "polarity-music"
            MANUFACTURER_CODE "Plsc"
            SUBTYPE_CODE "PscD"
            MACOS_EMBEDDED_CLAP_LOCATION "${POLARITY_CLAP_LOCATION}"
    )
    add_executable(polarity_sc_dark_as_auv3_standalone)
    target_add_auv3_standalone_wrapper(
            TARGET polarity_sc_dark_as_auv3_standalone
            OUTPUT_NAME "Polarity-SC-Dark AUv3"
            BUNDLE_IDENTIFIER "de.polarity-music.polarity-sc-dark.auv3standalone"
            BUNDLE_VERSION "1.0"
            AUV3_TARGET polarity_sc_dark_as_auv3
            AU_TYPE "aufx"
            AU_SUBTYPE "PscD"
            AU_MANUFACTURER "Plsc"
    )
endif()
```

(One note for the implementer: if `target_add_auv3_standalone_wrapper` has a different argument name for `AUV3_TARGET`/`AU_TYPE` in the pinned tag, read `auv3/clap-wrapper/cmake/wrap_auv3_standalone.cmake` first and match the names there — never guess them.)

- [ ] **Step 4: Linux smoke-configure (validates the CMake parses and AUv3 skips off-Apple)**

Run: `cmake -S auv3 -B /tmp/polarity-auv3-configure`
Expected: configure completes; log contains `clap-wrapper: auv3 is only available on macOS/iOS` (the `wrap_auv3.cmake` early-exit for non-Apple), and no CMake errors. If cmake ≥ 3.21 is missing, install it first (`pip install cmake >= 3.21` — needs no root).

- [ ] **Step 5: Confirm the existing Rust build is untouched**

Run: `cargo xtask bundle polarity_sc_dark --release`
Expected: builds clean, `target/bundled/` contains `Polarity-SC-Dark.clap` and the VST3 bundle as before.

- [ ] **Step 6: Commit**

```bash
git add auv3 .gitmodules
git commit -m "build(auv3): clap-wrapper super-project for AUv3 appex"
```

---

### Task 2: CI workflow — build CLAP + appex on macOS, upload artifacts

**Files:**
- Create: `.github/workflows/auv3.yml`

**Interfaces:**
- Consumes: `auv3/CMakeLists.txt` cache var `POLARITY_CLAP_LOCATION` (Task 1).
- Produces: workflow artifact `auv3-macos` containing `Polarity-SC-Dark.appex`, `Polarity-SC-Dark AUv3.app`, and the built `Polarity-SC-Dark.clap` (for later universal/lipo work if needed).

- [ ] **Step 1: Write `.github/workflows/auv3.yml`**

```yaml
name: auv3
on:
  workflow_dispatch:
  push:
    branches: [master]
    paths:
      - 'plugin/**'
      - 'framework/**'
      - 'auv3/**'
      - '.github/workflows/auv3.yml'

jobs:
  auv3-macos:
    runs-on: macos-15
    steps:
      - name: Checkout
        uses: actions/checkout@v4
        with:
          submodules: recursive

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: aarch64-apple-darwin,x86_64-apple-darwin

      - name: Build CLAP (arm64)
        run: cargo xtask bundle polarity_sc_dark --release --target aarch64-apple-darwin
      - name: Build CLAP (x86_64)
        run: cargo xtask bundle polarity_sc_dark --release --target x86_64-apple-darwin

      - name: Merge universal CLAP
        run: |
          mkdir -p target/bundled-universal/Polarity-SC-Dark.clap/Contents/MacOS
          cp -R target/aarch64-apple-darwin/release/bundled/Polarity-SC-Dark.clap/. \
                target/bundled-universal/Polarity-SC-Dark.clap/
          lipo -create \
            target/aarch64-apple-darwin/release/bundled/Polarity-SC-Dark.clap/Contents/MacOS/* \
            target/x86_64-apple-darwin/release/bundled/Polarity-SC-Dark.clap/Contents/MacOS/* \
            -output target/bundled-universal/Polarity-SC-Dark.clap/Contents/MacOS/Polarity-SC-Dark

      - name: Configure (Xcode generator)
        run: cmake -S auv3 -B build -G Xcode -DCLAP_WRAPPER_DOWNLOAD_DEPENDENCIES=ON
              -DCMAKE_OSX_ARCHITECTURES="arm64;x86_64"
              -DPOLARITY_CLAP_LOCATION=$PWD/target/bundled-universal/Polarity-SC-Dark.clap

      - name: Build appex + standalone
        run: |
          xcodebuild -project build/PolaritySC3.xcodeproj -configuration Release -jobs 4 \
            -target polarity_sc_dark_as_auv3 -target polarity_sc_dark_as_auv3_standalone

      - name: Stage and upload
        run: |
          mkdir -p stage
          ditto -R $(find build -name "Polarity-SC-Dark.appex") stage/Polarity-SC-Dark.appex
          ditto -R $(find build -name "Polarity-SC-Dark AUv3.app") "stage/Polarity-SC-Dark AUv3.app"
          ditto -R "$PWD/target/bundled-universal/Polarity-SC-Dark.clap" "stage/Polarity-SC-Dark.clap"
      - uses: actions/upload-artifact@v4
        with:
          name: auv3-macos
          path: stage
```

**Implementer notes (real decisions, not TBD):**
- `PolaritySC3.xcodeproj` above is a placeholder for whatever project name `cmake -G Xcode -S auv3 -B build` actually writes (`sed`/`ls build/*.xcodeproj` in the run step, e.g. `-project "$(ls build/*.xcodeproj | head -1)"`). Implement both robustly with the `ls` form.
- xtask per-target output paths (`target/<target>/release/bundled/...`) must be confirmed against `framework/xtask` source on this Linux box (`grep -rn "bundled" framework/xtask/src`) before finalizing the `lipo` step — fix the paths in the workflow to match, do not leave them guessed.
- The runner runs only two xcodebuild targets; the top-level `clapasvst3` VST3 wrapper must NOT be built (never invoke `-alltargets` — it would build clap-wrapper's own default VST3 target).

- [ ] **Step 2: Validate YAML + actionlint on Linux**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/auv3.yml'))" && echo OK`
Expected: `OK` (install `actionlint` if available — a shellcheck of the workflow is a bonus, not required).

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/auv3.yml
git commit -m "ci(auv3): macOS workflow building AUv3 appex via clap-wrapper"
```

- [ ] **Step 4: Push and watch the run, fix until green**

Run: `git push origin master && gh run watch -w $(gh run list -w auv3 --limit 1 --json databaseId -q '.[0].databaseId')`
Expected: job `auv3-macos` green; artifact `auv3-macos` contains the three bundles. Iterate on real build errors (SDK paths, xtask layout, scheme names) — expected to need 1–3 fix-up pushes.

---

### Task 3: Sign, install, `auval` — user-Mac bring-up document

**Files:**
- Create: `auv3/MAC-BRINGUP.md`

**Interfaces:**
- Consumes: CI artifact `auv3-macos` (Task 2).
- Produces: a document the user follows on a Mac with Xcode; no code produced.

- [ ] **Step 1: Write `auv3/MAC-BRINGUP.md`**

Content (verbatim commands, placeholders `<REAL_IDENTITY>` explained):
1. Download the `auv3-macos` artifact, `unzip` it.
2. Re-sign (ad-hoc CI signature is rejected by `pluginkit`; any Apple Development or Developer ID identity works for local testing):
```bash
codesign --force --deep -s <REAL_IDENTITY> "Polarity-SC-Dark.appex"
codesign --force --deep -s <REAL_IDENTITY> "Polarity-SC-Dark AUv3.app"
codesign --verify --deep --verbose=2 "Polarity-SC-Dark.appex"
```
   Sign the appex BEFORE the container app (inside-out); verify inside-out.
3. Smoke test without Logic first: `open "Polarity-SC-Dark AUv3.app"` — the standalone host app must load the CLAP and show the Vizia GUI. If that fails, the wrapper/GUI path is broken and Logic will fail too.
4. Install: copy `Polarity-SC-Dark AUv3.app` into `/Applications/`, then run it once (registers the embedded appex):
```bash
pluginkit -e use -i de.polarity-music.polarity-sc-dark.auv3
pluginkit -L | grep Polarity
```
5. Validate: `auval -v aufx PscD Plsc` — expected: "VALIDATING AUDIO UNIT: aufx PscD - Plsc" ending in `auval succeeded!`.
6. Logic gate test (see Task 4 checklist).

- [ ] **Step 2: Lint the doc for placeholder drift**

Run: `grep -n "<REAL_IDENTITY>" auv3/MAC-BRINGUP.md`
Expected: exactly the three `codesign` lines use the placeholder; no other step does.

- [ ] **Step 3: Commit**

```bash
git add auv3/MAC-BRINGUP.md
git commit -m "docs(auv3): Mac bring-up: re-sign, install, auval"
```

---

### Task 4: Logic sidechain gate

**Files:**
- Create: `auv3/LOGIC-GATE.md`

**Interfaces:**
- Consumes: appex installed per Task 3.
- Produces: a pass/fail record `auv3/LOGIC-GATE.md#result` the decision rests on (spec's early gate before any polish invested).

- [ ] **Step 1: Write `auv3/LOGIC-GATE.md` checklist**

Steps to record:
1. Logic Pro → new empty project; Polarity-SC-Dark appears grouped under `polarity-music` in the AU list; loads with the Vizia GUI open.
2. Audio sidechain: create an instrument/logic track playing audio; set its output to Polarity-SC-Dark's sidechain (Logic's Sidechain dropdown on the plugin slot). Expected: compressor reacts (mode = Sidechain Compression/Matching), `auval` and Logic remain stable for a 5-minute session.
3. Record both GUI rendering and sidechain behavior; a screenshot in the doc is sufficient.
4. Record Logic version number with the result — AUv3 bus handling differs across Logic versions (research finding).
5. `## Result` section: PASS (proceed, CI polish for user-only release) or FAIL (stop; per spec fallback ladder rung 1 = hand-rolled ObjC++ AUv3 shell — a separate plan; do NOT roll into this one).

- [ ] **Step 2: Commit**

```bash
git add auv3/LOGIC-GATE.md
git commit -m "docs(auv3): Logic Pro sidechain gate checklist"
```

---

### Task 5: Top-level README + repo visibility

**Files:**
- Modify: `README.md` (append an "AUv3" section)

**Interfaces:**
- Consumes: Tasks 1–4 artifacts.
- Produces: public-facing entry point.

- [ ] **Step 1: Append the AUv3 section to `README.md`**

Content: what AUv3 adds (Logic/GarageBand/MainStage/DP/iOS-note), that VST3/CLAP remain the primary formats (Cubase hosts no AU), CI artifact pointer, and a two-line link to `auv3/MAC-BRINGUP.md` and `auv3/LOGIC-GATE.md`. No more than ~10 lines.

- [ ] **Step 2: Commit and push**

```bash
git add README.md
git commit -m "docs: AUv3 target (clap-wrapper) overview"
git push origin master
```

---

## Explicitly out of this plan

- Hand-rolled ObjC++ AUv3 shell (spec fallback rung 1): separate plan, only if Task 4 fails. Do not create any ObjC++/cbindgen scaffolding "in parallel".
- iOS AUv3: deferred (spec non-goal). Noted for later: `wrap_auv3_standalone_ios.cmake` and PR #466 of clap-wrapper cover it; needs its own gate.
- Notarization, Developer ID CI signing, public install instructions: deferred (spec: distribution is user-only this pass).