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

use atomic_float::AtomicF32;
use atomic_refcell::AtomicRefCell;
use nih_plug::prelude::*;
use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::widgets::*;
use nih_plug_vizia::{assets, create_vizia_editor, ViziaState, ViziaTheming};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

#[cfg(feature = "hot-reload")]
use std::thread;
#[cfg(feature = "hot-reload")]
use std::time::{Duration, SystemTime};

use self::analyzer::Analyzer;
use self::delta_button::DeltaButton;
use self::export_ir_button::ExportIrButton;
use self::match_button::MatchButton;
use self::match_curve_button::{ClearCurveButton, MatchCurveButton};
use crate::analyzer::AnalyzerData;
// The compressor bank's `Path` enum, aliased to avoid the std `Path` import above.
use crate::compressor_bank::{Path as CompressionPath, ThresholdMode};
use crate::curve_preset::{
    delete_user_preset, load_curve_presets, save_user_preset_to_path, user_preset_dir, CurvePreset,
    CurvePresetEntry, CurvePresetSource,
};
use crate::frozen_ir::FrozenIrData;
use crate::match_curve::{MatchCurveFit, MatchCurveResult, MatchCurveRuntime};
use crate::match_level::{MatchResult, MatchRuntime};
use crate::{SpectralCompressor, SpectralCompressorParams};

mod analyzer;
mod delta_button;
mod export_ir_button;
mod match_button;
mod match_curve_button;
pub mod theme;

/// The entire GUI's width, in logical pixels.
const GUI_WIDTH: u32 = 1360;
/// The entire GUI's height, in logical pixels.
// ~690px of content: the globals bar plus the full per-path threshold and compressor columns.
// Keep this in sync with the Preferred GUI size in auv3/CMakeLists.txt.
const GUI_HEIGHT: u32 = 690;

// All GUI colors are defined in `editor::theme`.

#[derive(Clone, Lens)]
/// Editor state passed into the vizia views. This bundles the parameter tree and the
/// shared runtime handles the custom widgets need to talk to the audio thread.
pub(crate) struct Data {
    pub(crate) params: Arc<SpectralCompressorParams>,
    /// Display labels for the threshold mode pick list.
    pub(crate) mode_options: Vec<String>,

    /// When true the plugin outputs the delta signal (input minus processed).
    pub(crate) delta_active: Arc<AtomicBool>,
    /// Request/result handoff for the editor-triggered output gain match.
    pub(crate) match_runtime: Arc<MatchRuntime>,
    /// Request/result handoff for the editor-triggered threshold curve match.
    pub(crate) match_curve_runtime: Arc<MatchCurveRuntime>,

    pub(crate) analyzer_data: Arc<AtomicRefCell<triple_buffer::Output<AnalyzerData>>>,
    pub(crate) frozen_ir_data: Arc<AtomicRefCell<triple_buffer::Output<FrozenIrData>>>,
    /// Used by the analyzer to determine which FFT bins belong to which frequencies.
    pub(crate) sample_rate: Arc<AtomicF32>,
    /// Which path's threshold curve the analyzer edits and shows curve points for. 0 = downwards,
    /// 1 = upwards. GUI-local state, not persisted.
    pub(crate) selected_threshold_curve: Arc<AtomicU8>,
}

impl Model for Data {}

#[derive(Lens)]
/// Editor-local state for the status label and the export button's enabled flag.
struct ExportUiModel {
    can_export: bool,
    status_text: String,
    selected_curve_preset_index: usize,
    can_delete_curve_preset: bool,
    curve_preset_options: Vec<String>,
    curve_preset_entries: Vec<CurvePresetEntry>,
    frozen_ir_data: Arc<AtomicRefCell<triple_buffer::Output<FrozenIrData>>>,
    params: Arc<SpectralCompressorParams>,
    gui_context: Arc<dyn GuiContext>,
}

impl ExportUiModel {
    fn build(
        cx: &mut Context,
        frozen_ir_data: Arc<AtomicRefCell<triple_buffer::Output<FrozenIrData>>>,
        params: Arc<SpectralCompressorParams>,
        gui_context: Arc<dyn GuiContext>,
    ) {
        let can_export = Self::read_can_export(&frozen_ir_data);
        let loaded_presets = load_curve_presets();
        for warning in &loaded_presets.warnings {
            nih_log!("Could not load curve preset: {warning}");
        }
        let curve_preset_options = curve_preset_options(&loaded_presets.entries);

        Self {
            can_export,
            status_text: String::new(),
            selected_curve_preset_index: 0,
            can_delete_curve_preset: false,
            curve_preset_options,
            curve_preset_entries: loaded_presets.entries,
            frozen_ir_data,
            params,
            gui_context,
        }
        .build(cx);
    }

