//! egui front-end for simplooper: loop dashboard + settings + MIDI monitor.
//!
//! Layout:
//! - top bar: hamburger (settings), status, xruns, peak meter, master gain
//! - left side panel (optional): audio ports, MIDI bindings, easy quantize
//! - central: independent/dependent mode checkbox + four loop tracks, each
//!   with a waveform of the recorded audio and REC / OD / play / clear buttons
//! - bottom: MIDI monitor

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::Frame;
use egui::{Color32, RichText, Sense, Slider};

use crate::audio::{Backend, Cmd, Evt, PortInfo, MAX_LOOPS, TR_OD, TR_PLAY, TR_REC};
use crate::midi::{hex, MidiBind, MidiMsg};
use crate::quantize::Unit;

/// One row in the MIDI monitor log.
#[derive(Clone)]
struct MidiLine {
    ts: String,
    type_name: &'static str,
    describe: String,
    raw_hex: String,
}

/// Cached min/max columns for one track's waveform (invalidated by length or
/// content generation changes).
struct WaveCache {
    len: u32,
    gen: u64,
    cols: usize,
    data: Vec<[f32; 2]>,
}

pub struct App {
    backend: Backend,

    // --- audio / device state ---
    status: String,
    client_name: Option<String>,
    sample_rate: u32,
    avail_in: Vec<PortInfo>,
    avail_out: Vec<PortInfo>,
    sel_in: HashSet<String>,
    sel_out: HashSet<String>,
    applied: Option<(usize, usize)>,
    gain: f32,

    // --- loop dashboard ---
    show_settings: bool,
    dependent: bool,
    /// Track targeted by the keyboard shortcuts (keys 1–4 select it).
    selected: usize,
    /// True while a quantize text field has focus; keyboard shortcuts are
    /// suppressed then. Recomputed each frame in `ui()`.
    in_text_edit: bool,
    track_state: [u8; MAX_LOOPS],
    track_len: [u32; MAX_LOOPS],
    wave: [Option<WaveCache>; MAX_LOOPS],

    // --- easy quantize tool ---
    q_num: String,
    q_den: String,
    q_bars: String,
    q_unit_is_bar: bool,
    q_result: Option<String>,

    // --- MIDI bindings for REC/OD buttons ---
    rec_bind: [Option<MidiBind>; MAX_LOOPS],
    od_bind: [Option<MidiBind>; MAX_LOOPS],
    /// Recently seen bindable messages (most recent last), offered in the
    /// binding combo boxes.
    bind_history: Vec<MidiBind>,

