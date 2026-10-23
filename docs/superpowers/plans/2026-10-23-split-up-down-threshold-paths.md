# Independent Upwards/Downwards Threshold Paths — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the downwards and upwards compression paths each their own attack, release, global threshold, threshold center/slope, threshold curve (incl. curve points), mode, SC channel link, and smoothing — removing the old shared parameters.

**Architecture:** Params: `ThresholdParams` is nested twice under a new `ThresholdPairParams` (idiom already used for `CompressorParams`, compressor_bank.rs:282-287) behind a new `CompressionPathParams` wrapper that also owns the per-path attack/release. DSP: `CompressorBank` splits per-bin state into a per-path `PathState` (thresholds, ratios, knee parabolas, envelopes, raw/smoothed gain buffers); per path: envelope from that path's mode's source → gain differences → per-path smoothing → sum both paths → DC clamp on the combined curve → apply/freeze exactly as today. GUI: top panel becomes a `[Globals] [Thresholds]` tab row; Thresholds tab shows one full column per path.

**Tech Stack:** Rust, nih_plug (fork in `framework/`), Vizia GUI, realfft. Crate `polarity_sc_dark` (workspace root `Cargo.toml`).

**Spec:** `docs/superpowers/specs/2026-10-23-split-up-down-threshold-design.md`

## Global Constraints

- Param IDs are strings. New prefixes `downwards_` / `upwards_` may not collide with existing IDs. All pre-existing param IDs keep their exact spelling (the `tresh_global` typo only survives inside the new per-path copies as `downwards_tresh_global` / `upwards_tresh_global`; the old global `tresh_global` param itself is deleted).
- Old shared params `global/attack`, `global/release`, and everything in the old `threshold` group are **removed** — no hidden re-exports, no state migration. Saved states load with missing keys falling back to defaults (built-in nih_plug behavior, wrapper uses hashed string IDs).
- Version: bump `plugin/Cargo.toml` version **0.5.22 → 0.6.0** in the final task only.
- Regression rule (spec): with **both** paths configured to the old shared values, per-bin gain output and frozen-IR output must match the pre-change build. The DC-bin clamp runs once on the combined curve, never per path.
- Silence gating is per path (`SILENCE_THRESHOLD_GAIN = 1e-4`); zero the path's own raw contribution, `continue` that path's bin; a bin block bypasses a *path* on that path's envelope alone.
- Envelope-follower timing scale (`ENVELOPE_FOLLOWER_TIMING_FADE_MS`, `envelope_followers_timing_scale`) advances once per channel pass (when `channel_idx == num_channels - 1`), not per path.
- Existing style: rustfmt (`cargo fmt`), the crate's existing test helpers in `plugin/src/compressor_bank.rs` `mod tests`.
- All `ThresholdParams::new` call sites pass a `name_prefix` (`"Downwards"` / `"Upwards"` — reuse `DOWNWARDS_NAME_PREFIX` / `UPWARDS_NAME_PREFIX`); every threshold-section param name and curve-point name gets that prefix so GUI/host rows read uniquely.

## Review Focus

1. **DC-bin clamp placement** — clamp must be applied to the combined curve only; clamping per path changes results below ~20 Hz when one path pushes positive gain and the other negative, even at legacy-equal settings. Test: `smoothing_split_equals_legacy_when_paths_match_legacy` (Task 4) plus a mixed-sign test asserting clamp applies to the sum (Task 4).
2. **Per-path dirty flags coupling** — a change on one path must not recompute the other path's arrays. Test: `threshold_param_sets_only_its_own_flags` / `update_if_needed_touches_only_flagged_path` (Task 1).
3. **Silence gating with mixed sources** — a loud sidechain must keep a SidechainCompress path active while the main input is silent; whole block goes silent only when both paths are silent. Test: `silent_main_input_does_not_bypass_sidechain_compress_path` (Task 3).
4. **Match-curve fit convention under mixed modes** — metering source is the sidechain whenever any path is in a sidechain mode, else main input; the one fit is written to both paths' params and each path decodes its own slope convention. Test: `fixed_slope_uses_downwards_path_convention` (Task 5).
5. **Legacy freeze/IR contract** — freeze captures one combined snapshot per channel as before; frozen IR output must not change shape. Test: existing freeze tests keep passing (Task 4), and `frozen_gain_difference_db` stays a combined per-channel array.

---

### Task 1: Parameter tree — per-path threshold params + attack/release

