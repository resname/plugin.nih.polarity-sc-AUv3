// Spectral Compressor: an FFT based compressor
// Copyright (C) 2021-2024 Robbert van der Helm
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use nih_plug::prelude::*;
use realfft::num_complex::Complex32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::analyzer::AnalyzerData;
use crate::curve::{
    Curve, CurveParams, CurvePoint, MAX_THRESHOLD_CURVE_POINTS, THRESHOLD_CURVE_MAX_FREQUENCY_HZ,
    THRESHOLD_CURVE_MIN_FREQUENCY_HZ, THRESHOLD_CURVE_POINT_OFFSET_LIMIT_DB,
};
use crate::frozen_ir::FrozenIrData;
use crate::match_curve::{MatchCurveMeter, MatchCurveRuntime};
use crate::SpectralCompressorParams;

// These are the parameter name prefixes used for the downwards and upwards compression parameters.
// The ID prefixes a re set in the `CompressorBankParams` struct.
const DOWNWARDS_NAME_PREFIX: &str = "Downwards";
const UPWARDS_NAME_PREFIX: &str = "Upwards";
const GAIN_SMOOTHING_MAX_RADIUS_LN: f32 = std::f32::consts::LN_2;
const PINK_NOISE_SLOPE_OFFSET_DB_PER_OCT: f32 = 3.0;

/// Per-bin envelope follower values at or below this gain are considered silent. When every bin
/// of a block is silent the compressor bypasses that block entirely: without this the upwards
/// compressor keeps its (capped) boost alive in silence, so the gain keeps rising during long
/// pauses and the first incoming sound after a long attack is very loud.
const SILENCE_THRESHOLD_GAIN: f32 = 1e-4; // -80 dB

/// The envelopes are initialized to the RMS value of a -24 dB sine wave to make sure extreme upwards
/// compression doesn't cause pops when switching between window sizes and when deactivating and
/// reactivating the plugin.
const ENVELOPE_INIT_VALUE: f32 = std::f32::consts::FRAC_1_SQRT_2 / 8.0;

/// The target frequency for the high frequency ratio rolloff. This is fixed to prevent Spectral
/// Compressor from getting brighter as the sample rate increases.
const HIGH_FREQ_RATIO_ROLLOFF_FREQUENCY_LN: f32 = 10.001068; // 22_050.0f32.ln()

/// The length of time over which the envelope followers fade back from being instant to using the
/// configured timingsafter the compressor bank has been reset.
const ENVELOPE_FOLLOWER_TIMING_FADE_MS: f32 = 150.0;

/// One of the two compression paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Path {
    Downwards,
    Upwards,
}

/// The state for a single compression path: per-bin compressor parameters. Every field is sized to
/// match the current complex FFT buffer.
pub(crate) struct PathState {
    /// Per-bin compressor thresholds, in decibels.
    pub(crate) thresholds_db: Vec<f32>,
    /// The ratios for the compressors. At 1.0 the compressors won't do anything.
    pub(crate) ratios: Vec<f32>,
    /// The knee is modelled as a parabola using the formula `x + a * (x + b)^2`. This is `a` in
    /// that equation. The formula is taken from the Digital Dynamic Range Compressor Design paper
    /// by Dimitrios Giannoulis et. al.
    pub(crate) knee_parabola_scale: Vec<f32>,
    /// `b` in the equation from `knee_parabola_scale`.
    pub(crate) knee_parabola_intercept: Vec<f32>,
    /// The current envelope value for each bin, in linear space. Indexed by
    /// `[channel_idx][compressor_idx]`.
    pub(crate) envelopes: Vec<Vec<f32>>,
    /// Scratch buffer for this path's per-bin gain differences before they are combined with the
    /// other path's.
    pub(crate) raw_gain_difference_db: Vec<f32>,
    /// This path's per-bin gain differences after spectral smoothing but before being combined.
    pub(crate) smoothed_gain_difference_db: Vec<f32>,
    /// Prefix sum over this path's raw gain differences, used by the gain smoother.
    pub(crate) gain_smoothing_prefix_db: Vec<f32>,
    /// Scratch buffer for the linked sidechain magnitudes this path's thresholds are scaled
    /// with in sidechain-match mode. Only a SidechainMatch path's buffer is guaranteed to be
    /// valid.
    pub(crate) linked_sidechain_magnitudes: Vec<f32>,
}

/// Whether any compression path uses one of the sidechain modes. Decides whether the plugin's
/// STFT pipeline needs to run the sidechain variant.
pub(crate) fn any_path_sidechain(params: &SpectralCompressorParams) -> bool {
    [&params.thresholds.downwards, &params.thresholds.upwards]
        .iter()
        .any(|path_params| !matches!(path_params.threshold.mode.value(), ThresholdMode::Internal))
}

impl PathState {
    fn new() -> Self {
        Self {
            thresholds_db: Vec::new(),
            ratios: Vec::new(),
            knee_parabola_scale: Vec::new(),
            knee_parabola_intercept: Vec::new(),
            envelopes: Vec::new(),
            raw_gain_difference_db: Vec::new(),
            smoothed_gain_difference_db: Vec::new(),
            gain_smoothing_prefix_db: Vec::new(),
            linked_sidechain_magnitudes: Vec::new(),
        }
    }

    fn reserve_complex_len(&mut self, complex_buffer_len: usize) {
        self.thresholds_db
            .reserve_exact(complex_buffer_len.saturating_sub(self.thresholds_db.len()));
        self.ratios
            .reserve_exact(complex_buffer_len.saturating_sub(self.ratios.len()));
        self.knee_parabola_scale
            .reserve_exact(complex_buffer_len.saturating_sub(self.knee_parabola_scale.len()));
        self.knee_parabola_intercept
            .reserve_exact(complex_buffer_len.saturating_sub(self.knee_parabola_intercept.len()));
        self.raw_gain_difference_db
            .reserve_exact(complex_buffer_len.saturating_sub(self.raw_gain_difference_db.len()));
        self.smoothed_gain_difference_db.reserve_exact(
            complex_buffer_len.saturating_sub(self.smoothed_gain_difference_db.len()),
        );
        self.gain_smoothing_prefix_db.reserve_exact(
            (complex_buffer_len + 1).saturating_sub(self.gain_smoothing_prefix_db.len()),
        );
        self.linked_sidechain_magnitudes.reserve_exact(
            complex_buffer_len.saturating_sub(self.linked_sidechain_magnitudes.len()),
        );
    }

    fn resize_complex_len(&mut self, complex_buffer_len: usize) {
        self.thresholds_db.resize(complex_buffer_len, 1.0);
        self.ratios.resize(complex_buffer_len, 1.0);
        self.knee_parabola_scale.resize(complex_buffer_len, 1.0);
        self.knee_parabola_intercept.resize(complex_buffer_len, 1.0);
        self.raw_gain_difference_db.resize(complex_buffer_len, 0.0);
        self.smoothed_gain_difference_db
            .resize(complex_buffer_len, 0.0);
        self.gain_smoothing_prefix_db
            .resize(complex_buffer_len + 1, 0.0);
        self.linked_sidechain_magnitudes
            .resize(complex_buffer_len, 0.0);
    }

    fn reserve_envelopes(&mut self, num_channels: usize, complex_buffer_len: usize) {
        self.envelopes.resize_with(num_channels, Vec::new);
        for envelopes in self.envelopes.iter_mut() {
            envelopes.reserve_exact(complex_buffer_len.saturating_sub(envelopes.len()));
        }
    }

    fn resize_envelopes(&mut self, complex_buffer_len: usize) {
        for envelopes in self.envelopes.iter_mut() {
            envelopes.resize(complex_buffer_len, ENVELOPE_INIT_VALUE);
        }
    }
}

/// One entry per compression path in `update_if_needed()`: the path selector, that path's dirty
/// flags, its parameter sets, and the knee coefficient function.
type PathUpdate = (
    Path,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<CompressionPathParams>,
    Arc<CompressorParams>,
    fn(f32, f32, f32) -> (f32, f32),
);

/// A bank of compressors so each FFT bin can be compressed individually. The vectors in this struct
/// will have a capacity of `MAX_WINDOW_SIZE / 2 + 1` and a size that matches the current complex
/// FFT buffer size. This is stored as a struct of arrays to make SIMD-ing easier in the future.
pub struct CompressorBank {
    /// If set, then the downwards thresholds should be updated on the next processing cycle. Can be
    /// set from a parameter value change listener, and is also set when calling `.reset_for_size`.
    pub should_update_downwards_thresholds: Arc<AtomicBool>,
    /// The same as `should_update_downwards_thresholds`, but for upwards thresholds.
    pub should_update_upwards_thresholds: Arc<AtomicBool>,
    /// If set, then the downwards ratios should be updated on the next processing cycle. Can be set
    /// from a parameter value change listener, and is also set when calling `.reset_for_size`.
    pub should_update_downwards_ratios: Arc<AtomicBool>,
    /// The same as `should_update_downwards_ratios`, but for upwards ratios.
    pub should_update_upwards_ratios: Arc<AtomicBool>,
    /// If set, then the parameters for the downwards compression soft knee parabola should be
    /// updated on the next processing cycle. Can be set from a parameter value change listener, and
    /// is also set when calling `.reset_for_size`.
    pub should_update_downwards_knee_parabolas: Arc<AtomicBool>,
    /// The same as `should_update_downwards_knee_parabolas`, but for upwards compression.
    pub should_update_upwards_knee_parabolas: Arc<AtomicBool>,

    /// For each compressor bin, `ln(freq)` where `freq` is the frequency associated with that
    /// compressor. This is precomputed since all update functions need it.
    ln_freqs: Vec<f32>,

    /// The per-path compressor state. Each compression path has its own per-bin thresholds,
    /// ratios and soft knee parabolas.
    pub(crate) downwards: PathState,
    pub(crate) upwards: PathState,

    /// A scaling factor for the envelope follower timings. This is set to 0 and then slowly brought
    /// back up to 1 after after [`CompressorBank::reset()`] has been called to allow the envelope
    /// followers to settle back in.
    envelope_followers_timing_scale: f32,
    /// When sidechaining is enabled, this contains the per-channel frqeuency spectrum magnitudes
    /// for the current block. The compressor thresholds and knee values are multiplied by these
    /// values to get the effective thresholds.
    sidechain_spectrum_magnitudes: Vec<Vec<f32>>,
    /// The sum of both paths' per-bin gain differences before spectral smoothing.
    combined_raw_gain_difference_db: Vec<f32>,
    /// The sum of both paths' smoothed per-bin gain differences that will be applied.
    combined_smoothed_gain_difference_db: Vec<f32>,
    /// The frozen per-bin gain difference snapshot for each channel. When freeze is enabled these
    /// values are captured once and then reused until freeze is disabled or invalidated.
    frozen_gain_difference_db: Vec<Vec<f32>>,
    /// Whether the frozen gain difference snapshot for a channel has already been captured.
    frozen_gain_snapshot_valid: Vec<bool>,
    /// Tracks whether freeze was active in the previous compressor processing pass.
    freeze_was_active: bool,
    /// Tracks whether the current frozen snapshot has already been published for IR export.
    frozen_ir_snapshot_published: bool,
    /// The window size this compressor bank was configured for. This is used to compute the
    /// coefficients for the envelope followers in the process function.
    window_size: usize,
    /// The sample rate this compressor bank was configured for. This is used to compute the
    /// coefficients for the envelope followers in the process function.
    sample_rate: f32,

    /// The input data for the spectrum analyzer. Stores both the spectrum analyzer values and the
    /// current gain reduction. Used to draw the spectrum analyzer and gain reduction display in the
    /// editor.
    analyzer_input_data: triple_buffer::Input<AnalyzerData>,
    /// The exact frozen compressor gain curve shared with the editor for IR export.
    frozen_ir_input_data: triple_buffer::Input<FrozenIrData>,
    /// Set to `true` when analyzer data has been fully prepared and is ready to publish.
    analyzer_needs_publish: bool,
    /// Request/result handoff for editor-triggered threshold curve matching.
    match_curve_runtime: Arc<MatchCurveRuntime>,
    /// Audio-thread local accumulator for threshold curve matching.
    match_curve_meter: MatchCurveMeter,
}

#[derive(Params)]
pub struct ThresholdParams {
    /// The compressor threshold at the center frequency. When sidechaining is enabled, the input
    /// signal is gained by the inverse of this value. This replaces the input gain in the original
    /// Spectral Compressor. In the polynomial below, this is the intercept.
    #[id = "tresh_global"]
    pub threshold_db: FloatParam,
    /// The center frqeuency for the target curve when sidechaining is not enabled. The curve is a
    /// polynomial `threshold_db + curve_slope*x + curve_curve*(x^2)` that evaluates to a decibel
    /// value, where `x = ln(center_frequency) - ln(bin_frequency)`. In other words, this is
    /// evaluated in the log/log domain for decibels and octaves.
    #[id = "thresh_center_freq"]
    pub center_frequency: FloatParam,
    /// The slope for the curve, in the log/log domain. See the polynomial above.
    #[id = "thresh_curve_slope"]
    pub curve_slope: FloatParam,
    /// The, uh, 'curve' for the curve, in the logarithmic domain. This is the third coefficient in
    /// the quadratic polynomial and controls the parabolic behavior. Positive values turn the curve
    /// into a v-shaped curve, while negative values attenuate everything outside of the center
    /// frequency. See the polynomial above.
    #[id = "thresh_curve_curve"]
    pub curve_curve: FloatParam,
    /// Hidden point slots edited from the analyzer graph. These are regular parameters so plugin
    /// state is saved and restored by hosts without requiring custom serialization.
    #[nested(array)]
    pub curve_points: [ThresholdCurvePointParams; MAX_THRESHOLD_CURVE_POINTS],

    /// Controls the type of threshold that should be used. Check [`ThresholdMode`] for more
    /// information.
    #[id = "thresh_mode"]
    pub mode: EnumParam<ThresholdMode>,
    /// A `[0, 1]` parameter that controls how much of the other channels should be mixed in when
    /// computing the channel gain value that is then multiplied with he thresholds and knee values
    /// to the the compression parameters when using the sidechain modes.
    #[id = "thresh_sc_link"]
    pub sc_channel_link: FloatParam,
    /// Smooths the final per-bin gain reduction curve before applying it to the FFT bins.
    #[id = "gain_smoothing"]
    pub gain_smoothing: FloatParam,
}

#[derive(Params)]
pub struct ThresholdCurvePointParams {
    #[id = "curve_point_enabled"]
    pub enabled: BoolParam,
    #[id = "curve_point_frequency"]
    pub frequency: FloatParam,
    #[id = "curve_point_offset"]
    pub offset_db: FloatParam,
}

/// The type of threshold to use.
#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdMode {
    /// Configure the thresholds to offset pink noise. This means that the slope will receive an
    /// additional -3 dB/octave slope.
    #[id = "internal"]
    #[name = "Pink Noise"]
    Internal,
    /// Dynamically reconfigure the thresholds based on a sidechain input. The -3 dB/octave slope
    /// offset is not applied here so the curve stays true to the sidechain input at the default
    /// settings. This works by simply multiplying the sidechain gain levels with the precomputed
    /// threshold, knee start, and knee end values. The sidechain channel linking option determines
    /// how how much of the other channel values to mix in before multiplying the sidechain gain
    /// values with the thresholds.
    #[id = "sidechain"]
    #[name = "Sidechain Matching"]
    SidechainMatch,
    /// Compress the input signal based on the sidechain signal's activity. Can be used to
    /// spectrally duck the input, or to amplify parts of the input based on holes in the sidechain
    /// signal.
    #[id = "sidechain_compress"]
    #[name = "Sidechain Compression"]
    SidechainCompress,
}

