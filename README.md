# Polarity-SC-Dark

## macOS: trusting the unsigned builds (read this first)

Release builds are **not** signed with an Apple Developer ID and are not
notarized (the project has no Apple membership). That is fine on macOS, but
Gatekeeper will quarantine anything you download and block it on first use.
Un-quarantine once and it works. Two ways to do it:

**The GUI way**

1. Install the plugin first (copy the bundles where your DAW expects them, or
   drag the `Polarity-SC-Dark AUv3.app` into `/Applications` — see
   [`auv3/MAC-BRINGUP.md`](auv3/MAC-BRINGUP.md)).
2. Try to open/use it once. macOS will say the app "cannot be opened because
   the developer cannot be verified", or "is damaged" (the `damaged` message
   just means *quarantined*, it is not actually broken).
3. Open **System Settings → Privacy & Security**. Below the "FileVault"
   section, a note says the app was blocked with an **Open Anyway** button.
   Click it, then click **Open** in the confirmation dialog.
4. If `Open Anyway` does not appear, instead right-click (or Control-click)
   the app in Finder and choose **Open**, then click **Open** again. If Finder
   has no Open-in-context-menu entry, use the command way below.

**The command way** (works for plugins, which never get an "Open Anyway" row —
hosts scan them silently)

```shell
# VST3 (system-wide install)
sudo xattr -dr com.apple.quarantine "/Library/Audio/Plug-Ins/VST3/Polarity-SC-Dark.vst3"

# CLAP (user install; create the CLAP directory yourself if it does not exist)
sudo xattr -dr com.apple.quarantine "$HOME/Library/Audio/Plug-Ins/CLAP/Polarity-SC-Dark.clap"

# AUv3 standalone container app
sudo xattr -dr com.apple.quarantine "/Applications/Polarity-SC-Dark AUv3.app"
```

Or just strip everything you downloaded in one go, before installing:

```shell
xattr -dr com.apple.quarantine /path/to/the/extracted/release/folder
```

Notes:

- `xattr -dr` is recursive; `-r` matters because every plugin format is a
  bundle directory whose *contents* carry their own quarantine records.
- No `sudo` needed for paths under your home directory.
- The AUv3 `.appex` is code-signed *ad-hoc* (no identity). If Logic Pro still
  refuses it after un-quarantining, the local re-sign fix is documented in
  [`auv3/MAC-BRINGUP.md`](auv3/MAC-BRINGUP.md) — that is a host-registration
  quirk of AUv3 extensions, not a bug in the plugin.

---

Polarity-SC-Dark is an FFT-based spectral compressor built on top of a
vendored NIH-plug snapshot. The repository is organized as a plugin project
first: the product crate lives in `plugin/`, and the framework crates it
depends on live in `framework/`.

The current plugin feature set is intentionally focused on three compressor
modes:

- Pink Noise
- Sidechain Matching
- Sidechain Compression

Freeze and IR export remain part of the compressor workflow.

![Screenshot](Polarity-SC-Dark.png)

## Repository Layout

| Path | Purpose |
| --- | --- |
| `plugin/` | Polarity-SC-Dark crate: DSP, GUI, entry points, assets |
| `framework/nih_plug/` | Vendored NIH-plug core framework crate |
| `framework/nih_plug_derive/` | Vendored NIH-plug derive macros |
| `framework/nih_plug_vizia/` | Vendored VIZIA integration used by the plugin |
| `framework/nih_plug_xtask/` | Vendored bundling/tooling library |
| `framework/xtask/` | `cargo xtask` entry point |
| `framework/cargo_nih_plug/` | Optional `cargo nih-plug` wrapper |
| `docs/` | Project-specific technical notes |
| `docs/user-feedback` | Feature Requests and Reports by users. Each user a markdown file |


## Build

After installing [Rust](https://rustup.rs/), bundle the plugin with:

```shell
rustup run stable cargo xtask bundle polarity_sc_dark --release
```

Artifacts are written to `target/bundled/`.

To build just the plugin crate:

```shell
rustup run stable cargo build -p polarity_sc_dark
```

To run the standalone binary:

```shell
rustup run stable cargo run -p polarity_sc_dark --bin polarity_sc_dark_standalone --release
```

## GUI Development

The plugin theme lives at `plugin/src/editor/theme.css`. Use the `hot-reload`
feature to load the CSS from disk at runtime:

```shell
cargo run -p polarity_sc_dark --bin polarity_sc_dark_standalone --features hot-reload
```

Close and reopen the editor window after editing `plugin/src/editor/theme.css`.

## Notes

- All vendored NIH-plug crates live under `framework/` because I like it.
- `bundler.toml` is intentionally scoped to this single plugin.

## AUv3

An [AUv3](auv3/CMakeLists.txt) build via clap-wrapper targets Apple hosts (Logic Pro, GarageBand, MainStage, Digital Performer; iOS later). VST3/CLAP remain the primary formats — Cubase hosts no AU. CI builds an unsigned (ad-hoc-signed) `auv3-macos` artifact and tags become GitHub Releases with those builds attached ([`.github/workflows/auv3.yml`](.github/workflows/auv3.yml)). See [`auv3/MAC-BRINGUP.md`](auv3/MAC-BRINGUP.md) for trusting, installing, and `auval`, and [`auv3/LOGIC-GATE.md`](auv3/LOGIC-GATE.md) for the Logic sidechain gate checklist.