**Files:**
- Modify: `plugin/src/compressor_bank.rs` (param structs & builders only, lines 153-468)
- Modify: `plugin/src/lib.rs` (107-131, 129-162, 225-295, 297-310)
- Modify: `plugin/src/editor.rs` (call sites of `params.threshold` compile fixes only — keep old visuals working with the new tree; the tab UI is Task 7)

**Interfaces:**
- Consumes: existing `ThresholdParams` struct, `CompressorBank` dirty-flag `Arc<AtomicBool>` fields (`should_update_*`, compressor_bank.rs:63-76).
- Produces:
  - `compressor_bank::ThresholdPairParams { downwards: Arc<CompressionPathParams>, upwards: Arc<CompressionPathParams> }` (`#[derive(Params)]`, `#[nested(id_prefix = "downwards", group = "downwards")]` / `"upwards"`)
  - `compressor_bank::CompressionPathParams { compressor_attack_ms: FloatParam ("Attack"), compressor_release_ms: FloatParam ("Release"), #[nested(group = "threshold")] threshold: ThresholdParams }`
  - `ThresholdParams::new(name_prefix: &str, should_update_thresholds: Arc<AtomicBool>, should_update_knee_parabolas: Arc<AtomicBool>) -> Self` — callbacks set **only that path's** two flags
  - `CompressionPathParams::new(name_prefix: &str, should_update_thresholds: Arc<AtomicBool>, should_update_knee_parabolas: Arc<AtomicBool>) -> Self`
  - `ThresholdPairParams::new(compressor_bank: &CompressorBank) -> Self`
  - `SpectralCompressorParams.thresholds: Arc<ThresholdPairParams>` (field named `thresholds`); `GlobalParams` loses `compressor_attack_ms` / `compressor_release_ms`

- [ ] **Step 1: Write the failing test** — in `plugin/src/compressor_bank.rs` `mod tests` (create a `mod param_tests` or reuse `tests`):

```rust
#[test]
fn threshold_param_sets_only_its_own_flags() {
    let (bank, _params, _ir) = make_bank_and_params(1, 64);
    // Fresh bank with all flags cleared
    let flag_sets: Vec<Vec<Arc<AtomicBool>>> = vec![vec![
        bank.should_update_downwards_thresholds.clone(),
        bank.should_update_downwards_knee_parabolas.clone(),
        bank.should_update_upwards_thresholds.clone(),
        bank.should_update_upwards_knee_parabolas.clone(),
    ]]; // build one Arc pair per path from the bank below in the real test
    // Drive the downwards path's params through the params tree:
    // set `params.thresholds.downwards.threshold.threshold_db` via a param update event
    // (params.set_downwards... does not exist; use nih_plug Param::set via ParamPtr or call
    // the callback directly: build the params, then use `unsafe { param.set_value(..) }` style
    // used elsewhere in this crate). Assert: downwards flags true, upwards flags still false.
}
```

Exactly: construct `(bank, params, _)`; snapshot all six flag values (load, then store `false` on all six); call `params.thresholds.downwards.threshold.threshold_db` and `params.compressors.downwards.threshold_offset_db` set paths — the crate's existing way of setting a param programmatically (see how editor events set params; in tests a direct `param.set_value(...)` on `FloatParam` is valid). Assert: `should_update_downwards_thresholds` and `should_update_downwards_knee_parabolas` are `true`; the four upwards/other-path flags are `false`.

- [ ] **Step 2: Run it and watch it fail** — `cargo test -p polarity_sc_dark threshold_param_sets_only_its_own_flags` — FAIL (field `thresholds` does not exist).

- [ ] **Step 3: Implement the param tree** in `compressor_bank.rs` + `lib.rs`:
  - Add `CompressionPathParams` and `ThresholdPairParams` exactly as in Interfaces. Move `ThresholdParams::new` to the flagged-only signature (delete `set_update_both_thresholds`; each `FloatParam::new(...)` keeps its `with_callback` but targeting that path's two `Arc`s — thresholds callback sets both that path's threshold+knee flags, like `CompressorParams::new` does at compressor_bank.rs:502-518).
  - Every threshold param name gets the prefix: `"Downwards Global Threshold"`, `"Downwards Threshold Center"`, `"Downwards Threshold Slope"`, `"Downwards Threshold Curve"`, `"Downwards Mode"`, `"Downwards SC Channel Link"`, `"Downwards Smoothing"`, `"Downwards Curve Point N …"`. Attack/Release names: `"Downwards Attack"` / `"Downwards Release"` (also inside `CompressionPathParams::new`).
  - `ThresholdParams` gains `name_prefix: &'static str`-free — prefix is applied at construction time, param struct unchanged otherwise; `curve_params()` body unchanged (mode-relative slope decode per this path only).
  - `GlobalParams`: delete `compressor_attack_ms`, `compressor_release_ms` fields and their `Default` entries (lib.rs:270-291); keep every other field verbatim.
  - `SpectralCompressorParams`: replace `threshold: Arc<ThresholdParams>` with `#[nested(group = "thresholds")] pub thresholds: Arc<ThresholdPairParams>`; `SpectralCompressorParams::new` constructs `ThresholdPairParams::new(compressor_bank)`.