    // --- MIDI monitor state ---
    last_midi: Option<MidiLine>,
    midi_log: VecDeque<MidiLine>,
    peak_disp: f32,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>, backend: Backend) -> Self {
        Self {
            backend,
            status: "starting…".into(),
            client_name: None,
            sample_rate: 0,
            avail_in: Vec::new(),
            avail_out: Vec::new(),
            sel_in: HashSet::new(),
            sel_out: HashSet::new(),
            applied: None,
            gain: 1.0,
            show_settings: false,
            dependent: false,
            selected: 0,
            in_text_edit: false,
            track_state: [0u8; MAX_LOOPS],
            track_len: [0u32; MAX_LOOPS],
            wave: core::array::from_fn(|_| None),
            q_num: "4".into(),
            q_den: "4".into(),
            q_bars: "2".into(),
            q_unit_is_bar: false,
            q_result: None,
            rec_bind: [None; MAX_LOOPS],
            od_bind: [None; MAX_LOOPS],
            bind_history: Vec::new(),
            last_midi: None,
            midi_log: VecDeque::with_capacity(300),
            peak_disp: 0.0,
        }
    }

    fn now_stamp() -> String {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            (t.as_secs() % 86400) / 3600,
            (t.as_secs() % 3600) / 60,
            t.as_secs() % 60,
            t.as_millis() % 1000
        )
    }

    /// Remember a bindable message so it can be picked in the binding UI.
    fn push_bind(&mut self, b: MidiBind) {
        if let Some(pos) = self.bind_history.iter().position(|x| *x == b) {
            self.bind_history.swap_remove(pos);
        }
        self.bind_history.push(b);
        if self.bind_history.len() > 32 {
            self.bind_history.remove(0);
        }
    }

    /// Global keyboard shortcuts (IDEA.md: "simple action mapping system with
    /// midi input and keyboard support"). Keys 1–4 select a loop; the
    /// transport keys act on the selected one. Press edges are read from the
    /// per-frame event list so key auto-repeat doesn't re-fire an action, and
    /// the keys are ignored while a text field (quantize fields) has focus.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // Ignore keys while a text field has focus (tracked in `ui()`), so
        // typing "1" into the quantize fields doesn't switch loops.
        if self.in_text_edit {
            return;
        }
        let pressed = |key: egui::Key| {
            ctx.input(|i| {
                i.events.iter().any(|e| matches!(
                    e,
                    egui::Event::Key { key: k, pressed: true, repeat: false, .. } if *k == key
                ))
            })
        };

        #[derive(PartialEq)]
        enum Act { Play, Rec, Od, Clear, Dep }

        for (i, key) in [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
        ]
        .into_iter()
        .enumerate()
        {
            if pressed(key) {
                self.selected = i;
            }
        }

        let mut act: Option<Act> = None;
        if pressed(egui::Key::Space) {
            act = Some(Act::Play);
        }
        if act.is_none() && pressed(egui::Key::R) {
            act = Some(Act::Rec);
        }
        if act.is_none() && pressed(egui::Key::O) {
            act = Some(Act::Od);
        }
        if act.is_none() && pressed(egui::Key::C) {
            act = Some(Act::Clear);
        }
        if act.is_none() && pressed(egui::Key::D) {
            act = Some(Act::Dep);
        }

        match act {
            Some(Act::Play) => self.backend.cmd(Cmd::TogglePlay { i: self.selected }),
            Some(Act::Rec) => self.backend.cmd(Cmd::ToggleRecord { i: self.selected }),
            Some(Act::Od) => self.backend.cmd(Cmd::ToggleOverdub { i: self.selected }),
            Some(Act::Clear) => self.backend.cmd(Cmd::ClearTrack { i: self.selected }),
            Some(Act::Dep) => {
                self.dependent = !self.dependent;
                self.backend.cmd(Cmd::SetDependent(self.dependent));
            }
            None => {}
        }
    }

    fn handle_evt(&mut self, evt: Evt) {
        match evt {
            Evt::Unavailable(msg) => self.status = msg,
            Evt::Ready { client_name, sample_rate } => {
                self.client_name = Some(client_name.clone());
                self.sample_rate = sample_rate;
                self.status = "JACK ready — select ports, then Apply".into();
            }
            Evt::Ports { ins, outs } => {
                self.avail_in = ins;
                self.avail_out = outs;
            }
            Evt::Applied { ins, outs } => {
                self.applied = Some((ins, outs));
                self.status = format!("applied: {} in / {} out", ins, outs);
            }
            Evt::LoopState { states, lens } => {
                self.track_state = states;
                self.track_len = lens;
            }
            Evt::Quantized { summary } => {
                self.q_result = Some(summary);
            }
            Evt::Midi(raw) => {
                if let Some(msg) = MidiMsg::parse(&raw.bytes) {
                    // Bindable messages drive the bound REC/OD buttons.
                    if let Some(bind) = MidiBind::from_msg(&msg) {
                        self.push_bind(bind);
                        for i in 0..MAX_LOOPS {
                            if self.rec_bind[i] == Some(bind) {
                                self.backend.cmd(Cmd::ToggleRecord { i });
                            }
                            if self.od_bind[i] == Some(bind) {
                                self.backend.cmd(Cmd::ToggleOverdub { i });
                            }
                        }
                    }
                    let line = MidiLine {
                        ts: Self::now_stamp(),
                        type_name: msg.type_name(),
                        describe: msg.describe(),
                        raw_hex: hex(&raw.bytes),
                    };
                    // "print the last detected MIDI input" — also to stdout.
                    println!(
                        "[{}] {:<12} {}  | {}",
                        line.ts, line.type_name, line.describe, line.raw_hex
                    );
                    self.last_midi = Some(line.clone());
                    self.midi_log.push_front(line);
                    while self.midi_log.len() > 300 {
                        self.midi_log.pop_back();
                    }
                }
            }
            Evt::Xrun => {}
            Evt::Error(e) => self.status = e,
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        // Keep the update loop ticking while idle so backend events (MIDI,
        // port changes) are drained promptly; eframe otherwise pauses frames
        // when nothing has changed.
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        self.handle_shortcuts(ctx);

        let mut changed = false;
        while let Ok(evt) = self.backend.evt_rx.try_recv() {
            self.handle_evt(evt);
            changed = true;
        }

        // Decay the displayed peak toward the live value.
        let p = f32::from_bits(self.backend.peak.load(Ordering::Relaxed));
        self.peak_disp = p.max(self.peak_disp * 0.9);

        if changed {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut Frame) {
        // Push the gain slider value into the audio thread (f32 as bits).
        self.backend.gain.store(self.gain.to_bits(), Ordering::Relaxed);

        // Recomputed by the text-edit widgets below while they have focus.
        self.in_text_edit = false;

        top_bar(ui, self);
        if self.show_settings {
            settings_panel(ui, self);
        }
        midi_monitor(ui, self);
        central_panels(ui, self);
    }
}

fn top_bar(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::top("topbar")
        .exact_size(40.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // Hamburger (settings) button: three horizontal bars.
                let size = egui::vec2(26.0, 18.0);
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                if rect.width() > 4.0 && rect.height() > 4.0 {
                    let p = ui.painter();
                    for k in 0..3u32 {
                        let y = rect.min.y + 4.0 + k as f32 * 5.0;
                        p.rect_filled(
                            egui::Rect::from_min_size(egui::pos2(rect.min.x + 3.0, y), egui::vec2(20.0, 2.0)),
                            1.0,
                            if app.show_settings { Color32::WHITE } else { Color32::from_rgb(200, 200, 205) },
                        );
                    }
                }
                let resp = resp.on_hover_text("Settings");
                if resp.clicked() {
                    app.show_settings = !app.show_settings;
                }

                ui.separator();
                ui.heading(RichText::new("simplooper").strong());
                let status = match &app.client_name {
                    Some(name) => format!("{} @ {} Hz", name, app.sample_rate),
                    None => app.status.clone(),
                };
                ui.label(RichText::new(status).weak());

                if let Some((i, o)) = app.applied {
                    ui.label(RichText::new(format!("applied: {} in / {} out", i, o)).weak());
                }

                let xruns = app.backend.xruns.load(Ordering::Relaxed);
                if xruns > 0 {
                    ui.label(RichText::new(format!("xruns: {}", xruns)).color(Color32::YELLOW));
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Peak meter.
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(90.0, 8.0), Sense::hover());
                    let painter = ui.painter();
                    painter.rect_filled(rect, 1.0, Color32::from_rgb(40, 40, 45));
                    let frac = (app.peak_disp * 0.9).clamp(0.0, 1.0);
                    let color = if app.peak_disp > 0.99 {
                        Color32::RED
                    } else if app.peak_disp > 0.7 {
                        Color32::YELLOW
                    } else {
                        Color32::LIGHT_GREEN
                    };
                    if frac > 0.01 {
                        painter.rect_filled(
                            egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * frac, rect.height())),
                            1.0,
                            color,
                        );
                    }
                    ui.label(RichText::new(format!("peak {:.2}", app.peak_disp)).weak());

                    ui.add(Slider::new(&mut app.gain, 0.0..=2.0).suffix("× gain"));
                });
            });
        });
}

