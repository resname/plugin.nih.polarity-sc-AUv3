# Mac bring-up: sign, install, `auval`

Brings up the CI-built AUv3 on a local Mac with Xcode installed. No distribution
signing — any Apple Development or Developer ID identity works for local testing.

## 1. Get the build

Download the `auv3-macos` artifact from the `auv3` GitHub Actions workflow
(`.github/workflows/auv3.yml`) and unzip it. It contains:

- `Polarity-SC-Dark.appex` — the AUv3 app extension
- `Polarity-SC-Dark AUv3.app` — the standalone host container app (embeds the appex)
- `Polarity-SC-Dark.clap` — the wrapped CLAP bundle

Browser-downloaded zips mark the bundles `com.apple.quarantine`; an
un-notarized, dev-identity-signed bundle then fails Gatekeeper confusingly.
Strip it before anything else:

```bash
xattr -dr com.apple.quarantine "Polarity-SC-Dark.appex" "Polarity-SC-Dark AUv3.app" "Polarity-SC-Dark.clap"
```

## 2. Re-sign

CI signs ad-hoc, which `pluginkit` rejects. Replace the signature with a local
identity (the placeholder below stands for it): a codesign identity name
available on the Mac, e.g. `Apple Development: you@example.com (XXXXXXXXXX)`
(find with `security find-identity -v -p codesigning`).

**Sign the appex BEFORE the container app (inside-out); verify inside-out.**
Run these from the repo root so the entitlements path resolves.

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

## 3. Smoke test without Logic

```bash
open "Polarity-SC-Dark AUv3.app"
```

The standalone host app must load the CLAP and show the Vizia GUI. If that
fails, the wrapper/GUI path is broken and Logic will fail too — fix before
proceeding.

## 4. Install

Copy `Polarity-SC-Dark AUv3.app` into `/Applications/`, then launch that
installed copy (`open "/Applications/Polarity-SC-Dark AUv3.app"`) — launching
registers the embedded appex with `pluginkit`.

```bash
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