- [ ] **Step 4: Fix compile fallout mechanically** — update every read site to the new tree, choosing the path that preserves current behavior until later tasks refactor it:
  - `update_envelopes` / `update_envelopes_sidechain` (compressor_bank.rs:1024-1027, 1084-1087): read attack/release from `params.thresholds.downwards` for now (transitional; Task 2 replaces this) — mark with `// FIXME(plan Task 2): per-path envelopes`.
  - Anything reading `params.threshold.X` → `params.thresholds.downwards.threshold.X` for now (match curve `finish_match_curve_channel_if_needed` compressor_bank.rs:1216-1228, analyzer publish compressor_bank.rs:956, editor/analyzer.rs, editor.rs `threshold_column`/`mode_picklist`, curve preset code). Leave a single-source pattern so later tasks swap easily.
  - `threshold_column` reads the downwards lens; `global_column` name-match drops the `Attack`/`Release` branches.
  - `ThresholdMode` import paths in editor `Data` construction unchanged.
- [ ] **Step 5: `cargo test -p polarity_sc_dark`** — all tests pass (existing DSP tests still green because both paths now exist with legacy defaults; adapt those that construct `ThresholdParams` directly).
- [ ] **Step 6: Commit**

```bash
git add plugin/src/compressor_bank.rs plugin/src/lib.rs plugin/src/editor.rs plugin/src/editor/ plugin/src/match_curve.rs
git commit -m "feat(params): per-path threshold params + per-path attack/release (Task 1)"

Co-Authored-By: Claude Code <noreply@anthropic.com>
```

---

### Task 2: Per-path threshold/ratio/knee computation + dirty-flag split in `update_if_needed`

**Files:**
- Modify: `plugin/src/compressor_bank.rs` — introduce `PathState` struct, refactor `update_if_needed` (1607-1732) and per-bin arrays (83-102)

**Interfaces:**
- Consumes: Task 1's param tree (`params.thresholds.downwards.threshold.curve_params()` etc.), `Curve::new(&CurveParams)` (curve.rs).
- Produces:
  - `struct PathState { thresholds_db: Vec<f32>, ratios: Vec<f32>, knee_parabola_scale: Vec<f32>, knee_parabola_intercept: Vec<f32>, envelopes: Vec<Vec<f32>>, raw_gain_difference_db: Vec<f32>, smoothed_gain_difference_db: Vec<f32>, gain_smoothing_prefix_db: Vec<f32> }` with `pub(crate)` fields, held as `pub(crate) downwards: PathState`, `pub(crate) upwards: PathState` on `CompressorBank`
  - `fn path_state(&mut self, path: Path) -> &mut PathState` where `#[derive(Clone, Copy)] pub(crate) enum Path { Downwards, Upwards }`

- [ ] **Step 1: Write the failing test** `update_if_needed_touches_only_flagged_path`:
  - In `mod tests`: build bank, `bank.upwards.thresholds_db.fill(1234.0)`; clear all flags; set only `should_update_downwards_thresholds = true`; set `params.thresholds.downwards.threshold.threshold_db` to −24.0 and `params.compressors.downwards.threshold_offset_db` to 0.0 via the param set path used in Task 1's test; call `bank.update_if_needed(&params)` directly (add `pub(crate)` to it if needed — keep `fn` private, test in same module).
  - Assert: every `bank.downwards.thresholds_db[bin] == -24.0`; every `bank.upwards.thresholds_db[bin] == 1234.0`.