/// Left settings panel: audio ports, MIDI bindings, easy quantize tool.
fn settings_panel(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::left("settings")
        .resizable(true)
        .default_size(370.0)
        .min_size(280.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading(RichText::new("SETTINGS").strong());

                // ---- audio ports -------------------------------------------------
                ui.separator();
                ui.label(RichText::new("AUDIO PORTS").strong());
                port_list(
                    ui,
                    "INPUTS",
                    "remote output ports (sources)",
                    &app.avail_in,
                    &mut app.sel_in,
                    160.0,
                );
                port_list(
                    ui,
                    "OUTPUTS",
                    "remote input ports (sinks)",
                    &app.avail_out,
                    &mut app.sel_out,
                    160.0,
                );
                ui.horizontal(|ui| {
                    if ui.button("Apply Selection").clicked() {
                        let ins: Vec<String> = app.sel_in.iter().cloned().collect();
                        let outs: Vec<String> = app.sel_out.iter().cloned().collect();
                        app.backend.cmd(Cmd::Apply { ins, outs });
                    }
                    if ui.button("Stop").clicked() {
                        app.backend.cmd(Cmd::Apply { ins: vec![], outs: vec![] });
                    }
                    if ui.button("Refresh").clicked() {
                        app.backend.cmd(Cmd::Refresh);
                    }
                });

                // ---- MIDI bindings -------------------------------------------------
                ui.separator();
                ui.label(RichText::new("MIDI BINDINGS").strong());
                ui.label(
                    RichText::new(
                        "Send a NOTE ON / CC / PROGRAM CHANGE from your controller; it \
                         appears in the lists below. A bound button fires on every \
                         matching message (value/velocity ignored).",
                    )
                    .weak()
                    .small(),
                );
                for i in 0..MAX_LOOPS {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("Loop {}", i + 1)).strong());
                        bind_combo(ui, &mut app.rec_bind[i], &app.bind_history, "REC");
                        bind_combo(ui, &mut app.od_bind[i], &app.bind_history, "OD");
                    });
                }

                // ---- keyboard shortcuts --------------------------------------------
                ui.separator();
                ui.label(RichText::new("KEYBOARD SHORTCUTS").strong());
                ui.label(
                    RichText::new(
                        "Keys 1–4 select a loop (highlighted); the transport keys act on \
                         the selected one. Keys are ignored while a text field has focus.",
                    )
                    .weak()
                    .small(),
                );
                for (key, what) in [
                    ("1–4", "select loop"),
                    ("space", "play / stop the selected loop"),
                    ("R", "record toggle (selected loop)"),
                    ("O", "overdub toggle (selected loop)"),
                    ("C", "clear the selected loop"),
                    ("D", "toggle dependent mode"),
                ] {
                    ui.horizontal(|ui| {
                        ui.monospace(key);
                        ui.label(what);
                    });
                }

                // ---- easy quantize -------------------------------------------------
                ui.separator();
                ui.label(RichText::new("EASY QUANTIZE").strong());
                ui.label(
                    RichText::new(
                        "Derives the grid from Loop 1's recorded length: \
                         bar = len₁ ÷ bars, beat = bar ÷ numerator. Every loop is then \
                         snapped to the nearest multiple of the chosen unit; the cut is \
                         refined to a zero crossing and the seam crossfaded.",
                    )
                    .weak()
                    .small(),
                );
                ui.horizontal(|ui| {
                    ui.label("time sig.");
                    let r_num = ui.add(egui::TextEdit::singleline(&mut app.q_num).desired_width(40.0));
                    ui.label("/");
                    let r_den = ui.add(egui::TextEdit::singleline(&mut app.q_den).desired_width(40.0));
                    app.in_text_edit = app.in_text_edit || (r_num.has_focus() || r_den.has_focus());
                });
                ui.horizontal(|ui| {
                    ui.label("bars in loop 1:");
                    let r_bars = ui.add(egui::TextEdit::singleline(&mut app.q_bars).desired_width(60.0));
                    app.in_text_edit = app.in_text_edit || r_bars.has_focus();
                });
                ui.horizontal(|ui| {
                    ui.label("quantize to:");
                    ui.radio_value(&mut app.q_unit_is_bar, false, "beat");
                    ui.radio_value(&mut app.q_unit_is_bar, true, "bar");
                });
                if ui.button("Quantize all loops").clicked() {
                    match (
                        app.q_num.parse::<u32>(),
                        app.q_den.parse::<u32>(),
                        app.q_bars.parse::<u32>(),
                    ) {
                        (Ok(n), Ok(d), Ok(b)) if n >= 1 && d >= 1 && b >= 1 => {
                            let unit = if app.q_unit_is_bar { Unit::Bar } else { Unit::Beat };
                            app.backend.cmd(Cmd::Quantize { num: n, den: d, bars: b, unit });
                        }
                        _ => {
                            app.status = "quantize: enter positive integers for time signature and bars".into();
                        }
                    }
                }
                if let Some(r) = &app.q_result {
                    ui.add_space(4.0);
                    egui::Frame::group(ui.style())
                        .fill(Color32::from_rgb(35, 40, 50))
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.label(RichText::new(r).monospace().small());
                        });
                }
            });
        });
}