    fn read_can_export(
        frozen_ir_data: &Arc<AtomicRefCell<triple_buffer::Output<FrozenIrData>>>,
    ) -> bool {
        if let Ok(mut frozen_ir_data) = frozen_ir_data.try_borrow_mut() {
            frozen_ir_data.read().valid
        } else {
            false
        }
    }

    fn refresh_export_availability(&mut self) {
        self.can_export = Self::read_can_export(&self.frozen_ir_data);
    }
}

enum CurvePresetEvent {
    Apply(usize),
    SaveRequested,
    SaveDialogFinished(Option<PathBuf>),
    DeleteSelected,
}

#[derive(Clone, Copy)]
enum CurvePresetButtonAction {
    SaveRequested,
    DeleteSelected,
}

struct CurvePresetButton {
    action: CurvePresetButtonAction,
}

impl CurvePresetButton {
    fn new<T>(
        cx: &mut Context,
        action: CurvePresetButtonAction,
        label: impl Res<T> + Clone,
    ) -> Handle<'_, Self>
    where
        T: ToString,
    {
        Self { action }
            .build(cx, |cx| {
                Label::new(cx, label).hoverable(false);
            })
            .class("editor-mode")
    }
}

impl View for CurvePresetButton {
    fn element(&self) -> Option<&'static str> {
        Some("param-button")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, meta| match window_event {
            WindowEvent::MouseDown(MouseButton::Left)
            | WindowEvent::MouseDoubleClick(MouseButton::Left)
            | WindowEvent::MouseTripleClick(MouseButton::Left) => {
                meta.consume();
                if cx.is_disabled() {
                    return;
                }

                match self.action {
                    CurvePresetButtonAction::SaveRequested => {
                        cx.emit(CurvePresetEvent::SaveRequested)
                    }
                    CurvePresetButtonAction::DeleteSelected => {
                        cx.emit(CurvePresetEvent::DeleteSelected)
                    }
                }
            }
            _ => {}
        });
    }
}

impl Model for ExportUiModel {
    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        // Events from the match poll thread arrive here as `MatchEvent::Result`.
        event.map(
            |match_event: &match_button::MatchEvent, _meta| match match_event {
                match_button::MatchEvent::Started => {
                    self.status_text = String::from("Matching...");
                }
                match_button::MatchEvent::Result(result) => {
                    match result {
                        MatchResult::Matched(output_gain) => {
                            let output_gain = *output_gain;
                            let setter = ParamSetter::new(self.gui_context.as_ref());
                            setter.begin_set_parameter(&self.params.global.output_gain);
                            setter.set_parameter(&self.params.global.output_gain, output_gain);
                            setter.end_set_parameter(&self.params.global.output_gain);
                            cx.emit(RawParamEvent::ParametersChanged);

                            let output_gain_db = nih_plug::prelude::util::gain_to_db(output_gain);
                            self.status_text = format!("Matched {output_gain_db:+.2} dB");
                        }
                        MatchResult::Failed => {
                            self.status_text = String::from("Match failed: signal too quiet.");
                        }
                    }

                    self.refresh_export_availability();
                }
            },
        );

        event.map(
            |match_curve_event: &match_curve_button::MatchCurveEvent, _meta| match match_curve_event
            {
                match_curve_button::MatchCurveEvent::Started => {
                    self.status_text = String::from("Matching curve...");
                }
                match_curve_button::MatchCurveEvent::Result(result) => {
                    match result {
                        MatchCurveResult::Matched(fit) => {
                            self.apply_match_curve(*fit);
                            cx.emit(RawParamEvent::ParametersChanged);
                            self.selected_curve_preset_index = 0;
                            self.refresh_curve_preset_delete_enabled();
                            self.status_text = String::from("Curve matched");
                        }
                        MatchCurveResult::Failed => {
                            self.status_text =
                                String::from("Match curve failed: signal too quiet.");
                        }
                    }

                    self.refresh_export_availability();
                }
                match_curve_button::MatchCurveEvent::ClearRequested => {
                    self.clear_match_curve();
                    cx.emit(RawParamEvent::ParametersChanged);
                    self.selected_curve_preset_index = 0;
                    self.refresh_curve_preset_delete_enabled();
                    self.status_text = String::from("Curve cleared");
                    self.refresh_export_availability();
                }
            },
        );