- [ ] **Step 2: Run it, expect FAIL** — `cargo test -p polarity_sc_dark update_if_needed_touches_only_flagged_path` (field `upwards` does not exist yet).
- [ ] **Step 3: Implement.** Move the per-path four `Vec<f32>` fields into `PathState` (downwards.rs:83-93 → into `downwards`, 96-102 → `upwards`); rewrite `update_if_needed` as one per-path loop over `[(Path::Downwards, flags…), (Path::Upwards, flags…)]` driving the existing threshold/ratio/knee blocks with that path's params (`params.thresholds.<path>.threshold.curve_params()`, `params.compressors.<path>.…`) and its own flags. Update `compress`/`compress_sidechain_match` reads of the moved arrays to `self.downwards/…` / field access via `self.downwards.`, `self.upwards.`. `resize`/`update_capacity`/`reset` fill both `PathState`s. The threshold update uses each path's own curve + offset (today both read the shared curve); the knee blocks and ratio blocks are literally the existing per-path code re-homed.
- [ ] **Step 4: `cargo test -p polarity_sc_dark`** — PASS, including Task 1's test.
- [ ] **Step 5: Commit** — `feat(dsp): per-path threshold/ratio/knee state and update pass (Task 2)` + Co-Authored-By line.

---

### Task 3: Per-path envelopes + gain differences (raw), legacy-equivalent combined result

**Files:**
- Modify: `plugin/src/compressor_bank.rs` — `update_envelopes` (1011-1067), `update_envelopes_sidechain` (1073-1142), `compress` (1241-1341), `compress_sidechain_match` (1350-1479), `process` (899-939), test helpers

**Interfaces:**
- Consumes: Task 2's `PathState`, Task 1's per-path `compressor_attack_ms` / `compressor_release_ms`.
- Produces:
  - `fn update_envelopes(&mut self, magnitudes: &[f32], channel_idx: usize, attack_ms: f32, release_ms: f32, overlap_times: usize)` — attack/release passed by caller now
  - `fn update_envelopes_sidechain(&mut self, path_state: &mut PathState, link_amount: f32, channel_idx: usize, attack_ms: f32, release_ms: f32, overlap_times: usize)` (link from that path's `sc_channel_link`)
  - `fn compress_path(&mut self, buffer: &[Complex32], channel_idx: usize, path: Path, params: &SpectralCompressorParams, path_mode: ThresholdMode, first_non_dc_bin: usize) -> raw accumulation` — per-path, per-bin loop writing `path.raw_gain_difference_db[bin]`, gated silently per path (silence test in Step 1b)
  - Combined stage (unchanged math): `combined_raw[bin] = down_raw + up_raw` summed into `self.smoothed_gain_difference_db` pipeline temporarily — i.e. in this task, sum per-path raws into the existing single `raw_gain_difference_db` (renamed `combined_raw_gain_difference_db` on `CompressorBank`), then smooth+apply as today. Smooth/apply/freeze/analyzer code untouched.

- [ ] **Step 1a: Write failing test `mixed_modes_use_their_own_envelope_source`** (transitionally the shared envelope follows whichever path mode the bank uses — skip until Task 4; instead write):
- [ ] **Step 1b: Write failing test `per_path_attack_release`**: set `params.thresholds.downwards` attack 1.0 ms, upwards attack 100_000.0 ms via the param set path; feed a synthetic buffer through `update_envelopes` for both paths (call with the same magnitude sequence for each `PathState`); assert `bank.downwards.envelopes[0][k] > bank.upwards.envelopes[0][k]` after one call for every bin with nonzero magnitude.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
  - `CompressorBank.envelopes: Vec<Vec<f32>>` (shared) moves to per-`PathState` (`envelopes: Vec<Vec<f32>>`, indexed `[channel][bin]` each).
  - `update_envelopes` becomes magnitude-in + timings-in as in Interfaces; the timing-scale increment (`channel_idx == last channel`) stays in `process`, executed once per channel, before the per-path loop.
  - `process` per channel: for each path → its mode decides envelope source: `Internal`/`SidechainMatch` → `update_envelopes` with the main buffer norms (both use main input today); `SidechainCompress` → `update_envelopes_sidechain` for that path (transitional: modes are read from `params.thresholds.downwards` for the whole `process` arm selection, see Task 4).
  - `compress_path` per path: today's per-bin body of `compress` / `compress_sidechain_match` re-homed onto the path's state; gating uses the path's own envelope (`SILENCE_THRESHOLD_GAIN` test lands in Task 3 test for sidechain mode — `silent_main_input_does_not_bypass_sidechain_compress_path`: fill main buffer with silence-magnitude bins, sidechain magnitudes loud, downwards mode `Internal`, upwards `SidechainCompress` (transitional double-write; if Task 4 hasn't landed, drive via the downwards arm), assert upwards `raw_gain_difference_db` is nonzero for bins beyond `first_non_dc_bin`).
  - After both paths' `compress_path`: sum per-path raws bin-wise into `combined_raw_gain_difference_db`, continue into today's `smooth_gain_differences` + `apply_gain_differences` (unchanged). Envelope asserts (compressor_bank.rs:970-981) switch to summing both paths' envelope arrays (transitional average).