/// Combo box for picking a MIDI binding (or none) for one button.
fn bind_combo(ui: &mut egui::Ui, slot: &mut Option<MidiBind>, history: &[MidiBind], label: &str) {
    egui::ComboBox::from_label(label)
        .selected_text(match *slot {
            Some(b) => b.label(),
            None => "—".to_string(),
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(slot, None, "— none —");
            for b in history {
                ui.selectable_value(slot, Some(*b), b.label());
            }
        });
}

/// Central dashboard: mode checkbox + four loop tracks.
fn central_panels(ui: &mut egui::Ui, app: &mut App) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("loop mode:").strong());
            let dep = ui.checkbox(&mut app.dependent, "dependent").on_hover_text(
                "Dependent looping: the longest loop is the master — every other loop \
                 wraps on top of it, and the whole ensemble restarts together when the \
                 longest one ends.\n\nUse only with properly quantized loops (Settings → \
                 Easy Quantize), or you will hear skips at the loop points.",
            );
            if dep.changed() {
                app.backend.cmd(Cmd::SetDependent(app.dependent));
            }
            ui.separator();
            ui.label(
                RichText::new("independent = each loop plays on its own clock").weak().small(),
            );
        });
        ui.label(
            RichText::new("keys: 1–4 select · space play/stop · R rec · O od · C clear · D mode")
                .weak()
                .small(),
        );
        ui.add_space(10.0);

        for i in 0..MAX_LOOPS {
            track_row(ui, app, i);
            ui.add_space(8.0);
        }
    });
}