        event.map(
            |curve_preset_event: &CurvePresetEvent, _meta| match curve_preset_event {
                CurvePresetEvent::Apply(index) => {
                    if *index == 0 {
                        return;
                    }

                    if let Some(entry) = self.curve_preset_entries.get(index - 1) {
                        let preset = entry.preset.clone();
                        let source = entry.source;
                        self.apply_curve_preset(preset.clone(), source);
                        self.selected_curve_preset_index = *index;
                        self.refresh_curve_preset_delete_enabled();
                        cx.emit(RawParamEvent::ParametersChanged);
                        self.status_text = format!("Applied: {}", preset.name);
                        self.refresh_export_availability();
                    }
                }
                CurvePresetEvent::SaveRequested => {
                    let Some(dir) = user_preset_dir() else {
                        self.status_text = String::from("Could not find preset folder");
                        return;
                    };

                    self.status_text = String::from("Choose preset name...");
                    cx.spawn(move |proxy| {
                        let path = prompt_for_curve_preset_path(&dir);
                        let _ = proxy.emit(CurvePresetEvent::SaveDialogFinished(path));
                    });
                }
                CurvePresetEvent::SaveDialogFinished(path) => {
                    let Some(path) = path else {
                        self.status_text = String::new();
                        return;
                    };

                    let preset = self.current_curve_preset();
                    match save_user_preset_to_path(path, preset) {
                        Ok(entry) => {
                            self.upsert_user_preset(entry);
                            self.curve_preset_options =
                                curve_preset_options(&self.curve_preset_entries);
                            self.refresh_curve_preset_delete_enabled();
                            let name = self
                                .selected_curve_preset_name()
                                .unwrap_or_else(|| String::from("Preset"));
                            self.status_text = format!("Saved preset: {name}");
                        }
                        Err(err) => {
                            self.selected_curve_preset_index = 0;
                            self.refresh_curve_preset_delete_enabled();
                            self.status_text = format!("Save failed: {err}");
                        }
                    }
                }
                CurvePresetEvent::DeleteSelected => {
                    let Some(entry) = self.selected_user_curve_preset_entry().cloned() else {
                        self.status_text = String::from("Select a user preset to delete");
                        return;
                    };

                    match delete_user_preset(&entry) {
                        Ok(()) => {
                            let name = entry.preset.name;
                            self.curve_preset_entries.retain(|existing| {
                                existing.source != CurvePresetSource::User
                                    || existing.preset.name != name
                            });
                            self.curve_preset_options =
                                curve_preset_options(&self.curve_preset_entries);
                            self.selected_curve_preset_index = 0;
                            self.refresh_curve_preset_delete_enabled();
                            self.status_text = format!("Deleted preset: {name}");
                        }
                        Err(err) => {
                            self.status_text = format!("Delete failed: {err}");
                        }
                    }
                }
            },
        );

        // Keep export state in sync when parameters are changed from the UI/host.
        event.map(|_param_event: &RawParamEvent, _meta| {
            self.refresh_export_availability();
        });
    }
}

#[cfg(feature = "hot-reload")]
enum HotReloadEvent {
    CssChanged,
}

#[cfg(feature = "hot-reload")]
struct HotReloadModel;

#[cfg(feature = "hot-reload")]
impl HotReloadModel {
    fn build(cx: &mut Context, css_path: PathBuf) {
        Self.build(cx);

        cx.spawn(move |cx| {
            let mut last_modified = css_modified_at(&css_path);

            loop {
                thread::sleep(Duration::from_millis(250));

                let modified = css_modified_at(&css_path);
                if modified.is_some() && modified != last_modified {
                    last_modified = modified;

                    // Give editors a moment to finish writing the file before parsing it.
                    thread::sleep(Duration::from_millis(75));

                    if cx.emit(HotReloadEvent::CssChanged).is_err() {
                        break;
                    }
                }
            }
        });
    }
}

#[cfg(feature = "hot-reload")]
impl Model for HotReloadModel {
    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(
            |hot_reload_event: &HotReloadEvent, _meta| match hot_reload_event {
                HotReloadEvent::CssChanged => {
                    if let Err(err) = cx.reload_styles() {
                        nih_error!("[hot-reload] Could not reload CSS: {err:?}");
                    } else {
                        nih_log!("[hot-reload] Reloaded CSS");
                    }
                }
            },
        );
    }
}