- [ ] **Step 4: Regression test `legacy_pipeline_matches_two_paths_equally`** (`mod tests`): both paths' params set to the legacy config the test helper `configure_test_downwards_compression` already uses (−24 dB threshold, ratio 4, knee 0); push a full buffer through `process` as existing freeze tests do; assert applied `analyzer_input_data.gain_difference_db` per bin equals the value from one call of the pre-split formula — computed inline via `compress_downwards`/`compress_upwards` reference calls. Existing freeze tests (compressor_bank.rs tests module) must keep passing; adapt helper `configure_test_downwards_compression` to write both `PathState`s with the same values.
- [ ] **Step 5: `cargo test -p polarity_sc_dark`** — PASS.
- [ ] **Step 6: Commit** — `feat(dsp): per-path envelopes and gain differences, legacy-equivalent combination (Task 3)` + Co-Authored-By.

---

### Task 4: Per-path smoothing, DC clamp on combined curve, mixed-mode process dispatch

**Files:**
- Modify: `plugin/src/compressor_bank.rs` — `smooth_gain_differences` (1481-1536), `compress`/`compress_sidechain_match` tails, `process` mode dispatch (899-939)
- Modify: `plugin/src/lib.rs` — process dispatch (483-526)

**Interfaces:**
- Consumes: Task 3's per-path raws; `params.thresholds.<path>.threshold.gain_smoothing.value()`.
- Produces:
  - `fn smooth_path_gain_differences(&mut self, path: Path, smoothing_amount: f32, first_non_dc_bin: usize, num_bins: usize)` — the existing algorithm, **minus the DC clamp** (lines 1530-1532 removed from the per-bin loop), operating on the path's raw → path's smoothed buffers via the path's own prefix buffer
  - `fn combine_and_clamp_gain_differences(&mut self, first_non_dc_bin: usize, num_bins: usize)` — `combined[bin] = down.smoothed[bin] + up.smoothed[bin]`, then clamp positive values to 0.0 for `bin < first_non_dc_bin`; result lands in `smoothed_gain_difference_db`-consumed-by-apply (rename `combined_smoothed_gain_difference_db` on `CompressorBank`)
  - `apply_gain_differences` input switches to the combined buffer — body otherwise unchanged (freeze capture, analyzer accumulation, IR contract untouched)

- [ ] **Step 1a: Write failing test `smoothing_split_equals_legacy_when_paths_match_legacy`**: both paths mirrored legacy values (same threshold curve, same smoothing amount s>0), run today's flow; per-bin applied value equals the old single-run `smooth_gain_differences` output — assert against the reference computed with a local copy of the legacy algorithm (linear part only: windowed mean blend) + clamp at DC bins, matching within `1e-3` dB.
- [ ] **Step 1b: Write failing test `dc_clamp_applies_to_combined_curve`**: construct paths whose per-path smoothed values at a DC bin are +2.0 and −5.0 (raw values reachable by setting one path's ratio > 1 and the other's threshold very high so raw ≈ 0 contribution and forcing signs via thresholds/offsets); assert combined DC bin is 0.0 (clamped), not −3.0 and not +2.0.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement** `smooth_path_gain_differences` + `combine_and_clamp_gain_differences`, wire into the tails of the per-path compress calls (per path: smooth; then once per channel: combine+clamp → apply).
- [ ] **Step 4: `cargo test -p polarity_sc_dark`** — PASS including regression and DC-clamp tests.
- [ ] **Step 5: Commit** — `feat(dsp): per-path gain smoothing, combined clamp/apply (Task 4a)`.

### Task 4b: Mixed-mode process dispatch (per-path mode sources)

**Files:**
- Modify: `plugin/src/compressor_bank.rs` — `process` (mode arms at 899-939)
- Modify: `plugin/src/lib.rs` — dispatch (483-526)