/// One loop track: label + waveform + REC/OD/play/clear buttons. The row the
/// keyboard shortcuts target (keys 1–4) is highlighted with a frame.
fn track_row(ui: &mut egui::Ui, app: &mut App, i: usize) {
    if app.selected == i {
        egui::Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(110, 165, 205)))
            .inner_margin(4.0)
            .show(ui, |ui| track_row_inner(ui, app, i));
    } else {
        track_row_inner(ui, app, i);
    }
}

fn track_row_inner(ui: &mut egui::Ui, app: &mut App, i: usize) {
    let state = app.track_state[i];
    let len = app.track_len[i];
    let secs = if app.sample_rate > 0 {
        len as f32 / app.sample_rate as f32
    } else {
        0.0
    };
    let (state_txt, state_col) = match state {
        TR_REC => ("recording…", Color32::RED),
        TR_PLAY => ("playing", Color32::LIGHT_GREEN),
        TR_OD => ("overdubbing", Color32::YELLOW),
        _ if len > 0 => ("ready", Color32::GRAY),
        _ => ("idle", Color32::GRAY),
    };

    ui.horizontal(|ui| {
        // Left: name + state.
        ui.vertical(|ui| {
            ui.set_min_width(128.0);
            ui.label(RichText::new(format!("Loop {}", i + 1)).strong());
            ui.label(
                RichText::new(format!("{state_txt} · {secs:.2} s"))
                    .color(state_col)
                    .monospace()
                    .small(),
            );
        });

        // Middle: waveform of the recorded audio (reserve room for buttons).
        let wave_w = (ui.available_width() - 250.0).clamp(60.0, f32::INFINITY);
        let eng = app.backend.engine.clone();
        let data = eng.read_range(i, len);
        draw_wave(ui, &mut app.wave[i], len, state, eng.generation(), data, wave_w);

        // Right: transport buttons.
        let mut b_rec = egui::Button::new(RichText::new("● REC").monospace());
        if state == TR_REC {
            b_rec = b_rec.fill(Color32::from_rgb(210, 70, 70));
        }
        let r_rec = ui.add(b_rec).on_hover_text(
            "Record: fresh capture (replaces current content). Key: R. Bindable via MIDI.",
        );
        if r_rec.clicked() {
            app.backend.cmd(Cmd::ToggleRecord { i });
        }

        let mut b_od = egui::Button::new(RichText::new("◉ OD").monospace());
        if state == TR_OD {
            b_od = b_od.fill(Color32::from_rgb(210, 170, 60));
        }
        let r_od = ui.add(b_od).on_hover_text(
            "Overdub: play while writing live input on top. Key: O. Bindable via MIDI.",
        );
        if r_od.clicked() {
            app.backend.cmd(Cmd::ToggleOverdub { i });
        }

        let (p_label, p_active) = match state {
            TR_PLAY | TR_OD => ("■", true),
            _ => ("▶", false),
        };
        let mut b_play = egui::Button::new(RichText::new(p_label).monospace());
        if p_active {
            b_play = b_play.fill(Color32::from_rgb(90, 140, 90));
        }
        let r_play = ui.add(b_play).on_hover_text("Play / stop this loop (space).");
        if r_play.clicked() {
            app.backend.cmd(Cmd::TogglePlay { i });
        }

        let r_clr = ui
            .add(egui::Button::new(RichText::new("✕").monospace()))
            .on_hover_text("Stop and erase this loop (C).");
        if r_clr.clicked() {
            app.backend.cmd(Cmd::ClearTrack { i });
        }
    });
}

