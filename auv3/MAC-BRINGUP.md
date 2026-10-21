# Mac bring-up: install, trust, `auval`

Brings up the CI-built AUv3 on a local Mac. **No code-signing by default:**
releases ship unsigned (ad-hoc signatures only — that is what CI produces and
what the release zips contain). Only if Logic refuses the appex after the
steps below, see the "Fallback: local re-sign" section at the end.

## 1. Get the build

Download a release (`auv3-macos.zip`) or the `auv3-macos` artifact from the
`auv3` GitHub Actions workflow (`.github/workflows/auv3.yml`) and unzip it. It
contains:

- `Polarity-SC-Dark.appex` — the AUv3 app extension
- `Polarity-SC-Dark AUv3.app` — the standalone host container app (embeds the appex and its CLAP)
- `Polarity-SC-Dark.clap` — the wrapped CLAP bundle

```bash
unzip auv3-macos.zip
```

## 2. Trust (strip quarantine)

Browser-downloaded zips mark the bundles `com.apple.quarantine`; an
un-notarized bundle then fails Gatekeeper with a "cannot be verified" or
"damaged" dialog. Strip it before anything else — see also the tutorial at the
top of the repo [README](../README.md):

```bash
xattr -dr com.apple.quarantine "Polarity-SC-Dark.appex" "Polarity-SC-Dark AUv3.app" "Polarity-SC-Dark.clap"
```

## 3. Smoke test without any DAW

```bash
open "Polarity-SC-Dark AUv3.app"
```

The standalone host app must load the embedded appex and CLAP and show the
Vizia GUI. If it fails here, the wrapper/GUI path is broken and no DAW will
work either — fix before proceeding. On macOS 14+ this failure surfaces as a
dialog *from the host app itself* ("Failed to load Audio Unit" / OSStatus
codes); run the app from Terminal to see its `[auv3-standalone]` diagnostics
and `pluginkit -L | grep Polarity` for registration state.

## 4. Install

Copy `Polarity-SC-Dark AUv3.app` into `/Applications/`, then launch that
installed copy (`open "/Applications/Polarity-SC-Dark AUv3.app"`) — launching
registers the embedded appex with `pluginkit`. (If the app was copied there
before stripping quarantine, strip it in `/Applications` too.)

```bash
sudo xattr -dr com.apple.quarantine "/Applications/Polarity-SC-Dark AUv3.app"
pluginkit -e use -i de.polarity-music.polarity-sc-dark.auv3
pluginkit -L | grep Polarity
```

## 5. Validate

```bash
auval -v aufx PscD Plsc
```

Expected: `VALIDATING AUDIO UNIT: aufx PscD - Plsc` ending in
`auval succeeded!`. (`aufx`/`PscD`/`Plsc` are the type/subtype/manufacturer
codes set in `auv3/CMakeLists.txt`.)

## 6. Logic gate test

Continue with [`auv3/LOGIC-GATE.md`](LOGIC-GATE.md).

## Fallback: local re-sign

The appex carries only an ad-hoc signature. On some macOS versions
`pluginkit` refuses ad-hoc extensions at registration — symptom: the app runs
and `pluginkit -L` never lists the AU, even after re-launching. Fix with any
Apple Development or Developer ID identity (free with an Apple Developer
account; find with `security find-identity -v -p codesigning`).

**Sign the appex BEFORE the container app (inside-out); verify inside-out.**
Run these from the repo root so the entitlements path resolves:

```bash
codesign --force --deep -s <REAL_IDENTITY> \
  --entitlements auv3/clap-wrapper/src/detail/auv3/auv3.entitlements "Polarity-SC-Dark.appex"
codesign --force --deep -s <REAL_IDENTITY> \
  --entitlements auv3/clap-wrapper/src/detail/auv3/auv3.entitlements "Polarity-SC-Dark AUv3.app"
codesign --verify --deep --verbose=2 "Polarity-SC-Dark.appex"
codesign -d --entitlements - "Polarity-SC-Dark.appex"
```

The last command prints the appex's embedded entitlements; the output must
include `com.apple.security.app-sandbox` (the vendored `auv3.entitlements`
the wrapper build uses). If the check is empty or lacks app-sandbox,
`pluginkit` registration will fail cryptically later — re-sign before
diagnosing anything else.