impl ThresholdCurvePointParams {
    fn new(
        name_prefix: &str,
        index: usize,
        set_update_thresholds: Arc<dyn Fn(f32) + Send + Sync>,
    ) -> Self {
        let set_update_enabled = {
            let set_update_thresholds = set_update_thresholds.clone();
            Arc::new(move |_| set_update_thresholds(0.0))
        };

        Self {
            enabled: BoolParam::new(
                format!("{name_prefix}Curve Point {} Enabled", index + 1),
                false,
            )
            .with_callback(set_update_enabled)
            .hide()
            .hide_in_generic_ui(),
            frequency: FloatParam::new(
                format!("{name_prefix}Curve Point {} Frequency", index + 1),
                1_000.0,
                FloatRange::Skewed {
                    min: THRESHOLD_CURVE_MIN_FREQUENCY_HZ,
                    max: THRESHOLD_CURVE_MAX_FREQUENCY_HZ,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_callback(set_update_thresholds.clone())
            .with_value_to_string(formatters::v2s_f32_hz_then_khz(0))
            .with_string_to_value(formatters::s2v_f32_hz_then_khz())
            .hide()
            .hide_in_generic_ui(),
            offset_db: FloatParam::new(
                format!("{name_prefix}Curve Point {} Offset", index + 1),
                0.0,
                FloatRange::Linear {
                    min: -THRESHOLD_CURVE_POINT_OFFSET_LIMIT_DB,
                    max: THRESHOLD_CURVE_POINT_OFFSET_LIMIT_DB,
                },
            )
            .with_callback(set_update_thresholds)
            .with_unit(" dB")
            .with_step_size(0.1)
            .hide()
            .hide_in_generic_ui(),
        }
    }

    pub fn curve_point(&self) -> CurvePoint {
        CurvePoint {
            enabled: self.enabled.value(),
            frequency: self.frequency.value(),
            offset_db: self.offset_db.value(),
        }
    }
}

/// The parameters for a single compression path: the envelope follower timings plus that path's
/// own threshold parameters.
#[derive(Params)]
pub struct CompressionPathParams {
    /// The compressor's attack time in milliseconds.
    #[id = "attack"]
    pub compressor_attack_ms: FloatParam,
    /// The compressor's release time in milliseconds.
    #[id = "release"]
    pub compressor_release_ms: FloatParam,
    /// The threshold and curve parameters for this compression path.
    #[nested(group = "threshold")]
    pub threshold: ThresholdParams,
}

/// The downwards and upwards compression paths. Both paths get their own copy of every threshold
/// parameter and their own envelope follower timings.
#[derive(Params)]
pub struct ThresholdPairParams {
    #[nested(id_prefix = "downwards", group = "downwards")]
    pub downwards: Arc<CompressionPathParams>,
    #[nested(id_prefix = "upwards", group = "upwards")]
    pub upwards: Arc<CompressionPathParams>,
}

impl CompressionPathParams {
    /// Create parameters for a single compression path. `name_prefix` (e.g. `"Downwards"`) is
    /// prepended to every parameter name. Changing the threshold parameters sets the passed
    /// path's dirty flags, just like [`CompressorParams::new`] does.
    pub fn new(
        name_prefix: &str,
        should_update_thresholds: Arc<AtomicBool>,
        should_update_knee_parabolas: Arc<AtomicBool>,
    ) -> Self {
        CompressionPathParams {
            compressor_attack_ms: FloatParam::new(
                format!("{name_prefix} Attack"),
                150.0,
                FloatRange::Skewed {
                    min: 0.0,
                    max: 10_000.0,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_unit(" ms")
            .with_step_size(0.1),
            compressor_release_ms: FloatParam::new(
                format!("{name_prefix} Release"),
                300.0,
                FloatRange::Skewed {
                    min: 0.0,
                    max: 10_000.0,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_unit(" ms")
            .with_step_size(0.1),
            threshold: ThresholdParams::new(
                name_prefix,
                should_update_thresholds,
                should_update_knee_parabolas,
            ),
        }
    }
}

impl ThresholdPairParams {
    /// Create the downwards and upwards compression path parameters, linked to `compressor_bank`'s
    /// per-path dirty flags.
    pub fn new(compressor_bank: &CompressorBank) -> Self {
        ThresholdPairParams {
            downwards: Arc::new(CompressionPathParams::new(
                DOWNWARDS_NAME_PREFIX,
                compressor_bank.should_update_downwards_thresholds.clone(),
                compressor_bank
                    .should_update_downwards_knee_parabolas
                    .clone(),
            )),
            upwards: Arc::new(CompressionPathParams::new(
                UPWARDS_NAME_PREFIX,
                compressor_bank.should_update_upwards_thresholds.clone(),
                compressor_bank.should_update_upwards_knee_parabolas.clone(),
            )),
        }
    }
}

/// Contains the compressor parameters for both the upwards and downwards compressor banks.
#[derive(Params)]
pub struct CompressorBankParams {
    #[nested(id_prefix = "upwards", group = "upwards")]
    pub upwards: Arc<CompressorParams>,
    #[nested(id_prefix = "downwards", group = "downwards")]
    pub downwards: Arc<CompressorParams>,
}

/// This struct contains the parameters for either the upward or downward compressors. The `Params`
/// trait is implemented manually to avoid copy-pasting parameters for both types of compressor.
/// Both versions will have a parameter ID and a parameter name prefix to distinguish them.
#[derive(Params)]
pub struct CompressorParams {
    /// The compression threshold relative to the target curve.
    #[id = "threshold_offset"]
    pub threshold_offset_db: FloatParam,
    /// The compression ratio. At 1.0 the compressor is disengaged.
    #[id = "ratio"]
    pub ratio: FloatParam,
    /// A `[0, 1]` scaling factor that causes the compressors for the higher registers to have lower
    /// ratios than the compressors for the lower registers. The scaling is applied logarithmically
    /// rather than linearly over the compressors. If this is set to 1.0, then the ratios will be
    /// the same for every compressor. A value of 0.5 means that at
    /// 22,050 Hz, the compression ratio will be 0.5 times that as the one at 0 Hz.
    #[id = "high_freq_rolloff"]
    pub high_freq_ratio_rolloff: FloatParam,
    /// The compression knee width, in decibels.
    #[id = "knee"]
    pub knee_width_db: FloatParam,
    /// The maximum gain the compressor may add relative to the envelope. Without this limit the
    /// upwards compressor keeps amplifying its slowly releasing envelope during silence, so the
    /// gain keeps rising until the signal returns and spikes. Only used for (and thus only
    /// visible in) the upwards bank; setting it to 0 disables upwards gain completely.
    #[id = "max_gain_change_db"]
    pub max_gain_change_db: FloatParam,
}

impl ThresholdParams {
    /// Create a new [`ThresholdParams`] object for a single compression path. All parameter names
    /// are prefixed with `name_prefix` (e.g. `"Downwards"`). Changing any of the threshold
    /// parameters causes the passed atomics — the corresponding compression path's dirty flags in
    /// the [`CompressorBank`] — to be updated.
    pub fn new(
        name_prefix: &str,
        should_update_thresholds: Arc<AtomicBool>,
        should_update_knee_parabolas: Arc<AtomicBool>,
    ) -> Self {
        let set_update_this_path = Arc::new({
            let should_update_knee_parabolas = should_update_knee_parabolas.clone();
            move |_| {
                should_update_thresholds.store(true, Ordering::Relaxed);
                should_update_knee_parabolas.store(true, Ordering::Relaxed);
            }
        });

        ThresholdParams {
            threshold_db: FloatParam::new(
                format!("{name_prefix} Global Threshold"),
                -12.0,
                FloatRange::Linear {
                    min: -100.0,
                    max: 20.0,
                },
            )
            .with_callback(set_update_this_path.clone())
            .with_unit(" dB")
            .with_step_size(0.1),
            center_frequency: FloatParam::new(
                format!("{name_prefix} Threshold Center"),
                1_000.0,
                FloatRange::Skewed {
                    min: 20.0,
                    max: 20_000.0,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_callback(set_update_this_path.clone())
            // This includes the unit
            .with_value_to_string(formatters::v2s_f32_hz_then_khz(0))
            .with_string_to_value(formatters::s2v_f32_hz_then_khz()),
            // These are polynomial coefficients that are evaluated in the log/log domain
            // (octaves/decibels). The global threshold is the intercept.
            curve_slope: FloatParam::new(
                format!("{name_prefix} Threshold Slope"),
                0.0,
                FloatRange::SymmetricalSkewed {
                    min: -36.0,
                    max: 36.0,
                    factor: FloatRange::skew_factor(-2.0),
                    center: 0.0,
                },
            )
            .with_callback(set_update_this_path.clone())
            .with_value_to_string(Arc::new(|value| {
                let display_value = threshold_slope_to_display_value(value);
                if (display_value * 100.0).round() / 100.0 == 0.0 {
                    String::from("0.00")
                } else {
                    format!("{display_value:.2}")
                }
            }))
            .with_string_to_value(Arc::new(|string| {
                string
                    .trim_end_matches([' ', 'd', 'D', 'b', 'B', '/', 'o', 'O', 'c', 'C', 't', 'T'])
                    .trim()
                    .parse()
                    .ok()
                    .map(threshold_slope_from_display_value)
            }))
            .with_unit(" dB/oct")
            .with_step_size(0.01),
            curve_curve: FloatParam::new(
                format!("{name_prefix} Threshold Curve"),
                0.0,
                FloatRange::SymmetricalSkewed {
                    min: -24.0,
                    max: 24.0,
                    factor: FloatRange::skew_factor(-2.0),
                    center: 0.0,
                },
            )
            .with_callback(set_update_this_path.clone())
            .with_unit(" dB/oct²")
            .with_step_size(0.01)
            .hide_in_generic_ui(),
            curve_points: std::array::from_fn(|index| {
                ThresholdCurvePointParams::new(
                    &format!("{name_prefix} "),
                    index,
                    set_update_this_path.clone(),
                )
            }),

            mode: EnumParam::new(format!("{name_prefix} Mode"), ThresholdMode::Internal)
                // Not the most efficient way to do this, but it's a bit cleaner than the
                // alternative
                .with_callback(Arc::new(move |_| set_update_this_path(0.0))),
            sc_channel_link: FloatParam::new(
                format!("{name_prefix} SC Channel Link"),
                0.8,
                FloatRange::Linear { min: 0.0, max: 1.0 },
            )
            .with_unit("%")
            .with_value_to_string(formatters::v2s_f32_percentage(0))
            .with_string_to_value(formatters::s2v_f32_percentage()),
            gain_smoothing: FloatParam::new(
                format!("{name_prefix} Smoothing"),
                0.0,
                FloatRange::Linear { min: 0.0, max: 1.0 },
            )
            .with_unit("%")
            .with_value_to_string(formatters::v2s_f32_percentage(0))
            .with_string_to_value(formatters::s2v_f32_percentage()),
        }
    }

    /// Build [`CurveParams`] out of this set of parameters.
    pub fn curve_params(&self) -> CurveParams {
        CurveParams {
            intercept: self.threshold_db.value(),
            center_frequency: self.center_frequency.value(),
            // The cheeky 3 additional dB/octave attenuation is to match pink noise with the
            // default settings. When using sidechaining we explicitly don't want this because
            // the curve should be a flat offset to the sidechain input at the default settings.
            slope: match self.mode.value() {
                ThresholdMode::Internal => internal_threshold_slope(self.curve_slope.value()),
                ThresholdMode::SidechainMatch | ThresholdMode::SidechainCompress => {
                    self.curve_slope.value()
                }
            },
            curve: self.curve_curve.value(),
            points: std::array::from_fn(|index| self.curve_points[index].curve_point()),
        }
    }
}

#[inline]
fn internal_threshold_slope(stored_slope: f32) -> f32 {
    stored_slope - PINK_NOISE_SLOPE_OFFSET_DB_PER_OCT
}

#[inline]
fn threshold_slope_to_display_value(stored_slope: f32) -> f32 {
    PINK_NOISE_SLOPE_OFFSET_DB_PER_OCT - stored_slope
}

#[inline]
fn threshold_slope_from_display_value(display_slope: f32) -> f32 {
    PINK_NOISE_SLOPE_OFFSET_DB_PER_OCT - display_slope
}

impl CompressorBankParams {
    /// Create compressor bank parameter objects for both the downwards and upwards compressors of
    /// `compressor`. Changing the ratio, threshold, and knee parameters will cause the compressor
    /// to recompute its values on the next processing cycle.
    pub fn new(compressor: &CompressorBank) -> Self {
        CompressorBankParams {
            downwards: Arc::new(CompressorParams::new(
                DOWNWARDS_NAME_PREFIX,
                compressor.should_update_downwards_thresholds.clone(),
                compressor.should_update_downwards_ratios.clone(),
                compressor.should_update_downwards_knee_parabolas.clone(),
            )),
            upwards: Arc::new(CompressorParams::new(
                UPWARDS_NAME_PREFIX,
                compressor.should_update_upwards_thresholds.clone(),
                compressor.should_update_upwards_ratios.clone(),
                compressor.should_update_upwards_knee_parabolas.clone(),
            )),
        }
    }
}

impl CompressorParams {
    /// Create a new [`CompressorBankParams`] object with a prefix for all parameter names. Changing
    /// any of the threshold, ratio, or knee parameters causes the passed atomics to be updated.
    /// These should be taken from a [`CompressorBank`] so the parameters are linked to it.
    pub fn new(
        name_prefix: &str,
        should_update_thresholds: Arc<AtomicBool>,
        should_update_ratios: Arc<AtomicBool>,
        should_update_knee_parabolas: Arc<AtomicBool>,
    ) -> Self {
        let set_update_thresholds = Arc::new({
            let should_update_knee_parabolas = should_update_knee_parabolas.clone();
            move |_| {
                should_update_thresholds.store(true, Ordering::Relaxed);
                should_update_knee_parabolas.store(true, Ordering::Relaxed);
            }
        });
        let set_update_ratios = Arc::new({
            let should_update_knee_parabolas = should_update_knee_parabolas.clone();
            move |_| {
                should_update_ratios.store(true, Ordering::Relaxed);
                should_update_knee_parabolas.store(true, Ordering::Relaxed);
            }
        });
        let set_update_knee_parabolas = Arc::new(move |_| {
            should_update_knee_parabolas.store(true, Ordering::Relaxed);
        });

        CompressorParams {
            // As explained above, these offsets are relative to the target curve
            threshold_offset_db: FloatParam::new(
                format!("{name_prefix} Offset"),
                0.0,
                FloatRange::Linear {
                    min: -50.0,
                    max: 50.0,
                },
            )
            .with_callback(set_update_thresholds)
            .with_unit(" dB")
            .with_step_size(0.1),
            ratio: FloatParam::new(
                format!("{name_prefix} Ratio"),
                // Downwards compression defaults to 2:1, upwards compression stays at 1:1
                // (disengaged)
                if name_prefix == DOWNWARDS_NAME_PREFIX {
                    2.0
                } else {
                    1.0
                },
                FloatRange::Skewed {
                    min: 1.0,
                    max: 500.0,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_callback(set_update_ratios.clone())
            .with_step_size(0.01)
            .with_value_to_string(formatters::v2s_compression_ratio(2))
            .with_string_to_value(formatters::s2v_compression_ratio()),
            high_freq_ratio_rolloff: FloatParam::new(
                format!("{name_prefix} Hi-Freq Rolloff"),
                // The upwards bank defaults to a gentle rolloff, while the downwards bank keeps
                // full-band ratios by default.
                if name_prefix == UPWARDS_NAME_PREFIX {
                    0.75
                } else {
                    // When used subtly, no rolloff is usually better for downwards compression
                    0.0
                },
                FloatRange::Linear { min: 0.0, max: 1.0 },
            )
            .with_callback(set_update_ratios)
            .with_unit("%")
            .with_value_to_string(formatters::v2s_f32_percentage(0))
            .with_string_to_value(formatters::s2v_f32_percentage()),
            knee_width_db: FloatParam::new(
                format!("{name_prefix} Knee"),
                6.0,
                FloatRange::Skewed {
                    min: 0.0,
                    max: 36.0,
                    factor: FloatRange::skew_factor(-1.0),
                },
            )
            .with_callback(set_update_knee_parabolas)
            .with_unit(" dB")
            .with_step_size(0.1),
            max_gain_change_db: {
                let param = FloatParam::new(
                    format!("{name_prefix} Max Boost"),
                    // The upwards compressor is the only user of this cap; the downwards
                    // compressor only ever attenuates. 24 dB is high enough not to interfere with
                    // normal upwards compression while keeping the silence ramp bounded.
                    24.0,
                    FloatRange::Linear {
                        min: 0.0,
                        max: 96.0,
                    },
                )
                .with_unit(" dB")
                .with_step_size(0.1);
                // A dead 'Downwards Max Boost' slider would be confusing in the generic UI
                if name_prefix == DOWNWARDS_NAME_PREFIX {
                    param.hide()
                } else {
                    param
                }
            },
        }
    }
}

impl CompressorBank {
    /// Set up the compressor for the given channel count and maximum FFT window size. The
    /// compressors won't be initialized yet.
    pub(crate) fn new(
        analyzer_input_data: triple_buffer::Input<AnalyzerData>,
        frozen_ir_input_data: triple_buffer::Input<FrozenIrData>,
        match_curve_runtime: Arc<MatchCurveRuntime>,
        num_channels: usize,
        max_window_size: usize,
    ) -> Self {
        let complex_buffer_len = max_window_size / 2 + 1;

        CompressorBank {
            should_update_downwards_thresholds: Arc::new(AtomicBool::new(true)),
            should_update_upwards_thresholds: Arc::new(AtomicBool::new(true)),
            should_update_downwards_ratios: Arc::new(AtomicBool::new(true)),
            should_update_upwards_ratios: Arc::new(AtomicBool::new(true)),
            should_update_downwards_knee_parabolas: Arc::new(AtomicBool::new(true)),
            should_update_upwards_knee_parabolas: Arc::new(AtomicBool::new(true)),

            ln_freqs: Vec::with_capacity(complex_buffer_len),

            downwards: PathState::new(),
            upwards: PathState::new(),

            envelope_followers_timing_scale: 0.0,
            sidechain_spectrum_magnitudes: vec![
                Vec::with_capacity(complex_buffer_len);
                num_channels
            ],
            combined_raw_gain_difference_db: Vec::with_capacity(complex_buffer_len),
            combined_smoothed_gain_difference_db: Vec::with_capacity(complex_buffer_len),
            frozen_gain_difference_db: vec![Vec::with_capacity(complex_buffer_len); num_channels],
            frozen_gain_snapshot_valid: vec![false; num_channels],
            freeze_was_active: false,
            frozen_ir_snapshot_published: false,
            window_size: 0,
            sample_rate: 1.0,

            analyzer_input_data,
            frozen_ir_input_data,
            analyzer_needs_publish: false,
            match_curve_runtime,
            match_curve_meter: MatchCurveMeter::new(complex_buffer_len),
        }
    }

    /// Change the capacities of the internal buffers to fit new parameters.
    ///
    /// After calling this, callers must invoke [`Self::resize()`] before the next processing pass.
    /// `update_capacity()` only reserves storage and does not make per-bin vectors safe to index
    /// for the active FFT size.
    pub fn update_capacity(&mut self, num_channels: usize, max_window_size: usize) {
        let complex_buffer_len = max_window_size / 2 + 1;

        self.ln_freqs
            .reserve_exact(complex_buffer_len.saturating_sub(self.ln_freqs.len()));

        self.downwards.reserve_complex_len(complex_buffer_len);
        self.upwards.reserve_complex_len(complex_buffer_len);
        self.downwards
            .reserve_envelopes(num_channels, complex_buffer_len);
        self.upwards
            .reserve_envelopes(num_channels, complex_buffer_len);

        self.sidechain_spectrum_magnitudes
            .resize_with(num_channels, Vec::new);
        for magnitudes in self.sidechain_spectrum_magnitudes.iter_mut() {
            magnitudes.reserve_exact(complex_buffer_len.saturating_sub(magnitudes.len()));
        }
        self.combined_raw_gain_difference_db.reserve_exact(
            complex_buffer_len.saturating_sub(self.combined_raw_gain_difference_db.len()),
        );
        self.combined_smoothed_gain_difference_db.reserve_exact(
            complex_buffer_len.saturating_sub(self.combined_smoothed_gain_difference_db.len()),
        );
        self.match_curve_meter.resize(complex_buffer_len);

        self.frozen_gain_difference_db
            .resize_with(num_channels, Vec::new);
        for gains in self.frozen_gain_difference_db.iter_mut() {
            gains.reserve_exact(complex_buffer_len.saturating_sub(gains.len()));
        }
        self.frozen_gain_snapshot_valid.resize(num_channels, false);
    }

    /// Resize the number of compressors to match the current window size. Also precomputes the
    /// 2-log frequencies for each bin.
    ///
    /// If the window size is larger than the maximum window size, then this will allocate.
    pub fn resize(&mut self, buffer_config: &BufferConfig, window_size: usize) {
        let complex_buffer_len = window_size / 2 + 1;

        // These 2-log frequencies are needed when updating the compressor parameters, so we'll just
        // precompute them to avoid having to repeat the same expensive computations all the time
        self.ln_freqs.resize(complex_buffer_len, 0.0);
        // The first one should always stay at zero, `0.0f32.ln() == NaN`.
        for (i, ln_freq) in self.ln_freqs.iter_mut().enumerate().skip(1) {
            let freq = (i as f32 / window_size as f32) * buffer_config.sample_rate;
            *ln_freq = freq.ln();
        }

        self.downwards.resize_complex_len(complex_buffer_len);
        self.upwards.resize_complex_len(complex_buffer_len);
        self.downwards.resize_envelopes(complex_buffer_len);
        self.upwards.resize_envelopes(complex_buffer_len);

        for magnitudes in self.sidechain_spectrum_magnitudes.iter_mut() {
            magnitudes.resize(complex_buffer_len, 0.0);
        }
        self.combined_raw_gain_difference_db
            .resize(complex_buffer_len, 0.0);
        self.combined_smoothed_gain_difference_db
            .resize(complex_buffer_len, 0.0);
        self.match_curve_meter.resize(complex_buffer_len);

        for gains in self.frozen_gain_difference_db.iter_mut() {
            gains.resize(complex_buffer_len, 0.0);
            gains.fill(0.0);
        }
        self.frozen_gain_snapshot_valid.fill(false);
        self.freeze_was_active = false;
        self.frozen_ir_snapshot_published = false;

        self.window_size = window_size;
        self.sample_rate = buffer_config.sample_rate;
        self.publish_frozen_ir_unavailable();

        // The compressors need to be updated on the next processing cycle
        self.should_update_downwards_thresholds
            .store(true, Ordering::Relaxed);
        self.should_update_upwards_thresholds
            .store(true, Ordering::Relaxed);
        self.should_update_downwards_ratios
            .store(true, Ordering::Relaxed);
        self.should_update_upwards_ratios
            .store(true, Ordering::Relaxed);
        self.should_update_downwards_knee_parabolas
            .store(true, Ordering::Relaxed);
        self.should_update_upwards_knee_parabolas
            .store(true, Ordering::Relaxed);
    }

    /// Get the sample rate this compressor bank was configured for.
    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Clear out the envelope followers.
    pub fn reset(&mut self) {
        // This will make the timings instant for the first iteration after a reset and then slowly
        // fade the timings back to their intended values so the envelope followers can settle in.
        // Otherwise suspending and resetting the plugin, or changing the window size, may result in
        // some huge spikes.
        self.envelope_followers_timing_scale = 0.0;
        self.clear_frozen_gain_snapshots(true);

        // Sidechain data doesn't need to be reset as it will be overwritten immediately before use
    }

    fn publish_frozen_ir_snapshot(&mut self, overlap_times: usize) {
        let snapshot = self.frozen_ir_input_data.input_buffer();
        snapshot.valid = true;
        snapshot.sample_rate = self.sample_rate;
        snapshot.window_size = self.window_size;
        snapshot.overlap_times = overlap_times;
        debug_assert!(self.frozen_gain_difference_db.len() <= 2);
        snapshot.num_channels = self.frozen_gain_difference_db.len().min(2);

        let num_bins = self.window_size / 2 + 1;
        for channel_idx in 0..snapshot.gain_difference_db.len() {
            if channel_idx < snapshot.num_channels {
                snapshot.gain_difference_db[channel_idx][..num_bins]
                    .copy_from_slice(&self.frozen_gain_difference_db[channel_idx][..num_bins]);
            } else {
                snapshot.gain_difference_db[channel_idx][..num_bins].fill(0.0);
            }
            snapshot.gain_difference_db[channel_idx][num_bins..].fill(0.0);
        }

        self.frozen_ir_input_data.publish();
    }

    fn publish_frozen_ir_unavailable(&mut self) {
        let snapshot = self.frozen_ir_input_data.input_buffer();
        snapshot.valid = false;
        snapshot.sample_rate = self.sample_rate;
        snapshot.window_size = self.window_size;
        snapshot.overlap_times = 0;
        debug_assert!(self.frozen_gain_difference_db.len() <= 2);
        snapshot.num_channels = self.frozen_gain_difference_db.len().min(2);
        for channel in &mut snapshot.gain_difference_db {
            channel.fill(0.0);
        }

        self.frozen_ir_input_data.publish();
    }

    /// Apply the magnitude compression to a buffer of FFT bins. The compressors are first updated
    /// if needed. The overlap amount is needed to compute the effective sample rate. The
    /// `first_non_dc_bin` argument is used to avoid upwards compression on the DC bins, or the
    /// neighbouring bins the DC signal may have been convolved into because of the Hann window
    /// function.
    pub fn process(
        &mut self,
        buffer: &mut [Complex32],
        channel_idx: usize,
        params: &SpectralCompressorParams,
        overlap_times: usize,
        first_non_dc_bin: usize,
    ) {
        assert_eq!(buffer.len(), self.ln_freqs.len());
        let freeze_enabled = params.global.compressor_freeze.value();
        // Snapshot capture works the same way in every mode, so freezing is always supported
        self.sync_freeze_state(freeze_enabled);

        // The gain difference/reduction amounts are accumulated in `self.analyzer_input_data`. When
        // processing the last channel, this data is divided by the channel count, the envelope
        // follower data is added, and the data is then sent to the editor so it can be displayed.
        // `analyzer_input_data` contains excess capacity so it can handle any supported window
        // size, so all operations on it are limited to the actual number of used bins.
        let num_bins = buffer.len();
        let num_channels = self.sidechain_spectrum_magnitudes.len();
        let should_update_analyzer_data = params.editor_state.is_open();
        if should_update_analyzer_data && channel_idx == 0 {
            // NOTE: This may briefly show a huge amount of accumulated data when the editor has
            //       just been opened. If this doesn't look too obvious or too jarring this is
            //       probably worth letting it be like this.
            let analyzer_input_data = self.analyzer_input_data.input_buffer();
            analyzer_input_data.gain_difference_db[..num_bins].fill(0.0);
        }

        self.update_if_needed(params);
        if self.match_curve_runtime.begin_running_if_requested() {
            self.match_curve_meter.start(self.sample_rate);
        }

        // The match curve meters the sidechain spectrum when any path is in a sidechain mode,
        // and the main input otherwise (the singular `measure_match_curve()` picks the source)
        self.measure_match_curve(buffer, channel_idx, params, overlap_times);

        // Each path follows its own envelope with its own attack and release times, from the
        // source its own mode dictates. The timing scale is advanced once per channel pass after
        // both paths have been updated (see `advance_envelope_timing_scale()`).
        for (path, path_params) in [
            (Path::Downwards, &params.thresholds.downwards),
            (Path::Upwards, &params.thresholds.upwards),
        ] {
            let attack_ms =
                path_params.compressor_attack_ms.value() * self.envelope_followers_timing_scale;
            let release_ms =
                path_params.compressor_release_ms.value() * self.envelope_followers_timing_scale;

            match path_params.threshold.mode.value() {
                ThresholdMode::Internal | ThresholdMode::SidechainMatch => {
                    self.update_envelopes(
                        buffer,
                        channel_idx,
                        path,
                        attack_ms,
                        release_ms,
                        overlap_times,
                    );
                }
                ThresholdMode::SidechainCompress => {
                    // This mode uses regular compression, but the envelopes are computed from the
                    // sidechain input magnitudes. These are already set in `process_sidechain`.
                    // This separate envelope updating function is needed for the channel linking.
                    self.update_envelopes_sidechain(
                        path,
                        path_params.threshold.sc_channel_link.value(),
                        channel_idx,
                        attack_ms,
                        release_ms,
                        overlap_times,
                    );
                }
            }
        }
        self.advance_envelope_timing_scale(channel_idx, overlap_times);

        // Each path compresses its own envelope with its own transfer curve, smooths its own raw
        // gain differences (without the DC clamp), and the smoothed curves are then combined
        // (with the DC clamp) into the curve that gets applied
        self.compress_path(Path::Downwards, channel_idx, params, first_non_dc_bin);
        self.compress_path(Path::Upwards, channel_idx, params, first_non_dc_bin);
        for bin_idx in 0..num_bins {
            self.combined_raw_gain_difference_db[bin_idx] = self.downwards.raw_gain_difference_db
                [bin_idx]
                + self.upwards.raw_gain_difference_db[bin_idx];
        }

        for (path, path_params) in [
            (Path::Downwards, &params.thresholds.downwards),
            (Path::Upwards, &params.thresholds.upwards),
        ] {
            let smoothing_amount = path_params.threshold.gain_smoothing.value();
            self.smooth_path_gain_differences(path, smoothing_amount, num_bins);
        }
        self.combine_and_clamp_gain_differences(first_non_dc_bin, num_bins);
        self.apply_gain_differences(
            buffer,
            channel_idx,
            freeze_enabled,
            should_update_analyzer_data,
        );

        if freeze_enabled
            && !self.frozen_ir_snapshot_published
            && self.frozen_gain_snapshot_valid.iter().all(|valid| *valid)
        {
            self.publish_frozen_ir_snapshot(overlap_times);
            self.frozen_ir_snapshot_published = true;
        }

        // When processing the last channel we can finalize the spectrum analyzer data and send it
        // to the editor for display
        if should_update_analyzer_data && channel_idx == num_channels - 1 {
            let analyzer_input_data = self.analyzer_input_data.input_buffer();

            // The editor needs to know about this too so it can draw the spectra correctly: the
            // curves and offsets are now carried per path (0 = downwards, 1 = upwards)
            analyzer_input_data.curve_params = [
                params.thresholds.downwards.threshold.curve_params(),
                params.thresholds.upwards.threshold.curve_params(),
            ];
            analyzer_input_data.curve_offsets_db = (
                params.compressors.upwards.threshold_offset_db.value(),
                params.compressors.downwards.threshold_offset_db.value(),
            );
            analyzer_input_data.num_bins = num_bins;

            // The gain reduction data needs to be averaged, see above
            let channel_multiplier = (num_channels as f32).recip();
            for gain_difference_db in &mut analyzer_input_data.gain_difference_db[..num_bins] {
                *gain_difference_db *= channel_multiplier;
            }

            // The spectrum analyzer data is carried per path: each path's channel-average
            // envelope trace goes to its own field
            assert!(self.downwards.envelopes.len() == num_channels);
            assert!(self.downwards.envelopes[0].len() >= num_bins);
            for (path, trace) in [
                (
                    Path::Downwards,
                    &mut analyzer_input_data.envelope_followers_downwards,
                ),
                (
                    Path::Upwards,
                    &mut analyzer_input_data.envelope_followers_upwards,
                ),
            ] {
                for (bin_idx, spectrum_data) in trace[..num_bins].iter_mut().enumerate() {
                    *spectrum_data = 0.0;
                    for channel_idx in 0..num_channels {
                        let path_state = match path {
                            Path::Downwards => &self.downwards,
                            Path::Upwards => &self.upwards,
                        };
                        *spectrum_data += path_state.envelopes[channel_idx][bin_idx];
                    }

                    *spectrum_data *= channel_multiplier;
                }
            }

            // After filling the object with data it can be sent to the editor. This happens
            // automatically when using the `.write()` interface, but since `AnalyzerData` contains
            // a lot of padding and we only use the first `num_bins` of the arrays that would be a
            // bit wasteful.
            // NOTE: The actual publish is deferred until after the STFT callback has finished
            //       with the compressor bank.
            self.analyzer_needs_publish = true;
        }
    }

    /// Publish analyzer data prepared during the compressor processing pass.
    pub fn publish_analyzer_if_needed(&mut self) {
        if self.analyzer_needs_publish {
            self.analyzer_input_data.publish();
            self.analyzer_needs_publish = false;
        }
    }

    /// Set the sidechain frequency spectrum magnitudes just before a [`process()`][Self::process()]
    /// call. These will be multiplied with the existing compressor thresholds and knee values to
    /// get the effective values for use with sidechaining.
    pub fn process_sidechain(&mut self, sc_buffer: &[Complex32], channel_idx: usize) {
        nih_debug_assert_eq!(sc_buffer.len(), self.ln_freqs.len());

        self.update_sidechain_spectra(sc_buffer, channel_idx);
    }

    /// Update the envelope followers for a single compression path from the bin magnitudes in
    /// `magnitude_bins`. The attack and release times are passed by the caller since they control
    /// this path's own envelope follower.
    fn update_envelopes(
        &mut self,
        magnitude_bins: &[Complex32],
        channel_idx: usize,
        path: Path,
        attack_ms: f32,
        release_ms: f32,
        overlap_times: usize,
    ) {
        let effective_sample_rate =
            self.sample_rate / (self.window_size as f32 / overlap_times as f32);

        let (attack_old_t, attack_new_t, release_old_t, release_new_t) =
            envelope_follower_coefficients(effective_sample_rate, attack_ms, release_ms);

        let path_state = match path {
            Path::Downwards => &mut self.downwards,
            Path::Upwards => &mut self.upwards,
        };
        for (bin, envelope) in magnitude_bins
            .iter()
            .zip(path_state.envelopes[channel_idx].iter_mut())
        {
            let magnitude = bin.norm();
            if *envelope > magnitude {
                // Release stage
                *envelope = (release_old_t * *envelope) + (release_new_t * magnitude);
            } else {
                // Attack stage
                *envelope = (attack_old_t * *envelope) + (attack_new_t * magnitude);
            }
        }
    }

    /// The same as [`update_envelopes()`][Self::update_envelopes()], but based on the previously
    /// set sidechain bin magnitudes. This allows for channel linking.
    /// [`process_sidechain()`][Self::process_sidechain()] needs to be called for all channels
    /// before this function can be used to set the magnitude spectra.
    fn update_envelopes_sidechain(
        &mut self,
        path: Path,
        link_amount: f32,
        channel_idx: usize,
        attack_ms: f32,
        release_ms: f32,
        overlap_times: usize,
    ) {
        let effective_sample_rate =
            self.sample_rate / (self.window_size as f32 / overlap_times as f32);

        let (attack_old_t, attack_new_t, release_old_t, release_new_t) =
            envelope_follower_coefficients(effective_sample_rate, attack_ms, release_ms);

        // For the channel linking
        let num_channels = self.sidechain_spectrum_magnitudes.len() as f32;
        let other_channels_t = link_amount / num_channels;
        let this_channel_t = 1.0 - (other_channels_t * (num_channels - 1.0));

        let path_state = match path {
            Path::Downwards => &mut self.downwards,
            Path::Upwards => &mut self.upwards,
        };
        for (bin_idx, envelope) in path_state.envelopes[channel_idx].iter_mut().enumerate() {
            // In this mode the envelopes are set based on the sidechain signal, taking channel
            // linking into account
            let sidechain_magnitude: f32 = self
                .sidechain_spectrum_magnitudes
                .iter()
                .enumerate()
                .map(|(sidechain_channel_idx, magnitudes)| {
                    let t = if sidechain_channel_idx == channel_idx {
                        this_channel_t
                    } else {
                        other_channels_t
                    };

                    magnitudes[bin_idx] * t
                })
                .sum::<f32>();

            if *envelope > sidechain_magnitude {
                // Release stage
                *envelope = (release_old_t * *envelope) + (release_new_t * sidechain_magnitude);
            } else {
                // Attack stage
                *envelope = (attack_old_t * *envelope) + (attack_new_t * sidechain_magnitude);
            }
        }
    }

    /// Slowly fade the envelope follower timing scale back to 1.0 after a reset. Must run once per
    /// channel pass (not once per path) to keep the legacy timing behaviour.
    fn advance_envelope_timing_scale(&mut self, channel_idx: usize, overlap_times: usize) {
        if self.envelope_followers_timing_scale >= 1.0 {
            return;
        }

        let effective_sample_rate =
            self.sample_rate / (self.window_size as f32 / overlap_times as f32);
        if channel_idx == self.downwards.envelopes.len() - 1 {
            let delta =
                ((ENVELOPE_FOLLOWER_TIMING_FADE_MS / 1000.0) * effective_sample_rate).recip();
            self.envelope_followers_timing_scale =
                (self.envelope_followers_timing_scale + delta).min(1.0);
        }
    }

    /// Update the spectral data using the sidechain input
    fn update_sidechain_spectra(&mut self, sc_buffer: &[Complex32], channel_idx: usize) {
        nih_debug_assert!(channel_idx < self.sidechain_spectrum_magnitudes.len());

        for (bin, magnitude) in sc_buffer
            .iter()
            .zip(self.sidechain_spectrum_magnitudes[channel_idx].iter_mut())
        {
            *magnitude = bin.norm();
        }
    }

    /// Meter the audio signal for the curve matching. Per the spec the source is the sidechain
    /// spectrum when any path is in a sidechain mode, and the main input otherwise; the linked
    //  magnitudes then use the first sidechain path's channel link, with downwards preferred.
    fn measure_match_curve(
        &mut self,
        main_buffer: &[Complex32],
        channel_idx: usize,
        params: &SpectralCompressorParams,
        overlap_times: usize,
    ) {
        if !self.match_curve_meter.is_active() {
            return;
        }

        let first_sidechain_link = [&params.thresholds.downwards, &params.thresholds.upwards]
            .iter()
            .find(|path_params| {
                !matches!(path_params.threshold.mode.value(), ThresholdMode::Internal)
            })
            .map(|path_params| path_params.threshold.sc_channel_link.value());

        match first_sidechain_link {
            Some(link_amount) => {
                let num_channels = self.sidechain_spectrum_magnitudes.len() as f32;
                let other_channels_t = link_amount / num_channels;
                let this_channel_t = 1.0 - (other_channels_t * (num_channels - 1.0));

                self.match_curve_meter
                    .measure_magnitudes((0..self.ln_freqs.len()).map(|bin_idx| {
                        self.sidechain_spectrum_magnitudes
                            .iter()
                            .enumerate()
                            .map(|(sidechain_channel_idx, magnitudes)| {
                                let t = if sidechain_channel_idx == channel_idx {
                                    this_channel_t
                                } else {
                                    other_channels_t
                                };

                                magnitudes[bin_idx] * t
                            })
                            .sum::<f32>()
                    }));
            }
            None => {
                self.match_curve_meter
                    .measure_magnitudes(main_buffer.iter().map(|bin| bin.norm()));
            }
        }
        self.finish_match_curve_channel_if_needed(channel_idx, params, overlap_times);
    }

    fn finish_match_curve_channel_if_needed(
        &mut self,
        channel_idx: usize,
        params: &SpectralCompressorParams,
        overlap_times: usize,
    ) {
        if channel_idx != self.sidechain_spectrum_magnitudes.len() - 1 {
            return;
        }

        let hop_frames = (self.window_size / overlap_times).max(1);
        let threshold = &params.thresholds.downwards.threshold;
        let fixed_slope = match threshold.mode.value() {
            ThresholdMode::Internal => internal_threshold_slope(threshold.curve_slope.value()),
            ThresholdMode::SidechainMatch | ThresholdMode::SidechainCompress => {
                threshold.curve_slope.value()
            }
        };
        if let Some(result) = self.match_curve_meter.advance_and_finish(
            hop_frames,
            &self.ln_freqs,
            params.compressors.downwards.threshold_offset_db.value(),
            threshold.center_frequency.value(),
            fixed_slope,
        ) {
            self.match_curve_runtime.publish_result(result);
        }
    }

    /// Actually do the thing for a single compression path. [`Self::update_envelopes()`] or
    /// [`Self::update_envelopes_sidechain()`] must have been called for this path before calling
    /// this. In sidechain-match mode the path's thresholds and knee values are scaled by the
    /// sidechain signal. The mode and channel link setting are read from the path's own
    /// parameters.
    ///
    /// # Panics
    ///
    /// Panics if the buffer does not have the same length as the one that was passed to the last
    /// `resize()` call.
    fn compress_path(
        &mut self,
        path: Path,
        channel_idx: usize,
        params: &SpectralCompressorParams,
        first_non_dc_bin: usize,
    ) {
        let path_params = match path {
            Path::Downwards => &params.thresholds.downwards,
            Path::Upwards => &params.thresholds.upwards,
        };
        let mode = path_params.threshold.mode.value();
        let knee_width_db = match path {
            Path::Downwards => params.compressors.downwards.knee_width_db.value(),
            Path::Upwards => params.compressors.upwards.knee_width_db.value(),
        };
        let upwards_max_gain_change_db = params.compressors.upwards.max_gain_change_db.value();
        let num_bins = self.ln_freqs.len();

        assert!(self.downwards.thresholds_db.len() == num_bins);
        assert!(self.downwards.ratios.len() == num_bins);
        assert!(self.upwards.thresholds_db.len() == num_bins);
        assert!(self.upwards.ratios.len() == num_bins);
        assert!(self.combined_raw_gain_difference_db.len() >= num_bins);

        // In sidechain-match mode the compressor thresholds/knee values are scaled by the
        // sidechain signal, thus sort of creating a dynamic multiband compressor. This uses the
        // path's own channel link setting.
        let sidechain = mode == ThresholdMode::SidechainMatch;
        if sidechain {
            let num_channels = self.sidechain_spectrum_magnitudes.len() as f32;
            let other_channels_t = path_params.threshold.sc_channel_link.value() / num_channels;
            let this_channel_t = 1.0 - (other_channels_t * (num_channels - 1.0));

            assert!(self.sidechain_spectrum_magnitudes[channel_idx].len() >= num_bins);
            let path_state = match path {
                Path::Downwards => &mut self.downwards,
                Path::Upwards => &mut self.upwards,
            };
            for (bin_idx, linked_magnitude) in path_state.linked_sidechain_magnitudes[..num_bins]
                .iter_mut()
                .enumerate()
            {
                *linked_magnitude = self
                    .sidechain_spectrum_magnitudes
                    .iter()
                    .enumerate()
                    .map(|(sidechain_channel_idx, magnitudes)| {
                        let t = if sidechain_channel_idx == channel_idx {
                            this_channel_t
                        } else {
                            other_channels_t
                        };

                        magnitudes[bin_idx] * t
                    })
                    .sum::<f32>()
                    // The thresholds may never reach zero as they are used in divisions.
                    .max(f32::EPSILON);
            }
        }

        for bin_idx in 0..num_bins {
            let envelope = match path {
                Path::Downwards => self.downwards.envelopes[channel_idx][bin_idx],
                Path::Upwards => self.upwards.envelopes[channel_idx][bin_idx],
            };
            // Bypass the bin entirely when it is silent (see SILENCE_THRESHOLD_GAIN): with no
            // signal there is nothing to compress. The gain smoother then decays back to
            // baseline during the pause, so the next incoming signal starts from unity gain
            // instead of inheriting the accumulated (capped) upwards boost. In sidechain
            // compression mode the envelopes are computed from the sidechain signal, so this
            // gates on the envelope the path itself follows.
            if envelope <= SILENCE_THRESHOLD_GAIN {
                match path {
                    Path::Downwards => self.downwards.raw_gain_difference_db[bin_idx] = 0.0,
                    Path::Upwards => self.upwards.raw_gain_difference_db[bin_idx] = 0.0,
                }
                continue;
            }

            // We'll apply the transfer curve to the envelope signal, and then scale the complex
            // `bin` by the gain difference
            let envelope_db = util::gain_to_db_fast_epsilon(envelope);

            // Notice how the thresholds and knee values are scaled in sidechain-match mode. Because
            // the thresholds are scaled based on the sidechain input, we also need to recompute the
            // knee coefficients.
            let (threshold_db, knee_parabola_scale, knee_parabola_intercept) = if sidechain {
                let sidechain_scale_db = util::gain_to_db_fast_epsilon(match path {
                    Path::Downwards => self.downwards.linked_sidechain_magnitudes[bin_idx],
                    Path::Upwards => self.upwards.linked_sidechain_magnitudes[bin_idx],
                });
                let threshold_db = match path {
                    Path::Downwards => self.downwards.thresholds_db[bin_idx],
                    Path::Upwards => self.upwards.thresholds_db[bin_idx],
                };
                let threshold_db = (threshold_db + sidechain_scale_db).max(util::MINUS_INFINITY_DB);
                let ratio = match path {
                    Path::Downwards => self.downwards.ratios[bin_idx],
                    Path::Upwards => self.upwards.ratios[bin_idx],
                };
                let coefficients = match path {
                    Path::Downwards => {
                        downwards_soft_knee_coefficients(threshold_db, knee_width_db, ratio)
                    }
                    Path::Upwards => {
                        upwards_soft_knee_coefficients(threshold_db, knee_width_db, ratio)
                    }
                };

                (threshold_db, coefficients.0, coefficients.1)
            } else {
                match path {
                    Path::Downwards => (
                        self.downwards.thresholds_db[bin_idx],
                        self.downwards.knee_parabola_scale[bin_idx],
                        self.downwards.knee_parabola_intercept[bin_idx],
                    ),
                    Path::Upwards => (
                        self.upwards.thresholds_db[bin_idx],
                        self.upwards.knee_parabola_scale[bin_idx],
                        self.upwards.knee_parabola_intercept[bin_idx],
                    ),
                }
            };
            let ratio = match path {
                Path::Downwards => self.downwards.ratios[bin_idx],
                Path::Upwards => self.upwards.ratios[bin_idx],
            };

            let compressed = match path {
                // Upwards compression should not happen when the signal is _too_ quiet as we'd
                // only be amplifying noise. We also don't want to amplify DC noise and super low
                // frequencies.
                Path::Upwards
                    if bin_idx >= first_non_dc_bin
                        && ratio != 1.0
                        && envelope_db > util::MINUS_INFINITY_DB =>
                {
                    compress_upwards(
                        envelope_db,
                        threshold_db,
                        ratio,
                        knee_width_db,
                        knee_parabola_scale,
                        knee_parabola_intercept,
                    )
                    // Without this cap the upwards compressor keeps amplifying its slowly
                    // releasing envelope in silence: the release ramps to the -100 dB floor which
                    // sits ever further below the threshold, so the gain difference keeps rising
                    // 'on its own' and spikes when the signal comes back. The gain difference
                    // relative to the envelope is capped since the envelope itself should remain
                    // untouched.
                    .min(envelope_db + upwards_max_gain_change_db)
                }
                Path::Upwards => envelope_db,
                Path::Downwards => compress_downwards(
                    envelope_db,
                    threshold_db,
                    ratio,
                    knee_width_db,
                    knee_parabola_scale,
                    knee_parabola_intercept,
                ),
            };

            // If the compressed output is -10 dBFS and the envelope follower was at -6 dBFS, then
            // we want to apply -4 dB of gain to the bin
            match path {
                Path::Downwards => {
                    self.downwards.raw_gain_difference_db[bin_idx] = compressed - envelope_db
                }
                Path::Upwards => {
                    self.upwards.raw_gain_difference_db[bin_idx] = compressed - envelope_db
                }
            }
        }
    }

    fn smooth_path_gain_differences(&mut self, path: Path, smoothing_amount: f32, num_bins: usize) {
        let path_state = match path {
            Path::Downwards => &mut self.downwards,
            Path::Upwards => &mut self.upwards,
        };
        assert!(path_state.raw_gain_difference_db.len() >= num_bins);
        assert!(path_state.smoothed_gain_difference_db.len() >= num_bins);
        assert!(path_state.gain_smoothing_prefix_db.len() > num_bins);
        assert!(self.ln_freqs.len() >= num_bins);

        let smoothing_amount = smoothing_amount.clamp(0.0, 1.0);
        if smoothing_amount <= f32::EPSILON || num_bins <= 2 {
            path_state.smoothed_gain_difference_db[..num_bins]
                .copy_from_slice(&path_state.raw_gain_difference_db[..num_bins]);
            return;
        }

        path_state.gain_smoothing_prefix_db[0] = 0.0;
        for bin_idx in 0..num_bins {
            path_state.gain_smoothing_prefix_db[bin_idx + 1] = path_state.gain_smoothing_prefix_db
                [bin_idx]
                + path_state.raw_gain_difference_db[bin_idx];
        }

        path_state.smoothed_gain_difference_db[0] = path_state.raw_gain_difference_db[0];

        let radius_ln = smoothing_amount * GAIN_SMOOTHING_MAX_RADIUS_LN;
        let mut left_idx = 1;
        let mut right_idx = 1;
        for bin_idx in 1..num_bins {
            let center_ln = self.ln_freqs[bin_idx];

            while left_idx < bin_idx && center_ln - self.ln_freqs[left_idx] > radius_ln {
                left_idx += 1;
            }

            right_idx = right_idx.max(bin_idx);
            while right_idx + 1 < num_bins && self.ln_freqs[right_idx + 1] - center_ln <= radius_ln
            {
                right_idx += 1;
            }

            let sum = path_state.gain_smoothing_prefix_db[right_idx + 1]
                - path_state.gain_smoothing_prefix_db[left_idx];
            let count = (right_idx - left_idx + 1) as f32;
            let averaged = sum / count;
            let raw = path_state.raw_gain_difference_db[bin_idx];
            let smoothed = raw + ((averaged - raw) * smoothing_amount);

            path_state.smoothed_gain_difference_db[bin_idx] = smoothed;
        }
    }

    /// Sum the per-path smoothed gain differences into the combined smoothed curve that the
    /// gain application consumes. Positive values below `first_non_dc_bin` are clamped to 0.0 so
    /// a smoothed boost never turns into a low-frequency upwards gain.
    fn combine_and_clamp_gain_differences(&mut self, first_non_dc_bin: usize, num_bins: usize) {
        assert!(self.downwards.smoothed_gain_difference_db.len() >= num_bins);
        assert!(self.upwards.smoothed_gain_difference_db.len() >= num_bins);
        assert!(self.combined_smoothed_gain_difference_db.len() >= num_bins);

        for bin_idx in 0..num_bins {
            let combined = self.downwards.smoothed_gain_difference_db[bin_idx]
                + self.upwards.smoothed_gain_difference_db[bin_idx];
            self.combined_smoothed_gain_difference_db[bin_idx] =
                if bin_idx < first_non_dc_bin && combined > 0.0 {
                    0.0
                } else {
                    combined
                };
        }
    }

    fn apply_gain_differences(
        &mut self,
        buffer: &mut [Complex32],
        channel_idx: usize,
        freeze_enabled: bool,
        should_update_analyzer_data: bool,
    ) {
        assert!(self.frozen_gain_difference_db[channel_idx].len() == buffer.len());
        assert!(self.combined_smoothed_gain_difference_db.len() >= buffer.len());

        let should_capture_snapshot =
            freeze_enabled && !self.frozen_gain_snapshot_valid[channel_idx];

        {
            // The gain reduction values are always added to the arrays stored in this object. This
            // makes it possible to visualize the gain reduction without a lot of conditionals.
            let analyzer_input_data = self.analyzer_input_data.input_buffer();
            assert!(analyzer_input_data.gain_difference_db.len() >= buffer.len());
            let frozen_gain_difference_db = &mut self.frozen_gain_difference_db[channel_idx];
            let smoothed_gain_difference_db =
                &self.combined_smoothed_gain_difference_db[..buffer.len()];

            for (bin_idx, bin) in buffer.iter_mut().enumerate() {
                let smoothed_gain_difference_db = smoothed_gain_difference_db[bin_idx];
                let applied_gain_difference_db = if freeze_enabled {
                    if should_capture_snapshot {
                        frozen_gain_difference_db[bin_idx] = smoothed_gain_difference_db;
                    }

                    frozen_gain_difference_db[bin_idx]
                } else {
                    smoothed_gain_difference_db
                };
                if should_update_analyzer_data {
                    analyzer_input_data.gain_difference_db[bin_idx] += applied_gain_difference_db;
                }

                *bin *= util::db_to_gain_fast(applied_gain_difference_db);
            }
        }

        if should_capture_snapshot {
            self.frozen_gain_snapshot_valid[channel_idx] = true;
        }
    }

    fn sync_freeze_state(&mut self, freeze_enabled: bool) {
        if freeze_enabled {
            if !self.freeze_was_active {
                self.clear_frozen_gain_snapshots(true);
            }
        } else if self.freeze_was_active
            || self.frozen_gain_snapshot_valid.iter().any(|valid| *valid)
        {
            self.clear_frozen_gain_snapshots(false);
        }
    }

    fn clear_frozen_gain_snapshots(&mut self, freeze_active: bool) {
        for gains in &mut self.frozen_gain_difference_db {
            gains.fill(0.0);
        }
        self.frozen_gain_snapshot_valid.fill(false);
        self.freeze_was_active = freeze_active;
        self.frozen_ir_snapshot_published = false;
        self.publish_frozen_ir_unavailable();
    }

    /// Update the compressors if needed. This is called just before processing, and the compressors
    /// are updated in accordance to the atomic flags set on this struct. Each compression path
    /// recomputes its state from its own parameters and its own dirty flags; a change to one
    /// path's parameters only touches that path.
    fn update_if_needed(&mut self, params: &SpectralCompressorParams) {
        // Owned clones of the dirty flags and parameter arcs so the per-path mutable state can be
        // borrowed below without holding a borrow of `self`
        let paths: [PathUpdate; 2] = [
            (
                Path::Downwards,
                self.should_update_downwards_thresholds.clone(),
                self.should_update_downwards_ratios.clone(),
                self.should_update_downwards_knee_parabolas.clone(),
                params.thresholds.downwards.clone(),
                params.compressors.downwards.clone(),
                downwards_soft_knee_coefficients,
            ),
            (
                Path::Upwards,
                self.should_update_upwards_thresholds.clone(),
                self.should_update_upwards_ratios.clone(),
                self.should_update_upwards_knee_parabolas.clone(),
                params.thresholds.upwards.clone(),
                params.compressors.upwards.clone(),
                upwards_soft_knee_coefficients,
            ),
        ];

        for (
            path,
            should_update_thresholds,
            should_update_ratios,
            should_update_knee_parabolas,
            path_params,
            compressor_params,
            knee_coefficients_fn,
        ) in paths
        {
            // Borrowed directly (not via `path_state()`) so the borrow checker can see the
            // disjoint field borrows of `self` below
            let path_state = match path {
                Path::Downwards => &mut self.downwards,
                Path::Upwards => &mut self.upwards,
            };

            if should_update_thresholds
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // The threshold curve is a polynomial in log-log (decibels-octaves) space
                let curve_params = path_params.threshold.curve_params();
                let curve = Curve::new(&curve_params);
                let intercept = compressor_params.threshold_offset_db.value();
                for (ln_freq, threshold_db) in self
                    .ln_freqs
                    .iter()
                    .zip(path_state.thresholds_db.iter_mut())
                {
                    *threshold_db = curve.evaluate_ln(*ln_freq) + intercept;
                }
            }

            if should_update_ratios
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // If the high-frequency rolloff is enabled then higher frequency bins will have
                // their ratios reduced to reduce harshness. This follows the octave scale. It's
                // easier to do this cleanly using reciprocals.
                let target_ratio_recip = compressor_params.ratio.value().recip();
                let high_freq_ratio_rolloff = compressor_params.high_freq_ratio_rolloff.value();
                for (ln_freq, ratio) in self.ln_freqs.iter().zip(path_state.ratios.iter_mut()) {
                    // Clamp to avoid negative low-frequency values in edge cases.
                    let octave_fraction = (ln_freq / HIGH_FREQ_RATIO_ROLLOFF_FREQUENCY_LN).max(0.0);
                    let rolloff_t = octave_fraction * high_freq_ratio_rolloff;

                    // If the octave fraction times the rolloff amount is high, then this should get
                    // closer to `high_freq_ratio_rolloff` (which is in [0, 1]).
                    let ratio_recip = (target_ratio_recip * (1.0 - rolloff_t)) + rolloff_t;
                    *ratio = ratio_recip.recip();
                }
            }

            if should_update_knee_parabolas
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let knee_width_db = compressor_params.knee_width_db.value();
                for ((ratio, threshold_db), (knee_parabola_scale, knee_parambola_intercept)) in
                    path_state
                        .ratios
                        .iter()
                        .zip(path_state.thresholds_db.iter())
                        .zip(
                            path_state
                                .knee_parabola_scale
                                .iter_mut()
                                .zip(path_state.knee_parabola_intercept.iter_mut()),
                        )
                {
                    // This is the formula from the Digital Dynamic Range Compressor Design paper
                    // by Dimitrios Giannoulis et. al. These are `a` and `b` from the
                    // `x + a * (x + b)^2` respectively used to compute the soft knee respectively.
                    // The upwards version is slightly different, hence the function pointer.
                    (*knee_parabola_scale, *knee_parambola_intercept) =
                        knee_coefficients_fn(*threshold_db, knee_width_db, *ratio);
                }
            }
        }
    }

    /// Get the mutable per-bin state for a single compression path.
    // Will be used by the per-path envelope and gain processing from Task 3 onward
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn path_state(&mut self, path: Path) -> &mut PathState {
        match path {
            Path::Downwards => &mut self.downwards,
            Path::Upwards => &mut self.upwards,
        }
    }
}

/// Compute the envelope follower's transfer coefficients for the given attack and release times.
/// Returns `(attack_old_t, attack_new_t, release_old_t, release_new_t)`. The coefficient the old
/// envelope value is multiplied by when the current rectified sample value is above the envelope's
/// value. The 0 to 1 step response retains 36.8% of the old value after the attack time has
/// elapsed, and the current value is 63.2% of the way towards 1. The effective sample rate needs
/// to compensate for the periodic nature of the STFT operation. Since with a 2048 sample window
/// and 4x overlap, you'd run this once for every 512 samples.
fn envelope_follower_coefficients(
    effective_sample_rate: f32,
    attack_ms: f32,
    release_ms: f32,
) -> (f32, f32, f32, f32) {
    let attack_old_t = if attack_ms == 0.0 {
        0.0
    } else {
        (-1.0 / (attack_ms / 1000.0 * effective_sample_rate)).exp()
    };
    let attack_new_t = 1.0 - attack_old_t;
    // The same as `attack_old_t`, but for the release phase of the envelope follower
    let release_old_t = if release_ms == 0.0 {
        0.0
    } else {
        (-1.0 / (release_ms / 1000.0 * effective_sample_rate)).exp()
    };
    let release_new_t = 1.0 - release_old_t;

    (attack_old_t, attack_new_t, release_old_t, release_new_t)
}

/// Apply downwards compression to the input with the supplied parameters. All values are in
/// decibels.
fn compress_downwards(
    input_db: f32,
    threshold_db: f32,
    ratio: f32,
    knee_width_db: f32,
    knee_parabola_scale: f32,
    knee_parabola_intercept: f32,
) -> f32 {
    // The soft-knee option will fade in the compression curve when reaching the knee start until it
    // matches the hard-knee curve at the knee-end
    let knee_start_db = threshold_db - (knee_width_db / 2.0);
    let knee_end_db = threshold_db + (knee_width_db / 2.0);
    if input_db <= knee_start_db {
        input_db
    } else if input_db <= knee_end_db {
        // See the `knee_parabola_intercept` field documentation for the full formula. The entire
        // osft knee part can be skipped if `knee_width_db == 0.0`.
        let parabola_x = input_db + knee_parabola_intercept;
        input_db + (knee_parabola_scale * parabola_x * parabola_x)
    } else {
        threshold_db + ((input_db - threshold_db) / ratio)
    }
}

/// Apply upwards compression to the input with the supplied parameters. All values are in
/// decibels.
fn compress_upwards(
    input_db: f32,
    threshold_db: f32,
    ratio: f32,
    knee_width_db: f32,
    knee_parabola_scale: f32,
    knee_parabola_intercept: f32,
) -> f32 {
    // We'll keep the terminology consistent, start is below the threshold, and end is above the
    // threshold
    let knee_start_db = threshold_db - (knee_width_db / 2.0);
    let knee_end_db = threshold_db + (knee_width_db / 2.0);

    // This goes the other way around compared to the downwards compression
    if input_db >= knee_end_db {
        input_db
    } else if input_db >= knee_start_db {
        let parabola_x = input_db + knee_parabola_intercept;
        input_db + (knee_parabola_scale * parabola_x * parabola_x)
    } else {
        threshold_db + ((input_db - threshold_db) / ratio)
    }
}

/// Compute the `(scale, intercept)`/`(a, b)` coefficients for the parabolic formula `x + a * (x +
/// b)^2`. The formula is taken from the Digital Dynamic Range Compressor Design paper by Dimitrios
/// Giannoulis et. al. This version applies to downwards compression. It can be precalculated for
/// the regular modes, since it's dependent on the threshold it has to be recomputed for every
/// sample with the sidechain matching mode.
fn downwards_soft_knee_coefficients(
    threshold_db: f32,
    knee_width_db: f32,
    ratio: f32,
) -> (f32, f32) {
    let scale = if knee_width_db != 0.0 {
        (2.0 * knee_width_db * ratio).recip() - (2.0 * knee_width_db).recip()
    } else {
        1.0
    };
    let intercept = -threshold_db + (knee_width_db / 2.0);

    (scale, intercept)
}

/// [`downwards_soft_knee_coefficients()`], but for upwards compression.
fn upwards_soft_knee_coefficients(threshold_db: f32, knee_width_db: f32, ratio: f32) -> (f32, f32) {
    // For the upwards version the scale becomes negated
    let scale = if knee_width_db != 0.0 {
        -((2.0 * knee_width_db * ratio).recip() - (2.0 * knee_width_db).recip())
    } else {
        1.0
    };
    // And the `+ (knee/2)` becomes `- (knee/2)` in the intercept
    let intercept = -threshold_db - (knee_width_db / 2.0);

    (scale, intercept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::match_curve::MatchCurveResult;
    use nih_plug::params::ParamMut;
    use nih_plug::prelude::{BufferConfig, ProcessMode};
    use triple_buffer::TripleBuffer;

    fn test_buffer_config(window_size: usize) -> BufferConfig {
        BufferConfig {
            sample_rate: 48_000.0,
            min_buffer_size: None,
            max_buffer_size: window_size as u32,
            process_mode: ProcessMode::Realtime,
        }
    }

    fn make_bank_and_params(
        num_channels: usize,
        window_size: usize,
    ) -> (
        CompressorBank,
        SpectralCompressorParams,
        triple_buffer::Output<FrozenIrData>,
    ) {
        let (bank, params, frozen_ir_output_data, _analyzer_output_data) =
            make_bank_and_params_with_analyzer(num_channels, window_size);

        (bank, params, frozen_ir_output_data)
    }

    /// The same as [`make_bank_and_params()`], but the analyzer output triple buffer is also
    /// returned so tests can read the published analyzer data.
    fn make_bank_and_params_with_analyzer(
        num_channels: usize,
        window_size: usize,
    ) -> (
        CompressorBank,
        SpectralCompressorParams,
        triple_buffer::Output<FrozenIrData>,
        triple_buffer::Output<AnalyzerData>,
    ) {
        let (analyzer_input_data, analyzer_output_data) =
            TripleBuffer::<AnalyzerData>::default().split();
        let (frozen_ir_input_data, frozen_ir_output_data) =
            TripleBuffer::<FrozenIrData>::default().split();
        let match_curve_runtime = Arc::new(MatchCurveRuntime::new());
        let mut compressor_bank = CompressorBank::new(
            analyzer_input_data,
            frozen_ir_input_data,
            match_curve_runtime,
            num_channels,
            window_size,
        );
        let buffer_config = test_buffer_config(window_size);
        compressor_bank.update_capacity(num_channels, window_size);
        compressor_bank.resize(&buffer_config, window_size);
        configure_test_downwards_compression(&mut compressor_bank, -24.0, 4.0, 0.0);

        let params = SpectralCompressorParams::new(&compressor_bank);

        (
            compressor_bank,
            params,
            frozen_ir_output_data,
            analyzer_output_data,
        )
    }

    fn configure_test_downwards_compression(
        compressor_bank: &mut CompressorBank,
        threshold_db: f32,
        ratio: f32,
        knee_width_db: f32,
    ) {
        let (knee_scale, knee_intercept) =
            downwards_soft_knee_coefficients(threshold_db, knee_width_db, ratio);

        compressor_bank.downwards.thresholds_db.fill(threshold_db);
        compressor_bank.downwards.ratios.fill(ratio);
        compressor_bank
            .downwards
            .knee_parabola_scale
            .fill(knee_scale);
        compressor_bank
            .downwards
            .knee_parabola_intercept
            .fill(knee_intercept);
        compressor_bank.upwards.thresholds_db.fill(0.0);
        compressor_bank.upwards.ratios.fill(1.0);
        compressor_bank.upwards.knee_parabola_scale.fill(0.0);
        compressor_bank.upwards.knee_parabola_intercept.fill(0.0);
    }

    fn make_complex_buffer(scale: f32, len: usize) -> Vec<Complex32> {
        (0..len)
            .map(|bin_idx| Complex32::new(scale * (bin_idx as f32 + 1.0), 0.0))
            .collect()
    }

    fn assert_snapshot_differs(lhs: &[f32], rhs: &[f32]) {
        assert!(
            lhs.iter()
                .zip(rhs.iter())
                .any(|(left, right)| (left - right).abs() > 1.0e-4),
            "expected snapshots to differ"
        );
    }

    fn assert_slice_near(lhs: &[f32], rhs: &[f32]) {
        assert_eq!(lhs.len(), rhs.len());
        for (idx, (left, right)) in lhs.iter().zip(rhs).enumerate() {
            assert!(
                (*left - *right).abs() < 1.0e-5,
                "mismatch at {idx}: {left} != {right}"
            );
        }
    }

    fn run_with_large_stack(test_fn: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(test_fn)
            .expect("failed to spawn test thread")
            .join()
            .expect("test thread panicked");
    }

    /// Legacy-style envelope update helper: follows the main-input magnitudes for both paths with
    /// the shared envelope timings (transitional until per-path modes land in Task 4).
    fn legacy_update_envelopes(
        bank: &mut CompressorBank,
        magnitudes: &[Complex32],
        channel_idx: usize,
        params: &SpectralCompressorParams,
        overlap_times: usize,
    ) {
        let timing_scale = bank.envelope_followers_timing_scale;
        let attack_ms = params.thresholds.downwards.compressor_attack_ms.value() * timing_scale;
        let release_ms = params.thresholds.downwards.compressor_release_ms.value() * timing_scale;
        bank.update_envelopes(
            magnitudes,
            channel_idx,
            Path::Downwards,
            attack_ms,
            release_ms,
            overlap_times,
        );
        bank.update_envelopes(
            magnitudes,
            channel_idx,
            Path::Upwards,
            attack_ms,
            release_ms,
            overlap_times,
        );
        bank.advance_envelope_timing_scale(channel_idx, overlap_times);
    }

    /// Legacy-style compression pass helper: compresses both paths with the shared mode, sums the
    /// per-path raw gain differences, and runs smoothing and gain application just like `process`.
    fn legacy_compress_all(
        bank: &mut CompressorBank,
        buffer: &mut [Complex32],
        channel_idx: usize,
        params: &SpectralCompressorParams,
        first_non_dc_bin: usize,
        freeze_enabled: bool,
        should_update_analyzer_data: bool,
    ) {
        let num_bins = buffer.len();
        bank.compress_path(Path::Downwards, channel_idx, params, first_non_dc_bin);
        bank.compress_path(Path::Upwards, channel_idx, params, first_non_dc_bin);
        for bin_idx in 0..num_bins {
            bank.combined_raw_gain_difference_db[bin_idx] = bank.downwards.raw_gain_difference_db
                [bin_idx]
                + bank.upwards.raw_gain_difference_db[bin_idx];
        }
        // Shared smoothing amount in the legacy configuration (both paths read the same value)
        let smoothing_amount = params.thresholds.downwards.threshold.gain_smoothing.value();
        bank.smooth_path_gain_differences(Path::Downwards, smoothing_amount, num_bins);
        bank.smooth_path_gain_differences(Path::Upwards, smoothing_amount, num_bins);
        bank.combine_and_clamp_gain_differences(first_non_dc_bin, num_bins);
        bank.apply_gain_differences(
            buffer,
            channel_idx,
            freeze_enabled,
            should_update_analyzer_data,
        );
    }

    #[test]
    fn per_path_attack_release() {
        run_with_large_stack(|| {
            let (mut bank, _params, _ir) = make_bank_and_params(1, 64);

            // A signal with a slow attack envelope: 1 ms attack tracks the input quickly, a very
            // long attack barely moves the envelope from its initial value
            let magnitudes: Vec<Complex32> = (0..33)
                .map(|bin_idx| Complex32::new((bin_idx + 1) as f32 * 0.1, 0.0))
                .collect();

            bank.update_envelopes(&magnitudes, 0, Path::Downwards, 1.0, 1.0, 4);
            bank.update_envelopes(&magnitudes, 0, Path::Upwards, 100_000.0, 1.0, 4);

            for (down_envelope, up_envelope) in bank.downwards.envelopes[0]
                .iter()
                .zip(&bank.upwards.envelopes[0])
            {
                assert!(down_envelope > up_envelope);
            }
        });
    }

    #[test]
    fn legacy_pipeline_matches_two_paths_equally() {
        run_with_large_stack(|| {
            let window_size = 64;
            let num_bins = window_size / 2 + 1;
            let (mut bank, params, _ir) = make_bank_and_params(1, window_size);

            // Both paths get the same envelope timings and the same (helper-configured)
            // compressor state, so the per-path pipeline must be exactly the legacy one
            let buffer = make_complex_buffer(1.0, num_bins);
            let timing_scale = bank.envelope_followers_timing_scale;
            let attack_ms = params.thresholds.downwards.compressor_attack_ms.value() * timing_scale;
            let release_ms =
                params.thresholds.downwards.compressor_release_ms.value() * timing_scale;
            bank.update_envelopes(&buffer, 0, Path::Downwards, attack_ms, release_ms, 4);
            bank.update_envelopes(&buffer, 0, Path::Upwards, attack_ms, release_ms, 4);

            bank.compress_path(Path::Downwards, 0, &params, 1);
            bank.compress_path(Path::Upwards, 0, &params, 1);
            for bin_idx in 0..num_bins {
                bank.combined_raw_gain_difference_db[bin_idx] =
                    bank.downwards.raw_gain_difference_db[bin_idx]
                        + bank.upwards.raw_gain_difference_db[bin_idx];
            }
            let smoothing_amount = params.thresholds.downwards.threshold.gain_smoothing.value();
            bank.smooth_path_gain_differences(Path::Downwards, smoothing_amount, num_bins);
            bank.smooth_path_gain_differences(Path::Upwards, smoothing_amount, num_bins);
            bank.combine_and_clamp_gain_differences(1, num_bins);

            // The legacy formula: one shared envelope, both curves applied to it, summed. The
            // per-path envelopes are equal so they double as the legacy envelope
            let knee_width_db = params.compressors.downwards.knee_width_db.value();
            let reference: Vec<f32> = (0..num_bins)
                .map(|bin_idx| {
                    let envelope_db =
                        util::gain_to_db_fast_epsilon(bank.downwards.envelopes[0][bin_idx]);
                    let downwards_compressed = compress_downwards(
                        envelope_db,
                        bank.downwards.thresholds_db[bin_idx],
                        bank.downwards.ratios[bin_idx],
                        knee_width_db,
                        bank.downwards.knee_parabola_scale[bin_idx],
                        bank.downwards.knee_parabola_intercept[bin_idx],
                    );
                    let upwards_compressed = if bin_idx >= 1
                        && bank.upwards.ratios[bin_idx] != 1.0
                        && envelope_db > util::MINUS_INFINITY_DB
                    {
                        compress_upwards(
                            envelope_db,
                            bank.upwards.thresholds_db[bin_idx],
                            bank.upwards.ratios[bin_idx],
                            knee_width_db,
                            bank.upwards.knee_parabola_scale[bin_idx],
                            bank.upwards.knee_parabola_intercept[bin_idx],
                        )
                    } else {
                        envelope_db
                    };

                    downwards_compressed + upwards_compressed - (envelope_db * 2.0)
                })
                .collect();

            assert_slice_near(
                &bank.combined_smoothed_gain_difference_db[..num_bins],
                &reference,
            );
        });
    }

    #[test]
    fn smoothing_split_equals_legacy_when_paths_match_legacy() {
        run_with_large_stack(|| {
            let window_size = 64;
            let num_bins = window_size / 2 + 1;
            let first_non_dc_bin = 1;
            let smoothing_amount = 0.8;
            let (mut bank, _params, _ir) = make_bank_and_params(1, window_size);

            // Arbitrary per-path raw curves: with both paths mirrored at the legacy
            // configuration the split pipeline must equal the legacy single smoothing pass on
            // the summed raw curve (smoothing is linear)
            let down_raw: Vec<f32> = (0..num_bins)
                .map(|bin_idx| ((bin_idx % 7) as f32 - 3.0) * 1.1)
                .collect();
            let up_raw: Vec<f32> = (0..num_bins)
                .map(|bin_idx| ((bin_idx % 5) as f32 - 2.5) * 0.9 - 1.0)
                .collect();
            bank.downwards.raw_gain_difference_db[..num_bins].copy_from_slice(&down_raw);
            bank.upwards.raw_gain_difference_db[..num_bins].copy_from_slice(&up_raw);

            bank.smooth_path_gain_differences(Path::Downwards, smoothing_amount, num_bins);
            bank.smooth_path_gain_differences(Path::Upwards, smoothing_amount, num_bins);
            bank.combine_and_clamp_gain_differences(first_non_dc_bin, num_bins);

            // Legacy reference: the windowed mean blend over the summed raw curve, plus the
            // DC-bins clamp
            let combined_raw: Vec<f32> = down_raw.iter().zip(&up_raw).map(|(d, u)| d + u).collect();
            let radius_ln = smoothing_amount * GAIN_SMOOTHING_MAX_RADIUS_LN;
            let mut prefix_db = vec![0.0; num_bins + 1];
            for bin_idx in 0..num_bins {
                prefix_db[bin_idx + 1] = prefix_db[bin_idx] + combined_raw[bin_idx];
            }
            let mut reference = vec![0.0; num_bins];
            reference[0] = combined_raw[0];
            for bin_idx in 1..num_bins {
                let center_ln = bank.ln_freqs[bin_idx];
                let mut left_idx = 1;
                while left_idx < bin_idx && center_ln - bank.ln_freqs[left_idx] > radius_ln {
                    left_idx += 1;
                }
                let mut right_idx = bin_idx;
                while right_idx + 1 < num_bins
                    && bank.ln_freqs[right_idx + 1] - center_ln <= radius_ln
                {
                    right_idx += 1;
                }
                let sum = prefix_db[right_idx + 1] - prefix_db[left_idx];
                let count = (right_idx - left_idx + 1) as f32;
                let averaged = sum / count;
                let raw = combined_raw[bin_idx];
                let mut smoothed = raw + ((averaged - raw) * smoothing_amount);
                if bin_idx < first_non_dc_bin && smoothed > 0.0 {
                    smoothed = 0.0;
                }
                reference[bin_idx] = smoothed;
            }

            assert_slice_near(
                &bank.combined_smoothed_gain_difference_db[..num_bins],
                &reference,
            );
        });
    }

    #[test]
    fn dc_clamp_applies_to_combined_curve() {
        let window_size = 16;
        let num_bins = window_size / 2 + 1;
        let (mut bank, _params, _ir) = make_bank_and_params(1, window_size);
        bank.downwards.raw_gain_difference_db[..num_bins].fill(0.0);
        bank.upwards.raw_gain_difference_db[..num_bins].fill(0.0);
        // A smoothing amount of 0.0 copies the raws as-is, so the raw values directly control
        // the per-path smoothed values
        bank.downwards.raw_gain_difference_db[0] = 2.0;
        bank.upwards.raw_gain_difference_db[0] = -5.0;
        bank.downwards.raw_gain_difference_db[2] = 5.0;
        bank.upwards.raw_gain_difference_db[2] = -3.0;
        bank.downwards.raw_gain_difference_db[3] = 2.0;

        bank.smooth_path_gain_differences(Path::Downwards, 0.0, num_bins);
        bank.smooth_path_gain_differences(Path::Upwards, 0.0, num_bins);
        bank.combine_and_clamp_gain_differences(3, num_bins);

        // Combined values inside the DC range are clamped when they are positive: bin 2
        // combines to +2.0 and is clamped to 0.0, while the net-reducing bin 0 (-3.0) and the
        // net-neutral bin 1 (0.0) stay as they are. Bin 3, past the DC range, keeps its +2.0.
        assert_eq!(bank.combined_smoothed_gain_difference_db[0], -3.0);
        assert_eq!(bank.combined_smoothed_gain_difference_db[1], 0.0);
        assert_eq!(bank.combined_smoothed_gain_difference_db[2], 0.0);
        assert_eq!(bank.combined_smoothed_gain_difference_db[3], 2.0);
    }

    #[test]
    fn update_if_needed_touches_only_flagged_path() {
        run_with_large_stack(|| {
            let (mut bank, params, _ir) = make_bank_and_params(1, 64);
            bank.upwards.thresholds_db.fill(1234.0);

            // Clear all six dirty flags
            bank.should_update_downwards_thresholds
                .store(false, Ordering::Relaxed);
            bank.should_update_downwards_knee_parabolas
                .store(false, Ordering::Relaxed);
            bank.should_update_downwards_ratios
                .store(false, Ordering::Relaxed);
            bank.should_update_upwards_thresholds
                .store(false, Ordering::Relaxed);
            bank.should_update_upwards_knee_parabolas
                .store(false, Ordering::Relaxed);
            bank.should_update_upwards_ratios
                .store(false, Ordering::Relaxed);

            // Drive the downwards path's threshold params (this sets the downwards thresholds
            // and knee parabola flags via the params' callbacks). The slope is set so it decodes
            // to a flat curve in Internal mode (stored value 3 decodes to 0 dB/oct).
            params
                .thresholds
                .downwards
                .threshold
                .threshold_db
                .set_plain_value(-24.0);
            params
                .thresholds
                .downwards
                .threshold
                .curve_slope
                .set_plain_value(3.0);
            params
                .compressors
                .downwards
                .threshold_offset_db
                .set_plain_value(0.0);
            // Isolate the threshold update
            bank.should_update_downwards_knee_parabolas
                .store(false, Ordering::Relaxed);

            bank.update_if_needed(&params);

            assert!(bank.downwards.thresholds_db.iter().all(|t| *t == -24.0));
            assert!(bank.upwards.thresholds_db.iter().all(|t| *t == 1234.0));
        });
    }

    #[test]
    fn threshold_param_sets_only_its_own_flags() {
        run_with_large_stack(|| {
            let (bank, params, _ir) = make_bank_and_params(1, 64);

            // Clear all six dirty flags
            bank.should_update_downwards_thresholds
                .store(false, Ordering::Relaxed);
            bank.should_update_downwards_knee_parabolas
                .store(false, Ordering::Relaxed);
            bank.should_update_downwards_ratios
                .store(false, Ordering::Relaxed);
            bank.should_update_upwards_thresholds
                .store(false, Ordering::Relaxed);
            bank.should_update_upwards_knee_parabolas
                .store(false, Ordering::Relaxed);
            bank.should_update_upwards_ratios
                .store(false, Ordering::Relaxed);

            // Drive the downwards path's params through the param tree
            params
                .thresholds
                .downwards
                .threshold
                .threshold_db
                .set_plain_value(-13.0);
            params
                .compressors
                .downwards
                .threshold_offset_db
                .set_plain_value(1.0);

            assert!(bank
                .should_update_downwards_thresholds
                .load(Ordering::Relaxed));
            assert!(bank
                .should_update_downwards_knee_parabolas
                .load(Ordering::Relaxed));
            // Neither the threshold nor threshold-offset params touch ratios
            assert!(!bank.should_update_downwards_ratios.load(Ordering::Relaxed));
            assert!(!bank
                .should_update_upwards_thresholds
                .load(Ordering::Relaxed));
            assert!(!bank
                .should_update_upwards_knee_parabolas
                .load(Ordering::Relaxed));
            assert!(!bank.should_update_upwards_ratios.load(Ordering::Relaxed));
        });
    }

    #[test]
    fn threshold_slope_default_displays_as_pink_noise_tilt() {
        assert_eq!(threshold_slope_to_display_value(0.0), 3.0);
        assert_eq!(threshold_slope_from_display_value(3.0), 0.0);
        assert_eq!(internal_threshold_slope(0.0), -3.0);
    }

    #[test]
    fn threshold_slope_zero_display_maps_to_white_noise_tilt() {
        let stored_slope = threshold_slope_from_display_value(0.0);

        assert_eq!(stored_slope, 3.0);
        assert_eq!(internal_threshold_slope(stored_slope), 0.0);
    }

    #[test]
    fn gain_smoothing_zero_percent_is_unchanged() {
        let window_size = 32;
        let num_bins = window_size / 2 + 1;
        let (mut compressor_bank, _params, _frozen_ir_output_data) =
            make_bank_and_params(1, window_size);
        let raw: Vec<f32> = (0..num_bins)
            .map(|bin_idx| (bin_idx as f32 % 5.0) - 8.0)
            .collect();
        compressor_bank.downwards.raw_gain_difference_db[..num_bins].copy_from_slice(&raw);

        compressor_bank.smooth_path_gain_differences(Path::Downwards, 0.0, num_bins);

        assert_slice_near(
            &compressor_bank.downwards.smoothed_gain_difference_db[..num_bins],
            &raw,
        );
    }

    #[test]
    fn gain_smoothing_reduces_isolated_bin_spikes() {
        let window_size = 64;
        let num_bins = window_size / 2 + 1;
        let (mut compressor_bank, _params, _frozen_ir_output_data) =
            make_bank_and_params(1, window_size);
        compressor_bank.downwards.raw_gain_difference_db[..num_bins].fill(0.0);
        compressor_bank.downwards.raw_gain_difference_db[16] = -24.0;

        compressor_bank.smooth_path_gain_differences(Path::Downwards, 1.0, num_bins);

        assert!(compressor_bank.downwards.smoothed_gain_difference_db[16] > -24.0);
        assert!(compressor_bank.downwards.smoothed_gain_difference_db[16] < 0.0);
        assert!(compressor_bank.downwards.smoothed_gain_difference_db[15] < 0.0);
        assert!(compressor_bank.downwards.smoothed_gain_difference_db[17] < 0.0);
    }

    #[test]
    fn gain_smoothing_preserves_constant_curves() {
        let window_size = 64;
        let num_bins = window_size / 2 + 1;
        let (mut compressor_bank, _params, _frozen_ir_output_data) =
            make_bank_and_params(1, window_size);
        compressor_bank.downwards.raw_gain_difference_db[..num_bins].fill(-6.0);

        compressor_bank.smooth_path_gain_differences(Path::Downwards, 1.0, num_bins);

        for gain_db in &compressor_bank.downwards.smoothed_gain_difference_db[..num_bins] {
            assert!((*gain_db + 6.0).abs() < 1.0e-5);
        }
    }

    /// A completely silent block must bypass compression entirely: without the
    /// `SILENCE_THRESHOLD_GAIN` bypass the upwards compressor keeps boosting its releasing
    /// envelope in silence (capped by 'Max Boost'), so the gain keeps rising during the pause
    /// and the first incoming sound after a long attack is very loud.
    #[test]
    fn silence_bypasses_upwards_gain() {
        let num_bins = 65;
        let (mut compressor_bank, params, _frozen_ir_output_data) = make_bank_and_params(1, 128);
        // Strong upwards compression: threshold -24 dBFS, 4:1 ratio, 6 dB knee
        let (knee_scale, knee_intercept) = upwards_soft_knee_coefficients(-24.0, 6.0, 4.0);
        compressor_bank.upwards.thresholds_db.fill(-24.0);
        compressor_bank.upwards.ratios.fill(4.0);
        compressor_bank.upwards.knee_parabola_scale.fill(knee_scale);
        compressor_bank
            .upwards
            .knee_parabola_intercept
            .fill(knee_intercept);
        configure_test_downwards_compression(&mut compressor_bank, 0.0, 1.0, 0.0);

        // A silent spectrum: all bins at -100 dBFS
        let mut buffer = make_complex_buffer(1.0e-5, num_bins);
        legacy_update_envelopes(&mut compressor_bank, &buffer, 0, &params, 4);
        legacy_compress_all(
            &mut compressor_bank,
            &mut buffer,
            0,
            &params,
            1,
            false,
            false,
        );

        assert!(compressor_bank.combined_raw_gain_difference_db[..num_bins]
            .iter()
            .all(|raw| *raw == 0.0));
        assert!(
            compressor_bank.combined_smoothed_gain_difference_db[..num_bins]
                .iter()
                .all(|gain| gain.abs() < 1.0e-5)
        );
    }

    /// A silent main input must not bypass the sidechain-compression path: that path follows the
    /// sidechain signal, so its envelope gate has to use the path's own envelope (which tracks
    /// the sidechain), not the main input's magnitudes.
    #[test]
    fn silent_main_input_does_not_bypass_sidechain_compress_path() {
        run_with_large_stack(|| {
            let window_size = 64;
            let num_bins = window_size / 2 + 1;
            let (mut bank, params, _ir) = make_bank_and_params(1, window_size);
            // Strong upwards compression: threshold +6 dBFS (flat curve), 4:1 ratio, 6 dB knee.
            // The sidechain magnitudes are all below this threshold, so every bin gets boosted.
            // Real `process()` recomputes the bank from params on the first call (resize marks
            // all dirty flags), so the test configures the compressor through the params.
            params
                .thresholds
                .upwards
                .threshold
                .threshold_db
                .set_plain_value(6.0);
            params
                .thresholds
                .upwards
                .threshold
                .curve_slope
                .set_plain_value(0.0);
            params.compressors.upwards.ratio.set_plain_value(4.0);
            params
                .compressors
                .upwards
                .high_freq_ratio_rolloff
                .set_plain_value(0.0);
            // Each path decodes its own mode: the downwards path (Internal) follows the silent
            // main input, the upwards path (SidechainCompress) follows the sidechain signal
            params
                .thresholds
                .downwards
                .threshold
                .mode
                .set_plain_value(ThresholdMode::Internal);
            params
                .thresholds
                .upwards
                .threshold
                .mode
                .set_plain_value(ThresholdMode::SidechainCompress);

            // The main input is silent while the sidechain is quiet-but-alive. This feeds both
            // buffers through the full `process()` dispatch the STFT driver would use; each pass
            // gets a fresh copy since `process()` applies its gains to the buffer.
            let main = make_complex_buffer(1.0e-5, num_bins);
            let sidechain = make_complex_buffer(0.05, num_bins);
            for _ in 0..200 {
                bank.process_sidechain(&sidechain, 0);
                bank.process(&mut main.clone(), 0, &params, 4, 1);
            }

            // The downwards path is fully gated by the silent main input, but the upwards path
            // must not be: it follows the sidechain, so every non-DC bin gets compressed. The
            // combined curve sums both paths, so it carries the upwards path's contribution.
            assert!(bank
                .downwards
                .raw_gain_difference_db
                .iter()
                .all(|raw| *raw == 0.0));
            for bin_idx in 1..num_bins {
                assert!(
                    bank.upwards.raw_gain_difference_db[bin_idx] != 0.0,
                    "upwards gain difference is zero at bin {bin_idx}"
                );
                assert!(
                    bank.combined_raw_gain_difference_db[bin_idx] != 0.0,
                    "combined gain difference is zero at bin {bin_idx}"
                );
            }
        });
    }

    #[test]
    fn mixed_mode_thresholds_scale_only_matching_path() {
        run_with_large_stack(|| {
            let window_size = 64;
            let num_bins = window_size / 2 + 1;
            let (mut bank, params, _ir) = make_bank_and_params(1, window_size);
            let knee_width_db = params.compressors.downwards.knee_width_db.value();

            // The downwards path compresses at -24 dB / 4:1 with a 6 dB soft knee and scales its
            // thresholds by the sidechain signal; the upwards path boosts below +6 dB and does
            // not scale anything (`process()` recomputes the bank from params on the first call,
            // so the test configures everything through the params)
            params
                .thresholds
                .downwards
                .threshold
                .threshold_db
                .set_plain_value(-24.0);
            params
                .thresholds
                .downwards
                .threshold
                .curve_slope
                .set_plain_value(0.0);
            params.compressors.downwards.ratio.set_plain_value(4.0);
            params
                .compressors
                .downwards
                .high_freq_ratio_rolloff
                .set_plain_value(0.0);
            params
                .thresholds
                .upwards
                .threshold
                .threshold_db
                .set_plain_value(6.0);
            // Stored slope 3 decodes to 0 dB/oct in Internal mode, so the curve is flat
            params
                .thresholds
                .upwards
                .threshold
                .curve_slope
                .set_plain_value(3.0);
            params.compressors.upwards.ratio.set_plain_value(4.0);
            params
                .compressors
                .upwards
                .high_freq_ratio_rolloff
                .set_plain_value(0.0);

            params
                .thresholds
                .downwards
                .threshold
                .mode
                .set_plain_value(ThresholdMode::SidechainMatch);
            params
                .thresholds
                .upwards
                .threshold
                .mode
                .set_plain_value(ThresholdMode::Internal);

            // The main input sits at 0 dBFS per bin; the sidechain sits at -2 dBFS per bin, so a
            // correctly matching downwards threshold becomes -26 dB
            let main: Vec<Complex32> = (0..num_bins).map(|_| Complex32::new(1.0, 0.0)).collect();
            let sidechain_db = util::gain_to_db_fast_epsilon(util::db_to_gain(-2.0));
            let sidechain: Vec<Complex32> = (0..num_bins)
                .map(|_| Complex32::new(util::db_to_gain(-2.0), 0.0))
                .collect();

            // The envelopes follow the main input for both paths (SidechainMatch uses the main
            // signal for its envelopes), so the envelopes converge to the bin magnitudes. Each
            // pass gets a fresh copy of the input since `process()` applies its gains to the
            // buffer.
            for _ in 0..200 {
                bank.process_sidechain(&sidechain, 0);
                bank.process(&mut main.clone(), 0, &params, 4, 1);
            }

            for bin_idx in 1..num_bins {
                // Reference math for the downwards path: thresholds and knee coefficients
                // scaled by the (single-channel) sidechain magnitude
                let envelope_db =
                    util::gain_to_db_fast_epsilon(bank.downwards.envelopes[0][bin_idx]);
                let threshold_db = (-24.0 + sidechain_db).max(util::MINUS_INFINITY_DB);
                let (scale, intercept) =
                    downwards_soft_knee_coefficients(threshold_db, knee_width_db, 4.0);
                let expected = compress_downwards(
                    envelope_db,
                    threshold_db,
                    4.0,
                    knee_width_db,
                    scale,
                    intercept,
                ) - envelope_db;
                assert!(
                    (bank.downwards.raw_gain_difference_db[bin_idx] - expected).abs() < 1.0e-5,
                    "sidechain-match downwards threshold not scaled at bin {bin_idx}: {} != {expected}",
                    bank.downwards.raw_gain_difference_db[bin_idx]
                );

                // Reference math for the upwards path: the stored (unscaled) values
                let envelope_db = util::gain_to_db_fast_epsilon(bank.upwards.envelopes[0][bin_idx]);
                let threshold_db = 6.0;
                let (scale, intercept) =
                    upwards_soft_knee_coefficients(threshold_db, knee_width_db, 4.0);
                let expected = (compress_upwards(
                    envelope_db,
                    threshold_db,
                    4.0,
                    knee_width_db,
                    scale,
                    intercept,
                ) - envelope_db)
                    .min(envelope_db + params.compressors.upwards.max_gain_change_db.value());
                assert!(
                    (bank.upwards.raw_gain_difference_db[bin_idx] - expected).abs() < 1.0e-5,
                    "internal upwards threshold got scaled at bin {bin_idx}: {} != {expected}",
                    bank.upwards.raw_gain_difference_db[bin_idx]
                );
            }
        });
    }

    #[test]
    fn gain_smoothing_does_not_add_low_frequency_upwards_gain() {
        let window_size = 64;
        let num_bins = window_size / 2 + 1;
        let (mut compressor_bank, _params, _frozen_ir_output_data) =
            make_bank_and_params(1, window_size);
        compressor_bank.downwards.raw_gain_difference_db[..num_bins].fill(0.0);
        compressor_bank.downwards.raw_gain_difference_db[3] = 18.0;
        compressor_bank.downwards.raw_gain_difference_db[4] = 18.0;

        // Asserted through the combined curve: the DC clamp (which used to live inside the
        // smoother) now guards the combined curve, so the positive smear below the first
        // non-DC bin must still not reach the applied gain
        compressor_bank.smooth_path_gain_differences(Path::Downwards, 1.0, num_bins);
        compressor_bank.smooth_path_gain_differences(Path::Upwards, 1.0, num_bins);
        compressor_bank.combine_and_clamp_gain_differences(4, num_bins);

        assert!(compressor_bank.combined_smoothed_gain_difference_db[1] <= 0.0);
        assert!(compressor_bank.combined_smoothed_gain_difference_db[2] <= 0.0);
        assert!(compressor_bank.combined_smoothed_gain_difference_db[3] <= 0.0);
        assert!(compressor_bank.combined_smoothed_gain_difference_db[4] > 0.0);
    }

    #[test]
    fn freeze_captures_smoothed_gain_curve() {
        let window_size = 64;
        let num_bins = window_size / 2 + 1;
        let (mut compressor_bank, _params, _frozen_ir_output_data) =
            make_bank_and_params(1, window_size);
        compressor_bank.downwards.raw_gain_difference_db[..num_bins].fill(0.0);
        compressor_bank.downwards.raw_gain_difference_db[16] = -24.0;
        compressor_bank.smooth_path_gain_differences(Path::Downwards, 1.0, num_bins);
        compressor_bank.upwards.raw_gain_difference_db.fill(0.0);
        compressor_bank.smooth_path_gain_differences(Path::Upwards, 1.0, num_bins);
        compressor_bank.combine_and_clamp_gain_differences(1, num_bins);
        let smoothed_snapshot =
            compressor_bank.combined_smoothed_gain_difference_db[..num_bins].to_vec();
        let mut buffer = make_complex_buffer(1.0, num_bins);

        compressor_bank.apply_gain_differences(&mut buffer, 0, true, true);

        assert!(compressor_bank.frozen_gain_snapshot_valid[0]);
        assert_slice_near(
            &compressor_bank.frozen_gain_difference_db[0][..num_bins],
            &smoothed_snapshot,
        );
        assert_ne!(
            compressor_bank.frozen_gain_difference_db[0][16],
            compressor_bank.combined_raw_gain_difference_db[16]
        );
    }

    #[test]
    fn freeze_captures_snapshot_on_first_enabled_frame() {
        run_with_large_stack(|| {
            let window_size = 16;
            let num_bins = window_size / 2 + 1;
            let (mut compressor_bank, params, _frozen_ir_output_data) =
                make_bank_and_params(1, window_size);

            let mut warmup = make_complex_buffer(1.0, num_bins);
            legacy_update_envelopes(&mut compressor_bank, &warmup, 0, &params, 4);
            legacy_compress_all(
                &mut compressor_bank,
                &mut warmup,
                0,
                &params,
                1,
                false,
                true,
            );
            assert!(!compressor_bank.frozen_gain_snapshot_valid[0]);

            compressor_bank.sync_freeze_state(true);
            let mut capture = make_complex_buffer(2.0, num_bins);
            legacy_update_envelopes(&mut compressor_bank, &capture, 0, &params, 4);
            legacy_compress_all(
                &mut compressor_bank,
                &mut capture,
                0,
                &params,
                1,
                true,
                true,
            );

            assert!(compressor_bank.frozen_gain_snapshot_valid[0]);
            assert!(compressor_bank.frozen_gain_difference_db[0]
                .iter()
                .skip(1)
                .any(|gain| gain.abs() > 1.0e-4));
        });
    }

    #[test]
    fn freeze_holds_snapshot_across_input_and_sidechain_changes() {
        run_with_large_stack(|| {
            let window_size = 16;
            let num_bins = window_size / 2 + 1;
            let (mut compressor_bank, params, _frozen_ir_output_data) =
                make_bank_and_params(1, window_size);

            let sidechain_a = make_complex_buffer(1.0, num_bins);
            compressor_bank.process_sidechain(&sidechain_a, 0);
            compressor_bank.sync_freeze_state(true);
            let mut capture = make_complex_buffer(3.0, num_bins);
            legacy_update_envelopes(&mut compressor_bank, &capture, 0, &params, 4);
            legacy_compress_all(
                &mut compressor_bank,
                &mut capture,
                0,
                &params,
                1,
                true,
                true,
            );
            let frozen_snapshot = compressor_bank.frozen_gain_difference_db[0].clone();

            let sidechain_b = make_complex_buffer(0.1, num_bins);
            compressor_bank.process_sidechain(&sidechain_b, 0);
            let input_before = make_complex_buffer(0.25, num_bins);
            let mut held = input_before.clone();
            legacy_update_envelopes(&mut compressor_bank, &held, 0, &params, 4);
            legacy_compress_all(&mut compressor_bank, &mut held, 0, &params, 1, true, true);

            assert_eq!(
                compressor_bank.frozen_gain_difference_db[0],
                frozen_snapshot
            );
            let expected_gain = util::db_to_gain_fast(frozen_snapshot[1]);
            let actual_gain = held[1].norm() / input_before[1].norm();
            assert!((actual_gain - expected_gain).abs() < 1.0e-4);
        });
    }

    #[test]
    fn freeze_recaptures_after_toggle_off_and_on() {
        run_with_large_stack(|| {
            let window_size = 16;
            let num_bins = window_size / 2 + 1;
            let (mut compressor_bank, params, mut frozen_ir_output_data) =
                make_bank_and_params(1, window_size);

            compressor_bank.sync_freeze_state(true);
            let mut first_capture = make_complex_buffer(2.0, num_bins);
            legacy_update_envelopes(&mut compressor_bank, &first_capture, 0, &params, 4);
            legacy_compress_all(
                &mut compressor_bank,
                &mut first_capture,
                0,
                &params,
                1,
                true,
                true,
            );
            let snapshot_a = compressor_bank.frozen_gain_difference_db[0].clone();
            compressor_bank.publish_frozen_ir_snapshot(4);
            assert!(frozen_ir_output_data.read().valid);

            compressor_bank.sync_freeze_state(false);
            let mut live = make_complex_buffer(0.3, num_bins);
            legacy_update_envelopes(&mut compressor_bank, &live, 0, &params, 4);
            legacy_compress_all(&mut compressor_bank, &mut live, 0, &params, 1, false, true);
            assert!(!compressor_bank.frozen_gain_snapshot_valid[0]);
            assert!(!frozen_ir_output_data.read().valid);

            compressor_bank.sync_freeze_state(true);
            let mut second_capture = make_complex_buffer(6.0, num_bins);
            legacy_update_envelopes(&mut compressor_bank, &second_capture, 0, &params, 4);
            legacy_compress_all(
                &mut compressor_bank,
                &mut second_capture,
                0,
                &params,
                1,
                true,
                true,
            );
            let snapshot_b = compressor_bank.frozen_gain_difference_db[0].clone();
            compressor_bank.publish_frozen_ir_snapshot(4);

            assert!(compressor_bank.frozen_gain_snapshot_valid[0]);
            assert_snapshot_differs(&snapshot_a, &snapshot_b);
            assert!(frozen_ir_output_data.read().valid);
        });
    }

    #[test]
    fn freeze_snapshot_is_cleared_on_reset_and_resize() {
        run_with_large_stack(|| {
            let window_size = 16;
            let num_bins = window_size / 2 + 1;
            let (mut compressor_bank, params, mut frozen_ir_output_data) =
                make_bank_and_params(1, window_size);

            compressor_bank.sync_freeze_state(true);
            let mut capture = make_complex_buffer(2.5, num_bins);
            legacy_update_envelopes(&mut compressor_bank, &capture, 0, &params, 4);
            legacy_compress_all(
                &mut compressor_bank,
                &mut capture,
                0,
                &params,
                1,
                true,
                true,
            );
            assert!(compressor_bank.frozen_gain_snapshot_valid[0]);
            compressor_bank.publish_frozen_ir_snapshot(4);
            assert!(frozen_ir_output_data.read().valid);

            compressor_bank.reset();
            assert!(!compressor_bank.frozen_gain_snapshot_valid[0]);
            assert!(compressor_bank.frozen_gain_difference_db[0]
                .iter()
                .all(|gain| *gain == 0.0));
            assert!(!frozen_ir_output_data.read().valid);

            compressor_bank.sync_freeze_state(true);
            let mut recapture = make_complex_buffer(4.0, num_bins);
            legacy_update_envelopes(&mut compressor_bank, &recapture, 0, &params, 4);
            legacy_compress_all(
                &mut compressor_bank,
                &mut recapture,
                0,
                &params,
                1,
                true,
                true,
            );
            assert!(compressor_bank.frozen_gain_snapshot_valid[0]);
            compressor_bank.publish_frozen_ir_snapshot(4);
            assert!(frozen_ir_output_data.read().valid);

            compressor_bank.resize(&test_buffer_config(32), 32);
            assert!(!compressor_bank.frozen_gain_snapshot_valid[0]);
            assert!(compressor_bank.frozen_gain_difference_db[0]
                .iter()
                .all(|gain| *gain == 0.0));
            let snapshot = frozen_ir_output_data.read().clone();
            assert!(!snapshot.valid);
            assert_eq!(snapshot.window_size, 32);
        });
    }

    #[test]
    fn freeze_publishes_export_snapshot_after_capture() {
        run_with_large_stack(|| {
            let window_size = 16;
            let num_bins = window_size / 2 + 1;
            let (mut compressor_bank, params, mut frozen_ir_output_data) =
                make_bank_and_params(1, window_size);

            compressor_bank.sync_freeze_state(true);
            let mut capture = make_complex_buffer(3.5, num_bins);
            legacy_update_envelopes(&mut compressor_bank, &capture, 0, &params, 4);
            legacy_compress_all(
                &mut compressor_bank,
                &mut capture,
                0,
                &params,
                1,
                true,
                true,
            );
            compressor_bank.publish_frozen_ir_snapshot(4);

            let snapshot = frozen_ir_output_data.read().clone();
            assert!(snapshot.valid);
            assert_eq!(snapshot.window_size, window_size);
            assert_eq!(snapshot.overlap_times, 4);
            assert_eq!(snapshot.num_channels, 1);
            assert!(snapshot.gain_difference_db[0][1].abs() > 1.0e-4);
        });
    }

    #[test]
    fn freeze_export_snapshot_clears_on_unsupported_mode() {
        run_with_large_stack(|| {
            let window_size = 16;
            let num_bins = window_size / 2 + 1;
            let (mut compressor_bank, params, mut frozen_ir_output_data) =
                make_bank_and_params(1, window_size);

            compressor_bank.sync_freeze_state(true);
            let mut capture = make_complex_buffer(3.0, num_bins);
            legacy_update_envelopes(&mut compressor_bank, &capture, 0, &params, 4);
            legacy_compress_all(
                &mut compressor_bank,
                &mut capture,
                0,
                &params,
                1,
                true,
                true,
            );
            compressor_bank.publish_frozen_ir_snapshot(4);
            assert!(frozen_ir_output_data.read().valid);

            compressor_bank.sync_freeze_state(false);

            let snapshot = frozen_ir_output_data.read().clone();
            assert!(!snapshot.valid);
        });
    }

    #[test]
    fn fixed_slope_uses_downwards_path_convention() {
        run_with_large_stack(|| {
            let window_size = 64;
            let (mut bank, params, _ir) = make_bank_and_params(1, window_size);
            let num_bins = window_size / 2 + 1;

            // Both paths are Internal with a stored slope of 0, so each decodes its slope as
            // 0 - 3 = -3 dB/oct (pink noise). The metering convention is downwards-first, so the
            // published fit must carry the downwards path's decoded slope.
            params
                .thresholds
                .downwards
                .threshold
                .curve_slope
                .set_plain_value(0.0);
            params
                .thresholds
                .upwards
                .threshold
                .curve_slope
                .set_plain_value(0.0);

            let buffer = make_complex_buffer(1.0, num_bins);
            bank.match_curve_meter.start(48_000.0);
            // The meter needs 5 seconds of hop-sized frames to finish: drive it well past that
            let passes = 60_000 + 1;
            for _ in 0..passes {
                bank.measure_match_curve(&buffer, 0, &params, 4);
            }

            let result = bank
                .match_curve_runtime
                .take_finished_result()
                .expect("match curve meter did not finish");
            match result {
                MatchCurveResult::Matched(fit) => assert_eq!(fit.slope, -3.0),
                MatchCurveResult::Failed => panic!("fitting failed"),
            }
        });
    }

    #[test]
    fn analyzer_curve_params_are_per_path() {
        run_with_large_stack(|| {
            let window_size = 64;
            let (mut bank, params, _ir, mut analyzer_output_data) =
                make_bank_and_params_with_analyzer(1, window_size);
            let num_bins = window_size / 2 + 1;

            // Two distinct threshold curves so the published data must carry both intercepts
            params
                .thresholds
                .downwards
                .threshold
                .threshold_db
                .set_plain_value(-24.0);
            params
                .thresholds
                .upwards
                .threshold
                .threshold_db
                .set_plain_value(-40.0);
            // Distinct envelope trace timings so the two traces differ: with the same magnitudes
            // in every pass the envelopes would sit exactly on them, so the upwards path gets a
            // much slower attack. The timing scale is pinned at 1.0 (its fade-in would otherwise
            // zero the attack times on the first pass).
            params
                .thresholds
                .downwards
                .compressor_attack_ms
                .set_plain_value(1.0);
            params
                .thresholds
                .upwards
                .compressor_attack_ms
                .set_plain_value(100_000.0);
            bank.envelope_followers_timing_scale = 1.0;
            params.editor_state.set_open(true);

            let mut buffer = make_complex_buffer(1.0, num_bins);
            bank.process(&mut buffer, 0, &params, 4, 1);
            bank.publish_analyzer_if_needed();

            let data = analyzer_output_data.read().clone();
            assert_eq!(data.curve_params[0].intercept, -24.0);
            assert_eq!(data.curve_params[1].intercept, -40.0);
            // The traces are per path: the upwards path's slower attack leaves its envelope
            // well below the downwards trace on this short window
            for bin_idx in 1..num_bins {
                assert!(
                    data.envelope_followers_downwards[bin_idx]
                        > data.envelope_followers_upwards[bin_idx],
                    "envelope traces do not split per path at bin {bin_idx}"
                );
            }
        });
    }
}