/// Draw a track's waveform (min/max columns over the recorded region).
fn draw_wave(
    ui: &mut egui::Ui,
    cache: &mut Option<WaveCache>,
    len: u32,
    state: u8,
    gen: u64,
    data: &[f32],
    w: f32,
) {
    let h = 60.0f32;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w.max(40.0), h), Sense::hover());
    let painter = ui.painter();

    let color = match state {
        TR_REC => Color32::from_rgb(255, 96, 96),
        TR_PLAY => Color32::from_rgb(128, 224, 128),
        TR_OD => Color32::from_rgb(240, 208, 96),
        _ => Color32::from_rgb(150, 165, 190),
    };

    painter.rect_stroke(
        rect,
        1.0,
        egui::Stroke::new(1.0, Color32::from_rgb(70, 74, 84)),
        egui::StrokeKind::Inside,
    );
    let mid = rect.center().y;
    painter.line_segment(
        [egui::pos2(rect.min.x, mid), egui::pos2(rect.max.x, mid)],
        egui::Stroke::new(1.0, Color32::from_rgb(95, 100, 110)),
    );

    if len > 0 {
        let cols = (rect.width() / 2.0) as usize;
        let need = match cache {
            Some(c) => c.len != len || c.gen != gen || c.cols != cols,
            None => true,
        };
        if need {
            *cache = Some(WaveCache { len, gen, cols, data: wave_cols(data, cols) });
        }
        if let Some(c) = cache {
            let colw = rect.width() / c.cols.max(1) as f32;
            let scale = rect.height() / 2.0 - 2.0;
            for (k, [mn, mx]) in c.data.iter().enumerate() {
                let top = (mid - mx * scale).clamp(rect.min.y + 1.0, rect.max.y - 1.0);
                let bot = (mid - mn * scale).clamp(rect.min.y + 1.0, rect.max.y - 1.0);
                let x = rect.min.x + k as f32 * colw;
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(x, top.min(bot)),
                        egui::vec2(colw.max(1.0) - 0.5, (bot - top).max(1.0)),
                    ),
                    0.0,
                    color,
                );
            }
        }
    } else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "no recording yet",
            egui::FontId::proportional(11.0),
            Color32::from_rgb(110, 115, 125),
        );
    }

    resp.on_hover_text(format!("{} samples recorded", len));
}

