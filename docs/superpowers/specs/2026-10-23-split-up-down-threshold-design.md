# Split Threshold/Globals Into Independent Upwards & Downwards Paths

**Date:** 2026-10-23
**Status:** Approved design (user confirmed layout + scope in chat)

## Intent

Today one shared set of parameters governs both the upwards and the downwards
compression bank: attack, release, the whole threshold section (global
threshold, threshold center, threshold slope, threshold curve + curve points,
mode, SC channel link, smoothing). The per-path compressor params (offset,
ratio, hi-freq rolloff, knee, max boost) are already separate via
`CompressorBankParams`.

After this change **each path is a complete, independent compressor**:

- Downwards path: own attack, release, global threshold, threshold center,
  threshold slope, curve (incl. own curve-point array), own mode (Pink Noise /
  Sidechain Matching / Sidechain Compression), own SC channel link, own
  smoothing (frequency-domain gain smoothing), plus the existing
  downwards-* compressor params.
- Upwards path: identical set with its own values.

Motivation (user examples): compress low frequencies while expanding the high
ones; configure sidechain compression on one path while the other path expands
separately; in general, independent frequency regions and dynamics targets for
upward vs downward processing.

## Goals

- The whole former "threshold part" exists twice, one instance per path, with
  independent values, curve points, modes, smoothing and SC channel link.
- Attack/release are per path, no longer in GlobalParams at all.
- Mixed modes across the two paths work (e.g. downwards = Pink Noise while
  upwards = Sidechain Compression).
- Existing per-bank compressor params (offset, ratio, knee, hi-freq rolloff,
  max boost) stay as they are.
- Analyzer overlay draws/edits two curves (one per path), selectable.
- Curve Matching and curve presets apply to **both** paths' curves (the
  matched transfer curve is a single spectrum; writer's call, flagged in chat
  and unobjected).

## Non-goals / anti-goals

- No new compressor math (still upward/downward gain computing per bin).
- No M/S, no new modes.
- Old shared parameters are **removed**, not kept hidden. Existing saved
  states load duplicated params at defaults — user accepted the one-time
  reset of old threshold/attack/release settings. The "tresh_global" ID
  spelling dies with these params.
- No hidden/fallback compatibility parameters; no state migration code
  beyond the default-fallback nih_plug already does.

## Constraints

- Parameter IDs are strings in both CLAP and VST3 wrappers; removing params is
  safe (missing keys fall back to defaults), adding new prefixed params does
  not shift existing IDs.
- Per-bin threshold/ratio/knee arrays, envelope follower arrays and gain
  smoothing need a second instance for the other path. Memory/CPU small but
  real; keep code path-shaped, not copy-pasted: shared functions take a
  per-path struct where feasible.
- Re-entry-spike/silent-bin bypass optimizations from recent commits must keep
  working per path.

## The parts the user decided

1. **Full duplication** of the threshold part as independent paths.
2. **Old shared params removed** (state resets once; accepted).
3. **Layout** (from user sketches, imgur refs):
   - The existing top panel area keeps its structure but becomes a tab bar:
     `[Globals] [Thresholds]` — tabs drawn as simple in-panel tabs (no
     floating pop-out).
   - **Thresholds tab (default):** two full columns, DOWNWARDS left, UPWARDS
     right. Each column, top to bottom:
     - Attack, Release (per path)
     - THRESHOLD group: Global Threshold, Threshold (center + slope in one
       row), Curve Matching, Curve Presets, Mode, SC Channel Link, Smoothing
     - COMPRESSOR group: Offset, Ratio, Hi-Freq Rolloff, Knee, Max Boost
       (Max Boost shown greyed/absent in downwards — upwards-only, matching
       current behavior).
   - **Globals tab:** the shared survivors only — Output, Mix (dry/wet),
     FFT Window + Overlap, Freeze (+ Export IR stays with Freeze).
   - Analyzer stays in its usual place; curve overlay now edits two curves.

## Left to the builder (calls to make)

- `ThresholdParams` becomes a shared struct nested **twice** under
  `CompressorBankParams`/parent with `#[nested(id_prefix = "downwards"/
  "upwards")]` — same idiom the codebase already uses for `CompressorParams`
  (compressor_bank.rs:280-287). Curve point arrays duplicate inside it.
- Param callbacks/dirty-flag plumbing (`should_update_*` Arcs) gets a
  per-path variant set so a change on one path doesn't recompute the other.
- Envelope machinery: two envelope arrays (per bank) with per-path
  attack/release coefficient computation. Sidechain envelope path likewise.
- Mixed-mode processing: `process()` dispatch handles both paths; sidechain
  spectra/envelope machinery is computed if either path needs it.
- Curve Matching writes both curves; curve presets apply to both curves;
  analyzer lets the user pick which curve is being edited.
- **Smoothing pipeline order (settled):** per-path — each path has its own
  raw gain-difference array, smoothed with that path's smoothing param, then
  the two smoothed curves are summed into the combined applied curve. The
  DC-bin clamp (zero positive gain below first non-DC bin, compressor_bank.rs:
  1530-1532) is applied once on the **combined** curve, not per path — this
  keeps both-paths-equal-to-old-shared-settings numerically equal to today
  (smoothing's averaging part is linear; the non-linear clamp stays where it
  is today).
- **Freeze/IR (settled):** frozen snapshot remains **one combined smoothed
  curve per channel** (sum of both paths), exactly today's quantity and IR
  contract. Freeze is global and captures both paths' state at once.
- **Silence gating (settled):** per path — each path's raw contribution is
  zeroed by its own envelope falling below SILENCE_THRESHOLD
  (compressor_bank.rs:1272); a bin block is "silent" only when **both**
  paths' envelopes are silent. The smoother's decay-to-baseline runs per
  path. This is what keeps upward boosts driven by a loud sidechain alive
  while the main input is silent.
- **Curve matching source/slope with mixed modes (settled):** metering
  source = sidechain when **any** path is in a sidechain mode, main input
  otherwise. The one measured transfer curve is written to both paths' curve
  params as the same stored numbers; each path decodes the slope with its own
  mode convention exactly as it decodes any stored curve (Pink Noise path
  applies the −3 dB/oct offset, sidechain paths read it raw).
- **Analyzer data (settled):** `AnalyzerData` carries `CurveParams` +
  curve offsets **per path** (two of each, no more (up+down)*0.5 average),
  and the applied gain-reduction trace stays **one combined trace** — it is
  the actually-applied gain, one truth per bin. Envelope-follower trace
  becomes per-path, drawn in each path's column color.
- GUI: split the current single `threshold` column builder into a per-path
  builder; tab bar; name-match row logic handles the prefixed duplicates
  (`GenericUi` lists params of the nested group twice — the per-path group
  params are separate Params objects, so grouping works naturally).
- Defaults: both paths mirror the old shared defaults (attack 150 ms,
  release 300 ms, global threshold -12 dB, SC link 0.8, smoothing 0),
  keeping existing per-path compressor defaults (upwards ratio 1,
  downwards ratio 2, etc.) unchanged.
- Version bump to 0.6.0 (breaking parameter change, removal of params).

## Success criteria

- Two paths fully independent: changing any threshold-section param or
  attack/release on one leaves the other untouched, verified in a host by
  parameter diffing.
- Mixed modes behave correctly (pink-noise-only vs sidechain-only vs both).
- Old states load without errors, duplicated params at defaults.
- No regressions in the compressor DSP behavior per path compared to the old
  shared-param behavior when values are set equal on both paths.