**Interfaces:**
- Produces: `pub(crate) fn any_path_sidechain(params: &SpectralCompressorParams) -> bool` in `compressor_bank.rs`; `process()` handles both paths' modes simultaneously:
  - envelope source per path (Task 3 rule, now driven by **that path's** mode: `SidechainMatch` → main-input envelopes; `SidechainCompress` → sidechain envelopes with that path's link)
  - threshold scaling per path (SidechainMatch path scales **only its own** thresholds/knees by the sidechain, link = that path's `sc_channel_link`)
- [ ] **Step 1: Write failing test `silent_main_input_does_not_bypass_sidechain_compress_path`** (from Task 3, now with real dispatch: main silent, sidechain loud, downwards `Internal`, upwards `SidechainCompress`; assert upwards raws nonzero, downwards raws all 0.0, and combined output reflects both).
- [ ] **Step 2: Write failing test `mixed_mode_thresholds_scale_only_matching_path`**: downwards `SidechainMatch` with sidechain at −2 dB per bin, upwards `Internal`; assert `bank.downwards` bin compress uses scaled thresholds while `bank.upwards` stays at its stored thresholds (check `raw_gain_difference_db` behavior directly or via inline reference math).
- [ ] **Step 3: Run, FAIL. Implement.** In `process`: replace the single `match mode` with per-path handling as in Interfaces; `compress_path` takes the path mode; sidechain-match threshold scaling moves inside the per-path pass using that path's link and (for recompute) `upwards_soft_knee_coefficients`/`downwards_soft_knee_coefficients` on that path only. `linked_sidechain_magnitudes` buffer becomes per-path scratch (`linked_sidechain_magnitudes: Vec<f32>` on `PathState`; only the SidechainMatch path's buffer is guaranteed valid). lib.rs dispatch: `ThresholdMode` match → `if compressor_bank::any_path_sidechain(&self.params)` branch running `process_overlap_add_sidechain` with the existing callback, else `process_overlap_add` — both existing STFT paths, no new STFT code.
- [ ] **Step 4: `cargo test -p polarity_sc_dark`** — PASS.
- [ ] **Step 5: Commit** — `feat(dsp): independent per-path modes in one process pass (Task 4b)` + Co-Authored-By.

---

### Task 5: Curve matching — metering source + write both paths

**Files:**
- Modify: `plugin/src/compressor_bank.rs` — `measure_match_curve_complex`/`_sidechain`/`finish_match_curve_channel_if_needed` (1156-1232)
- Modify: `plugin/src/match_curve.rs` — no logic change; caller adapts
- Modify: `plugin/src/editor.rs` — `ClearCurveButton` handler, curve preset write site (editor.rs:421-478, 814-848), wherever `MatchCurveResult` is consumed and applied (search `take_finished_result`)

**Interfaces:**
- Consumes: Task 4b's per-path structure.
- Produces:
  - `fn measure_match_curve(&mut self, main_buffer: &[Complex32], channel_idx: usize, params: &SpectralCompressorParams, overlap_times: usize)` — singular call per channel; internally chooses: sidechain-linked magnitudes (with the sc link of the **first** sidechain path, downwards preferred) when `any_path_sidechain(params)`, else main buffer norms; `finish_match_curve_channel_if_needed` uses the **downwards** path's `center_frequency`, `internal_threshold_slope`-decoded (or sidechain-raw) slope, and `downwards` `threshold_offset_db` — the fit is stored as numbers and applied verbatim to both paths.
  - Fit application (editor side): one `MatchCurveFit` → param events setting **both** paths' threshold params (intercept, center, slope, curve, all points). Same for Clear (zero/clear both paths' points) and preset Save/Load.

- [ ] **Step 1: Write failing test `fixed_slope_uses_downwards_path_convention`** (`match_curve.rs` `mod tests`): bank in mixed mode (downwards `Internal` with stored slope 0, upwards `Internal` too), active meter fed synthetic magnitudes through `measure_match_curve` → published fit's `slope` equals `0.0 − PINK_NOISE_SLOPE_OFFSET_DB_PER_OCT` (i.e. −3.0).
- [ ] **Step 2: Write failing editor-side test if the fit apply lives in a testable pure function** — put the fit→param-events loop into `match_curve.rs` as `fn apply_fit_to_paths(cx, both path lenses, fit)` next to the existing per-path writer; assert both paths' point lists change. If it is event-machinery-bound, cover with Step 6's standalone smoke check instead and note it.
- [ ] **Step 3: Run, FAIL. Implement** the singular metering call, downwards-convention `fixed_slope`, and both-path writes in the editor.
- [ ] **Step 4: `cargo test -p polarity_sc_dark`** — PASS.
- [ ] **Step 5: Manual verification note:** run the standalone (`cargo run -p polarity_sc_dark_standalone`) or `cargo xtask` bundle, hit Match, confirm both curves update on the analyzer (Task 6/7 UI permitting before then — verify again in Task 7). Not gate-blocking on its own; the analyzer visual check completes in Task 7.
- [ ] **Step 6: Commit** — `feat(match): curve matching meters per-path sources, applies fit to both paths (Task 5)` + Co-Authored-By.

---

### Task 6: Analyzer split — two curves, per-path envelope traces, curve selection