/// Min/max per column with bounded stride sampling (fast even for 60 s buffers).
fn wave_cols(buf: &[f32], cols: usize) -> Vec<[f32; 2]> {
    let n = buf.len();
    if n == 0 || cols == 0 {
        return Vec::new();
    }
    let mut out: Vec<[f32; 2]> = vec![[0.0, 0.0]; cols];
    let stride = (n / (cols * 8)).max(1);
    for c in 0..cols {
        let lo = c * n / cols;
        let hi = ((c + 1) * n / cols).max(lo + 1);
        let mut mn = f32::MAX;
        let mut mx = f32::MIN;
        let mut k = lo;
        while k < hi {
            let v = buf[k];
            if v < mn {
                mn = v;
            }
            if v > mx {
                mx = v;
            }
            k += stride;
        }
        out[c] = [mn, mx];
    }
    out
}

fn port_list(
    ui: &mut egui::Ui,
    title: &str,
    hint: &str,
    avail: &[PortInfo],
    sel: &mut HashSet<String>,
    max_h: f32,
) {
    ui.label(RichText::new(format!("{}  ({})", title, avail.len())).strong());
    ui.label(RichText::new(hint).weak().small());
    egui::ScrollArea::vertical()
        .max_height(max_h)
        .show(ui, |ui| {
            if avail.is_empty() {
                ui.label(RichText::new("no remote audio ports found").weak());
                return;
            }
            let mut last_client = String::new();
            for p in avail {
                if p.client != last_client {
                    last_client = p.client.clone();
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("▸ {}", p.client)).strong());
                }
                let short = p.name.split(':').nth(1).unwrap_or(&p.name);
                let mut checked = sel.contains(&p.name);
                if ui.checkbox(&mut checked, short).clicked() {
                    if checked {
                        sel.insert(p.name.clone());
                    } else {
                        sel.remove(&p.name);
                    }
                }
                ui.label(
                    RichText::new(format!("  {}", p.name)).weak().small().monospace(),
                );
            }
        });
}

fn midi_monitor(ui: &mut egui::Ui, app: &mut App) {
    egui::Panel::bottom("midi")
        .resizable(true)
        .min_size(140.0)
        .default_size(220.0)
        .show(ui, |ui| {
            ui.label(RichText::new("MIDI MONITOR").strong());
            ui.label(
                RichText::new("last detected MIDI input (also printed to stdout)")
                    .weak()
                    .small(),
            );

            if let Some(last) = &app.last_midi {
                ui.add_space(4.0);
                egui::Frame::group(ui.style())
                    .fill(Color32::from_rgb(35, 40, 50))
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("[{}]", last.ts)).weak().monospace());
                            ui.label(RichText::new(last.type_name).strong().monospace());
                            ui.label(RichText::new(&last.describe).monospace());
                            ui.separator();
                            ui.label(
                                RichText::new(format!("| {}", last.raw_hex)).weak().monospace(),
                            );
                        });
                    });
            } else {
                ui.add_space(4.0);
                ui.label(
                    RichText::new("waiting for MIDI input on simplooper:midi_in …").weak(),
                );
            }

            egui::ScrollArea::vertical()
                .min_scrolled_height(f32::INFINITY)
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    // Newest first (log is push_front).
                    for line in app.midi_log.iter() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("[{}]", line.ts)).weak().monospace());
                            ui.label(RichText::new(line.type_name).monospace());
                            ui.label(RichText::new(&line.describe).weak().monospace());
                            ui.label(
                                RichText::new(format!("| {}", line.raw_hex)).weak().monospace(),
                            );
                        });
                    }
                });
        });
}