#[cfg(feature = "hot-reload")]
fn css_modified_at(css_path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(css_path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

impl ExportUiModel {
    fn current_curve_preset(&self) -> CurvePreset {
        let threshold = &self.params.thresholds.downwards.threshold;

        CurvePreset {
            name: String::new(),
            threshold_db: threshold.threshold_db.value(),
            center_frequency: threshold.center_frequency.value(),
            curve_slope: threshold.curve_slope.value(),
            curve_curve: threshold.curve_curve.value(),
            points: std::array::from_fn(|index| threshold.curve_points[index].curve_point()),
        }
    }

    fn selected_curve_preset_entry(&self) -> Option<&CurvePresetEntry> {
        self.selected_curve_preset_index
            .checked_sub(1)
            .and_then(|index| self.curve_preset_entries.get(index))
    }

    fn selected_user_curve_preset_entry(&self) -> Option<&CurvePresetEntry> {
        self.selected_curve_preset_entry()
            .filter(|entry| entry.source == CurvePresetSource::User)
    }

    fn selected_curve_preset_name(&self) -> Option<String> {
        self.selected_curve_preset_entry()
            .map(|entry| entry.preset.name.clone())
    }

    fn refresh_curve_preset_delete_enabled(&mut self) {
        self.can_delete_curve_preset = self
            .selected_curve_preset_entry()
            .is_some_and(|entry| entry.source == CurvePresetSource::User);
    }

    fn upsert_user_preset(&mut self, entry: CurvePresetEntry) {
        let name = entry.preset.name.clone();
        self.curve_preset_entries.retain(|existing| {
            existing.source != CurvePresetSource::User || existing.preset.name != name
        });
        self.curve_preset_entries.push(entry);

        let built_in_count = self
            .curve_preset_entries
            .iter()
            .filter(|entry| entry.source == CurvePresetSource::BuiltIn)
            .count();
        self.curve_preset_entries[built_in_count..]
            .sort_by(|left, right| left.preset.name.cmp(&right.preset.name));

        self.selected_curve_preset_index = self
            .curve_preset_entries
            .iter()
            .position(|entry| entry.source == CurvePresetSource::User && entry.preset.name == name)
            .map(|index| index + 1)
            .unwrap_or(0);
    }

    fn apply_curve_preset(&self, preset: CurvePreset, source: CurvePresetSource) {
        let setter = ParamSetter::new(self.gui_context.as_ref());

        // The curve is shared between both paths, so a preset writes both curves identically
        for threshold in [
            &self.params.thresholds.downwards.threshold,
            &self.params.thresholds.upwards.threshold,
        ] {
            setter.begin_set_parameter(&threshold.threshold_db);
            setter.set_parameter(&threshold.threshold_db, preset.threshold_db);
            setter.end_set_parameter(&threshold.threshold_db);

            if source.applies_anchor_parameters() {
                setter.begin_set_parameter(&threshold.center_frequency);
                setter.set_parameter(&threshold.center_frequency, preset.center_frequency);
                setter.end_set_parameter(&threshold.center_frequency);

                setter.begin_set_parameter(&threshold.curve_slope);
                setter.set_parameter(&threshold.curve_slope, preset.curve_slope);
                setter.end_set_parameter(&threshold.curve_slope);
            }

            setter.begin_set_parameter(&threshold.curve_curve);
            setter.set_parameter(&threshold.curve_curve, preset.curve_curve);
            setter.end_set_parameter(&threshold.curve_curve);

            for (point_params, point) in threshold.curve_points.iter().zip(preset.points) {
                setter.begin_set_parameter(&point_params.enabled);
                setter.set_parameter(&point_params.enabled, point.enabled);
                setter.end_set_parameter(&point_params.enabled);

                setter.begin_set_parameter(&point_params.frequency);
                setter.set_parameter(&point_params.frequency, point.frequency);
                setter.end_set_parameter(&point_params.frequency);

                setter.begin_set_parameter(&point_params.offset_db);
                setter.set_parameter(&point_params.offset_db, point.offset_db);
                setter.end_set_parameter(&point_params.offset_db);
            }
        }
    }

    fn apply_match_curve(&self, fit: MatchCurveFit) {
        let setter = ParamSetter::new(self.gui_context.as_ref());

        // The matching fit is a single spectrum, so it is written to both paths verbatim
        for threshold in [
            &self.params.thresholds.downwards.threshold,
            &self.params.thresholds.upwards.threshold,
        ] {
            setter.begin_set_parameter(&threshold.threshold_db);
            setter.set_parameter(&threshold.threshold_db, fit.intercept);
            setter.end_set_parameter(&threshold.threshold_db);

            setter.begin_set_parameter(&threshold.curve_curve);
            setter.set_parameter(&threshold.curve_curve, fit.curve);
            setter.end_set_parameter(&threshold.curve_curve);

            for (point_params, point) in threshold.curve_points.iter().zip(fit.points) {
                setter.begin_set_parameter(&point_params.enabled);
                setter.set_parameter(&point_params.enabled, point.enabled);
                setter.end_set_parameter(&point_params.enabled);

                setter.begin_set_parameter(&point_params.frequency);
                setter.set_parameter(&point_params.frequency, point.frequency);
                setter.end_set_parameter(&point_params.frequency);

                setter.begin_set_parameter(&point_params.offset_db);
                setter.set_parameter(&point_params.offset_db, point.offset_db);
                setter.end_set_parameter(&point_params.offset_db);
            }
        }
    }

    fn clear_match_curve(&self) {
        let setter = ParamSetter::new(self.gui_context.as_ref());

        // Clearing resets both paths' curves
        for threshold in [
            &self.params.thresholds.downwards.threshold,
            &self.params.thresholds.upwards.threshold,
        ] {
            setter.begin_set_parameter(&threshold.threshold_db);
            setter.set_parameter(&threshold.threshold_db, -12.0);
            setter.end_set_parameter(&threshold.threshold_db);

            setter.begin_set_parameter(&threshold.center_frequency);
            setter.set_parameter(&threshold.center_frequency, 1_000.0);
            setter.end_set_parameter(&threshold.center_frequency);

            setter.begin_set_parameter(&threshold.curve_slope);
            setter.set_parameter(&threshold.curve_slope, 0.0);
            setter.end_set_parameter(&threshold.curve_slope);

            setter.begin_set_parameter(&threshold.curve_curve);
            setter.set_parameter(&threshold.curve_curve, 0.0);
            setter.end_set_parameter(&threshold.curve_curve);

            for point_params in &threshold.curve_points {
                setter.begin_set_parameter(&point_params.enabled);
                setter.set_parameter(&point_params.enabled, false);
                setter.end_set_parameter(&point_params.enabled);

                setter.begin_set_parameter(&point_params.frequency);
                setter.set_parameter(&point_params.frequency, 1_000.0);
                setter.end_set_parameter(&point_params.frequency);

                setter.begin_set_parameter(&point_params.offset_db);
                setter.set_parameter(&point_params.offset_db, 0.0);
                setter.end_set_parameter(&point_params.offset_db);
            }
        }
    }
}

/// Creates the default persisted editor window state.
pub(crate) fn default_state() -> Arc<ViziaState> {
    ViziaState::new(move || (GUI_WIDTH, GUI_HEIGHT))
}

pub(crate) fn create(editor_state: Arc<ViziaState>, editor_data: Data) -> Option<Box<dyn Editor>> {
    create_vizia_editor(
        editor_state,
        ViziaTheming::Custom,
        move |cx, gui_context| {
            assets::register_noto_sans_light(cx);
            assets::register_noto_sans_thin(cx);

            // In hot-reload mode the CSS is read from disk so you can tweak it without
            // recompiling.
            #[cfg(feature = "hot-reload")]
            {
                let css_path: PathBuf = [env!("CARGO_MANIFEST_DIR"), "src", "editor", "theme.css"]
                    .iter()
                    .collect();
                if let Err(err) = cx.add_stylesheet(css_path.clone()) {
                    nih_error!(
                        "[hot-reload] Could not load {}: {err:?}",
                        css_path.display()
                    );
                } else {
                    nih_log!("[hot-reload] Loaded CSS from {}", css_path.display());
                }

                HotReloadModel::build(cx, css_path);
            }
            #[cfg(not(feature = "hot-reload"))]
            if let Err(err) = cx.add_stylesheet(include_style!("src/editor/theme.css")) {
                nih_error!("Failed to load stylesheet: {err:?}")
            }

            editor_data.clone().build(cx);
            ExportUiModel::build(
                cx,
                editor_data.frozen_ir_data.clone(),
                editor_data.params.clone(),
                gui_context,
            );

            HStack::new(cx, |cx| {
                main_column(cx);
                analyzer_column(cx);
            });
        },
    )
}

/// Builds the left/main side of the editor.
fn main_column(cx: &mut Context) {
    VStack::new(cx, |cx| {
        header_bar(cx);
        globals_bar(cx);
        threshold_row(cx);
    })
    .class("main-column");
}

/// The bar on top of the threshold columns holding the controls shared between both compression
/// paths: output gain, mix, the FFT window, and the freeze/export controls.
fn globals_bar(cx: &mut Context) {
    let global_params = Data::params.map(|p| p.global.clone());

    HStack::new(cx, |cx| {
        HStack::new(cx, |cx| {
            Label::new(cx, "Output Gain").class("label");
            ParamSlider::new(cx, global_params, |params| &params.output_gain).class("bar-slider");
        })
        .class("globals-bar-group");

        HStack::new(cx, |cx| {
            Label::new(cx, "Mix").class("label");
            ParamSlider::new(cx, global_params, |params| &params.dry_wet_ratio)
                .class("bar-slider");
        })
        .class("globals-bar-group");

        HStack::new(cx, |cx| {
            Label::new(cx, "FFT Window").class("label");
            HStack::new(cx, |cx| {
                ParamSlider::new(cx, global_params, |params| &params.window_size_order)
                    .set_style(ParamSliderStyle::FromLeft)
                    .class("bar-slider")
                    .tooltip(|cx| {
                        Label::new(
                            cx,
                            "Window size controls the FFT analysis length. \nLarger windows improve frequency resolution but respond more slowly.",
                        );
                    });
                ParamSlider::new(cx, global_params, |params| &params.overlap_times_order)
                    .set_style(ParamSliderStyle::FromLeft)
                    .class("bar-slider")
                    .tooltip(|cx| {
                        Label::new(
                            cx,
                            "Window overlap controls how often overlapping analysis windows are processed. \nMore overlap is smoother but uses more CPU.",
                        );
                    });
            })
            .class("globals-bar-group");
        })
        .class("globals-bar-group");

        HStack::new(cx, |cx| {
            ParamButton::new(cx, global_params, |params| &params.compressor_freeze)
                .with_label("Freeze")
                .class("freeze-button");
            ExportIrButton::new(cx, Data::frozen_ir_data, "Export IR...")
                .disabled(ExportUiModel::can_export.map(|can_export| !*can_export))
                .class("freeze-button");
        })
        .class("globals-bar-group");
    })
    .class("globals-bar");
}

/// The row with both compression paths' full threshold and compressor columns.
fn threshold_row(cx: &mut Context) {
    HStack::new(cx, |cx| {
        path_column(cx, CompressionPath::Downwards);
        path_column(cx, CompressionPath::Upwards);
    })
    .class("page-stack");
}

/// Header row containing Delta/Match/Bypass controls, status, and title/version.
fn header_bar(cx: &mut Context) {
    let global_params = Data::params.map(|p| p.global.clone());

    HStack::new(cx, |cx| {
        HStack::new(cx, |cx| {
            DeltaButton::new(cx, Data::delta_active, "Delta").class("header-btn");
            MatchButton::new(
                cx,
                Data::match_runtime,
                Data::delta_active,
                global_params,
                "Match",
            )
            .class("header-btn");
            ParamButton::new(cx, global_params, |params| &params.bypass)
                .with_label("Bypass")
                .class("header-btn");
        })
        .class("header-button-group");

        Label::new(cx, ExportUiModel::status_text).class("match-status");

        HStack::new(cx, |cx| {
            Label::new(cx, "Resname Spectral Compressor")
                .class("plugin-title")
                .on_mouse_down(|_, _| {
                    // `CARGO_PKG_HOMEPAGE` is `""` when no `homepage` is set in
                    // Cargo.toml, and `open::that("")` opens Explorer in the
                    // current working directory on Windows.
                    let url = SpectralCompressor::URL;
                    if url.is_empty() {
                        return;
                    }
                    // Spawn on a background thread: on Windows, `open::that`
                    // ultimately calls `ShellExecuteW`, which can hang or crash
                    // when invoked from a plugin host's UI thread (COM apartment
                    // mismatches, reentrant message pumping, etc.).
                    std::thread::spawn(move || {
                        let result = open::that(url);
                        if cfg!(debug_assertions) && result.is_err() {
                            nih_debug_assert_failure!("Failed to open web browser: {:?}", result);
                        }
                    });
                });
            Label::new(cx, SpectralCompressor::VERSION).class("version-label");
        })
        .class("title-group");
    })
    .class("header-bar");
}

/// Builds one full column for a single compression path: its envelope timings, its own threshold
/// parameters (including the curve matching controls), and its compressor parameters.
fn path_column(cx: &mut Context, path: CompressionPath) {
    let (name_prefix, css_class) = match path {
        CompressionPath::Downwards => ("Downwards", "downwards-col"),
        CompressionPath::Upwards => ("Upwards", "upwards-col"),
    };
    let path_params = match path {
        CompressionPath::Downwards => Data::params.map(|p| p.thresholds.downwards.clone()),
        CompressionPath::Upwards => Data::params.map(|p| p.thresholds.upwards.clone()),
    };
    let compressor_params = match path {
        CompressionPath::Downwards => Data::params.map(|p| p.compressors.downwards.clone()),
        CompressionPath::Upwards => Data::params.map(|p| p.compressors.upwards.clone()),
    };
    let global_params = Data::params.map(|p| p.global.clone());
    let mut curve_controls_drawn = false;

    make_column(cx, name_prefix, Some(css_class), |cx| {
        GenericUi::new_custom(cx, path_params, move |cx, param_ptr| {
            let name = unsafe { param_ptr.name() };

            if name == format!("{name_prefix} Threshold Center") {
                HStack::new(cx, |cx| {
                    Label::new(cx, "Threshold").class("label");
                    HStack::new(cx, |cx| {
                        ParamSlider::new(cx, path_params, |params| {
                            &params.threshold.center_frequency
                        })
                        .class("threshold-slider")
                        .tooltip(|cx| {
                            Label::new(
                                cx,
                                "Threshold center is the frequency where the global threshold value is anchored.",
                            );
                        });
                        ParamSlider::new(cx, path_params, |params| {
                            &params.threshold.curve_slope
                        })
                        .class("threshold-slider")
                        .tooltip(|cx| {
                            Label::new(
                                cx,
                                "Threshold slope tilts the threshold curve across octaves. In Pink Noise mode, 3 dB/oct is the neutral default.",
                            );
                        });
                    })
                    .class("threshold-slider-group");
                })
                .class("row");
            } else if name == format!("{name_prefix} Threshold Slope") {
                // Rendered together with Threshold Center above.
            } else if name == format!("{name_prefix} Mode") {
                HStack::new(cx, |cx| {
                    Label::new(cx, "Mode").class("label");
                    mode_picklist(cx, path);
                })
                .class("row");
            } else if name == format!("{name_prefix} Global Threshold") {
                // The threshold section heading is drawn before its first row
                Label::new(cx, "THRESHOLD").class("section-label");

                HStack::new(cx, |cx| {
                    Label::new(cx, "Global Threshold").class("label");
                    GenericUi::draw_widget(cx, path_params, param_ptr);
                })
                .class("row");
            } else {
                HStack::new(cx, |cx| {
                    Label::new(
                        cx,
                        name.strip_prefix(&format!("{name_prefix} "))
                            .expect("missing prefix"),
                    )
                    .class("label");

                    GenericUi::draw_widget(cx, path_params, param_ptr);
                })
                .class("row");
            }

            if !curve_controls_drawn && name == format!("{name_prefix} Threshold Slope") {
                curve_controls_drawn = true;
                HStack::new(cx, |cx| {
                    Label::new(cx, "Curve Matching").class("label");
                    HStack::new(cx, |cx| {
                        MatchCurveButton::new(
                            cx,
                            Data::match_curve_runtime,
                            Data::delta_active,
                            global_params,
                            "Match",
                        )
                        .class("threshold-curve-button");
                        ClearCurveButton::new(cx, "Clear").class("threshold-curve-button");
                    })
                    .class("curve-matching-button-group");
                })
                .class("row");

                HStack::new(cx, |cx| {
                    Label::new(cx, "Curve Presets").class("label");
                    HStack::new(cx, |cx| {
                        curve_preset_picklist(cx);
                        CurvePresetButton::new(cx, CurvePresetButtonAction::SaveRequested, "Save")
                            .class("curve-preset-button");
                        CurvePresetButton::new(cx, CurvePresetButtonAction::DeleteSelected, "Del")
                            .disabled(
                                ExportUiModel::can_delete_curve_preset
                                    .map(|can_delete| !*can_delete),
                            )
                            .class("curve-preset-button");
                    })
                    .class("curve-preset-control-group");
                })
                .class("row");
            }
        });

        GenericUi::new_custom(cx, compressor_params, move |cx, param_ptr| {
            let name = unsafe { param_ptr.name() };

            // The compressor section heading is drawn before its first row
            if name == format!("{name_prefix} Offset") {
                Label::new(cx, "COMPRESSOR").class("section-label");
            }

            if name == format!("{name_prefix} Max Boost") {
                // The downwards path never adds gain, so its dead cap stays visible but inert
                HStack::new(cx, |cx| {
                    Label::new(cx, "Max Boost").class("label");
                    GenericUi::draw_widget(cx, compressor_params, param_ptr);
                })
                .class("row")
                .disabled(true);
            } else {
                HStack::new(cx, |cx| {
                    Label::new(
                        cx,
                        name.strip_prefix(&format!("{name_prefix} "))
                            .expect("missing prefix"),
                    )
                    .class("label");

                    GenericUi::draw_widget(cx, compressor_params, param_ptr);
                })
                .class("row");
            }
        });
    });
}

/// Creates the analyzer panel on the right side of the editor.
fn analyzer_column(cx: &mut Context) {
    HStack::new(cx, |cx| {
        // The analyzer shows and edits the curve points of the selected path
        SelectStateButton::new(
            cx,
            Data::selected_threshold_curve,
            0,
            "Down",
            "threshold-curve-button",
        );
        SelectStateButton::new(
            cx,
            Data::selected_threshold_curve,
            1,
            "Up",
            "threshold-curve-button",
        );
    })
    .class("curve-matching-button-group");
    Analyzer::new(
        cx,
        Data::analyzer_data,
        Data::sample_rate,
        Data::params,
        Data::selected_threshold_curve,
    );
}

/// A button that stores a value into one of the editor's `AtomicU8` selection states, e.g. which
/// path's threshold curve the analyzer edits, or which tab page is open.
struct SelectStateButton {
    selected_state: Arc<AtomicU8>,
    value: u8,
}

impl SelectStateButton {
    pub fn new<'a, LSelected, T>(
        cx: &'a mut Context,
        selected_state: LSelected,
        value: u8,
        label: impl Res<T> + Clone,
        css_class: &'static str,
    ) -> Handle<'a, Self>
    where
        LSelected: Lens<Target = Arc<AtomicU8>>,
        T: ToString,
    {
        let handle = Self {
            selected_state: selected_state.get(cx),
            value,
        }
        .build(cx, |cx| {
            Label::new(cx, label).hoverable(false);
        })
        .class(css_class);

        // Grey the button out while its value is already selected
        handle
            .disabled(selected_state.map(move |selected| selected.load(Ordering::Relaxed) == value))
    }
}

impl View for SelectStateButton {
    fn element(&self) -> Option<&'static str> {
        Some("param-button")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, meta| match window_event {
            WindowEvent::MouseDown(MouseButton::Left)
            | WindowEvent::MouseDoubleClick(MouseButton::Left)
            | WindowEvent::MouseTripleClick(MouseButton::Left) => {
                meta.consume();
                if cx.is_disabled() {
                    return;
                }
                self.selected_state.store(self.value, Ordering::Relaxed);
            }
            _ => {}
        });
    }
}