**Files:**
- Modify: `plugin/src/analyzer.rs` — `AnalyzerData` (29-69): `curve_params: CurveParams` → `curve_params: [CurveParams; 2]` (index 0 = downwards, 1 = upwards); single `envelope_followers` → `envelope_followers: [Vec<f32>; 2]` (heap-alloc'd arrays are not; keep two separate `Vec<f32>` fields `envelope_followers_downwards` / `envelope_followers_upwards` if that keeps `Default` simpler — **use the two-fields form**); `curve_offsets_db: (f32, f32)` stays (upwards, downwards as today, now evaluated per path)
- Modify: `plugin/src/compressor_bank.rs` — analyzer publish block (952-991): fill both curve params (`params.thresholds.downwards.threshold.curve_params()`, same for upwards) and both envelope traces (per-path channel average of each path's envelopes)
- Modify: `plugin/src/editor/analyzer.rs` — `draw_threshold_curve` (635-683): two `Curve`s (one per `curve_params[i]`), each with its path's offset; `draw_curve_points` uses the **selected** path's curve + points; hit test (`hit_test_point`, `screen_to_point_values`, `begin_drag`, `first_free_point_index`, `set_point_*`) all read the selected path; `editable_curve_display_offset_db` keeps its average behavior per path (`selected path's offset` only when editing — pass the selected path's offset into the edit functions; drawing keeps both curves at their own offsets)
- Modify: `plugin/src/editor.rs` — add `selected_threshold_curve: Arc<AtomicU8>` (0 = downwards, 1 = upwards, default 0) to `editor::Data`; a small selection row in `analyzer_column` (two buttons following the `ClearCurveButton` widget pattern, not a lens-bound PickList) that stores into the atomic; `Analyzer` reads the atomic at event/draw time into `Cell<usize>`
- Modify: `plugin/src/editor/theme.rs` — add `ANALYZER_SPECTRUM_UPWARDS` color constant (reuse the hue of `ANALYZER_THRESHOLD_UPWARDS`)

**Interfaces:**
- Consumes: Task 1 tree; existing `Curve`/`CurveParams` (curve.rs); `theme` constants.
- Produces: `editor::Data.selected_threshold_curve: Arc<AtomicU8>`; `AnalyzerData` two-field shape above.

- [ ] **Step 1: Write failing test `analyzer_curve_params_are_per_path`** — construct `AnalyzerData::default()`, set both entries differently via a small bank-driven path: run Task 2's bank `process` with two distinct path curves and editor open (`should_update_analyzer_data`), assert the two published `CurveParams` entries have distinct intercepts. (Adapt: the publish happens in `compress_path`… the publish block is in `process` — test via `make_bank_and_params` + `process` with differing param trees.)
- [ ] **Step 2: Run, FAIL.** (Field is single `CurveParams` today.)
- [ ] **Step 3: Implement DSP-side publish** (`[CurveParams; 2]`, two envelope per-channel-average traces).
- [ ] **Step 4: Implement editor side**: `draw_threshold_curve` draws both curves with both paints exactly as today but each from its own `Curve` + own offset (delete the shared `curve` construction), `draw_spectrum` gets an upwards-tinted overlay mesh drawn from `envelope_followers_upwards` with `theme::ANALYZER_SPECTRUM_UPWARDS` (alpha/blend: draw the downwards mesh full color first, then the upwards mesh with a translucent overlay; keep bars/mesh segmentation logic by passing the array as data — no duplicate algorithm), curve-point interaction uses the selection atomic. Selection row above the analyzer canvas.
- [ ] **Step 5: `cargo test -p polarity_sc_dark`** and `cargo clippy -p polarity_sc_dark` — PASS. Visual confirmation deferred to Task 7 smoke check.
- [ ] **Step 6: Commit** — `feat(analyzer): two curves + per-path envelope traces with curve selection (Task 6)` + Co-Authored-By.

---

### Task 7: Editor layout — `[Globals] [Thresholds]` tabs + full per-path columns

**Files:**
- Modify: `plugin/src/editor.rs` — `main_column` (633-640), `global_and_threshold_row` (695-706), `global_column` (709-764), `threshold_column` (767-850), new `path_column`, `mode_picklist` (955-980) parameterization; new tab row + state
- Modify: `plugin/src/editor/theme.rs` — tab styling classes if the existing CSS classes don't cover tabs (likely `.tab`/`.tab-selected` new entries)

**Interfaces:**
- Consumes: Task 1 tree (`Data::params.map(|p| p.thresholds.downwards.clone())` lenses, `Data::selected_*`-style models per the codebase's `ExportUiModel` pattern), Task 5's both-path curve matching/preset rows.
- Produces: tab page state — `editor::Data.selected_page: Arc<AtomicU8>` (0 = Globals, 1 = Thresholds, **default 1 = Thresholds**); `fn path_column(cx: &mut Context, path: Path, header: &str)` rendering in order: Attack, Release, section label `THRESHOLD`, Threshold rows (Global Threshold; Threshold = center+slope pair; Curve Matching row; Curve Presets row; Mode; SC Channel Link; Smoothing), section label `COMPRESSOR`, then the existing compressor rows (Offset, Ratio, Hi-Freq Rolloff, Knee, Max Boost — downwards Max Boost rendered as a disabled slider row showing "—"/greyed, not hidden-only).

- [ ] **Step 1: Implement** (GUI — no unit tests, Vizia):
  1. `main_column`: replace `global_and_threshold_row` with the tab structure — small HStack `tab_row(cx)` with two Labels styled as tabs (mirror the header buttons' active/inactive classes; active = gold underline look from the sketch, implement via a `.tab-selected` class in theme.rs).
  2. Page state machine: a small Vizia `Model` (pattern: `ExportUiModel`) holding `Cell<u8>`; the tab labels emit a model event; each page's content is conditional on the model's page lens. Default page = Thresholds.
  3. Globals page: `make_column("Globals", …, global_column)` with `global_column` reduced to: Output Gain row, Mix row, FFT Window (paired sliders row), Freeze row (Freeze + Export IR buttons). Attack/Release branches deleted.
  4. Thresholds page: `HStack` of two `path_column`s — `path_column(Path::Downwards, "Downwards")` and `path_column(Path::Upwards, "Upwards")`, each `make_column(header, class, …)` in the style of `upwards_column`/`downwards_column` (editor.rs:862-901). Inside: one `GenericUi::new_custom` over the path's `CompressionPathParams` lens handling: strip the path's name prefix for labels; merged "Threshold" row for Center+Slope (existing pattern at editor.rs:775-799); inject Curve Matching + Curve Presets rows after the Threshold Curve/Slope row (existing logic at 814-848, now emitting through both paths per Task 5); `mode_picklist(cx, path_threshold_lens)` (parameterize the existing function, editor.rs:955-980); then a second `GenericUi::new_custom` over `params.compressors.<path>` with the existing strip-prefix row pattern, plus a `THRESHOLD` / `COMPRESSOR` section `Label`s between groups. Downwards Max Boost row: `ParamSlider`-style widget disabled (existing `.disabled(...)` idiom) or a greyed placeholder row — pick whichever the existing widget kit supports cleanly.
- [ ] **Step 2: Build + clippy** — `cargo build -p polarity_sc_dark_stand…` use `cargo build -p polarity_sc_dark_standalone` or plain `cargo build` (workspace) and `cargo clippy -p polarity_sc_dark` — clean.
- [ ] **Step 3: Manual smoke check (verification step, not unit test):**
  - Run `cargo run -p polarity_sc_dark_standalone` (or `cargo xtask bundle` + host).
  - Confirm: Thresholds tab default, both full columns; Globals tab shows Output/Mix/FFT/Freeze only, no Attack/Release anywhere in Globals; per-column labels read "Downwards Global Threshold"→"Global Threshold" etc.; Max Boost row present in both columns (disabled in downwards); Mode picklists operate independently per column; Curve Matching triggers both columns' curve change; analyzer shows two curves, editable points land on the selected path's curve.
- [ ] **Step 4: Commit** — `feat(editor): tabbed globals/thresholds panel with full per-path columns (Task 7)` + Co-Authored-By.

---

### Task 8: Version bump + final verification

**Files:**
- Modify: `plugin/Cargo.toml:3` — version 0.5.22 → 0.6.0
- (If `bundler.toml` or CI pins the version, update consistently — check for hardcoded versions there first)

**Interfaces:** none.

- [ ] **Step 1: Full test + lint sweep** — `cargo test --workspace && cargo clippy --workspace && cargo fmt --check` (or the repo's CI-equivalent commands).
- [ ] **Step 2: Bump version, commit** — `chore: bump 0.6.0 for split up/down threshold paths` + Co-Authored-By.
- [ ] **Step 3: Old-state load check:** drop a saved `.clapstate`/`.vst3preset` from an old session into a host (or load with the standalone's state load if available) — must load without errors, duplicated params at defaults. If no old state file is handy, note it as unverified instead of fabricating the check.