fn curve_preset_options(entries: &[CurvePresetEntry]) -> Vec<String> {
    let mut options = Vec::with_capacity(entries.len() + 1);
    options.push(String::from("Preset..."));
    options.extend(entries.iter().map(|entry| entry.preset.name.clone()));
    options
}

fn curve_preset_picklist(cx: &mut Context) {
    PickList::new(
        cx,
        ExportUiModel::curve_preset_options,
        ExportUiModel::selected_curve_preset_index,
        true,
    )
    .class("curve-preset-picklist")
    .tooltip(|cx| {
        Label::new(
            cx,
            "Threshold curve presets change the displayed threshold curve shape.",
        );
    })
    .on_select(|cx, index| {
        if index == 0 {
            return;
        }

        cx.emit(CurvePresetEvent::Apply(index));
    });
}

fn prompt_for_curve_preset_path(dir: &Path) -> Option<PathBuf> {
    if let Err(err) = std::fs::create_dir_all(dir) {
        nih_log!(
            "Could not create curve preset folder {}: {err}",
            dir.display()
        );
        return None;
    }

    rfd::FileDialog::new()
        .add_filter("Curve Preset", &["json"])
        .set_directory(dir)
        .set_file_name("curve-preset.json")
        .save_file()
}

/// Custom threshold mode selector that writes raw parameter automation events. There is one
/// picklist per compression path, each writing that path's own mode parameter.
fn mode_picklist(cx: &mut Context, path: CompressionPath) {
    let params = Data::params.get(cx);
    let mode_index = move |p: &SpectralCompressorParams| match path {
        CompressionPath::Downwards => p.thresholds.downwards.threshold.mode.value().to_index(),
        CompressionPath::Upwards => p.thresholds.upwards.threshold.mode.value().to_index(),
    };

    PickList::new(
        cx,
        Data::mode_options,
        Data::params.map(move |p| mode_index(p)),
        true,
    )
    .class("mode-picklist")
    .on_select(move |cx, index| {
        if mode_index(&params) == index {
            return;
        }

        let new_mode = ThresholdMode::from_index(index);
        let (normalized, mode_param) = match path {
            CompressionPath::Downwards => {
                let mode = &params.thresholds.downwards.threshold.mode;
                (mode.preview_normalized(new_mode), mode.as_ptr())
            }
            CompressionPath::Upwards => {
                let mode = &params.thresholds.upwards.threshold.mode;
                (mode.preview_normalized(new_mode), mode.as_ptr())
            }
        };

        cx.emit(RawParamEvent::BeginSetParameter(mode_param));
        cx.emit(RawParamEvent::SetParameterNormalized(
            mode_param, normalized,
        ));
        cx.emit(RawParamEvent::EndSetParameter(mode_param));
    });
}

/// Utility helper to render a titled editor column with optional CSS class.
fn make_column(
    cx: &mut Context,
    title: &str,
    css_class: Option<&str>,
    contents: impl FnOnce(&mut Context),
) {
    let col = VStack::new(cx, |cx| {
        Label::new(cx, title).class("column-title");

        contents(cx);
    })
    .class("column");

    if let Some(cls) = css_class {
        col.class(cls);
    }
}
