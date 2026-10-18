//! Child GUI windows for per-loop effects.
//!
//! Every enabled effect gets a button on its loop row; the button opens
//! this editor window. Two flavours, decided by the effect itself:
//!
//! * **Auto** (default, zero effect code): one labelled slider per param,
//!   straight from the [`EffectDesc`] — the same widget style the loop rows
//!   used before the editor existed.
//! * **Custom**: the effect exported `simplooper_effect_gui` ([`fx::
//!   EffectGuiRef`]). We allocate a canvas, build a [`GuiFrameV1`] and a
//!   [`GuiApiV1`] whose function pointers trampolines into egui draw calls
//!   ([`Bridge`]), and call the effect's `draw`. No egui type ever crosses
//!   the C boundary — only floats, pointers and u32s.
//!
//! One editor window at a time (simplooper's per-loop effects window and
//! the loop rows both route through it). The editor holds an `Arc<Instance>`
//! clone for its whole lifetime, so the audio instance (and the dlopen'd
//! library) cannot die under a live GUI.

use std::ffi::{c_char, c_void, CStr};
use std::sync::Arc;
use std::time::Instant;

use egui::{
    Align2, Checkbox, Color32, FontId, Id, Painter, Pos2, Rect, RichText, Sense, Slider, Stroke,
    StrokeKind, Ui, Vec2,
};

use crate::fx::{self, EffectDesc, GuiApiV1, GuiFrameV1, ParamDesc};

// ---------------------------------------------------------------------------
// Editor state
// ---------------------------------------------------------------------------

/// Custom (effect-drawn) editor flavour.
struct Custom {
    gui: fx::EffectGuiRef,
    /// The effect's per-window GUI state; freed when the editor drops.
    gs: *mut c_void,
}
// The pointer is raw but this module is the only holder; Drop calls destroy
// exactly once and the library stays mapped via the handle's Arc.
unsafe impl Send for Custom {}

/// One open effect editor window.
pub struct FxEditor {
    /// Loop and chain slot this editor is bound to (for persistence).
    pub loop_i: usize,
    pub slot: usize,
    pub stem: String,
    pub name: String,
    pub desc: EffectDesc,
    /// Keeps the audio instance (and its library) alive while GUIs draw.
    pub inst: Arc<fx::Instance>,
    custom: Option<Custom>,
    t0: Instant,
    last: f64,
    frame_no: u64,
    /// Param rides requested by the GUI since the last flush (the UI layer
    /// mirrors them into the persisted chain rows).
    pub pending: Vec<(usize, f32)>,
}

impl FxEditor {
    /// `gui` = the effect's custom GUI handle (None = auto slider window).
    /// A null `create` return falls back to the auto window as well.
    pub fn new(
        loop_i: usize,
        slot: usize,
        stem: String,
        desc: EffectDesc,
        inst: Arc<fx::Instance>,
        gui: Option<fx::EffectGuiRef>,
        sample_rate: f64,
    ) -> Self {
        let name = desc.name.clone();
        let custom = gui.and_then(|g| {
            let gs = g.create(sample_rate);
            (!gs.is_null()).then(|| Custom { gui: g, gs })
        });
        Self {
            loop_i,
            slot,
            stem,
            name,
            desc,
            inst,
            custom,
            t0: Instant::now(),
            last: 0.0,
            frame_no: 0,
            pending: Vec::new(),
        }
    }

    pub fn is_custom(&self) -> bool {
        self.custom.is_some()
    }

    /// Canvas size the window opens at (custom GUI hints, or the auto UI).
    pub fn size_hint(&self) -> (f32, f32) {
        match &self.custom {
            Some(c) => c.gui.size_hint(),
            None => (360.0, (28.0 * self.desc.params.len() as f32 + 56.0).clamp(80.0, 460.0)),
        }
    }
}

impl Drop for Custom {
    fn drop(&mut self) {
        // The FxEditor's Arc<Instance> is still alive here (field drop runs
        // after our drop body), which is exactly the guarantee the GUI ABI
        // documents for instance-state access.
        self.gui.destroy(self.gs);
    }
}

// ---------------------------------------------------------------------------
// Per-frame bridge (egui calls behind C function pointers)
// ---------------------------------------------------------------------------

/// Everything the trampolines need for one draw call. Built on the stack
/// per frame; `ctx` handed to the effect points at it for the call only.
struct Bridge<'a> {
    ui: &'a mut Ui,
    base: Pos2,
    inst: &'a fx::Instance,
    params: &'a [ParamDesc],
    pending: &'a mut Vec<(usize, f32)>,
}

impl Bridge<'_> {
    #[inline]
    fn pos(&self, x: f32, y: f32) -> Pos2 {
        self.base + Vec2::new(x, y)
    }
    #[inline]
    fn painter(&self) -> &Painter {
        self.ui.painter()
    }
    fn rect(&self, x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(self.pos(x, y), Vec2::new(w.max(4.0), h.max(4.0)))
    }
}

fn rgba(r: f32, g: f32, b: f32, a: f32) -> Color32 {
    let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u8;
    Color32::from_rgba_unmultiplied(c(r), c(g), c(b), c(a))
}

/// A range is usable only if both bounds are finite and ordered.
fn valid_range(min: f32, max: f32) -> bool {
    max > min && min.is_finite() && max.is_finite()
}

/// Stroke width safe for values coming from plugin scripts (NaN-proof).
fn stroke_w(w: f32) -> f32 {
    if w.is_finite() {
        w.clamp(0.5, 24.0)
    } else {
        1.0
    }
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_str().unwrap_or("").to_string()
}

fn norm(v: f32, min: f32, max: f32, curve: f32) -> f32 {
    if !valid_range(min, max) {
        return 0.0;
    }
    let v = v.clamp(min, max);
    if curve <= 0.0 && min > 0.0 {
        // log scale
        ((v / min).ln() / (max / min).ln()).clamp(0.0, 1.0)
    } else {
        ((v - min) / (max - min)).powf(if curve > 0.0 { curve } else { 1.0 }).clamp(0.0, 1.0)
    }
}

/// Map a normalized 0..1 position back to a value (inverse of `norm`).
fn denorm(n: f32, min: f32, max: f32, curve: f32) -> f32 {
    let n = n.clamp(0.0, 1.0);
    if !valid_range(min, max) {
        return min;
    }
    if curve <= 0.0 && min > 0.0 {
        min * (max / min).powf(n)
    } else {
        min + (max - min) * n.powf(if curve > 0.0 { curve } else { 1.0 })
    }
}

// ---- trampolines: one extern "C" fn per GuiApiV1 entry ---------------------

extern "C" fn tr_line(
    ctx: *mut c_void,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    width: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) {
    let br = unsafe { &mut *(ctx as *mut Bridge) };
    br.painter()
        .line_segment([br.pos(x0, y0), br.pos(x1, y1)], Stroke::new(stroke_w(width), rgba(r, g, b, a)));
}

extern "C" fn tr_polyline(
    ctx: *mut c_void,
    xy: *const f32,
    n: u32,
    closed: u32,
    width: f32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) {
    if xy.is_null() || n < 2 {
        return;
    }
    let br = unsafe { &mut *(ctx as *mut Bridge) };
    let pts = unsafe { core::slice::from_raw_parts(xy, n as usize * 2) };
    let stroke = Stroke::new(stroke_w(width), rgba(r, g, b, a));
    let segs = if closed != 0 { n as usize } else { n as usize - 1 };
    for i in 0..segs {
        let j = (i + 1) % n as usize;
        br.ui.painter().line_segment(
            [br.pos(pts[i * 2], pts[i * 2 + 1]), br.pos(pts[j * 2], pts[j * 2 + 1])],
            stroke,
        );
    }
}

extern "C" fn tr_rect_filled(ctx: *mut c_void, x: f32, y: f32, w: f32, h: f32, r: f32, g: f32, b: f32, a: f32) {
    let br = unsafe { &mut *(ctx as *mut Bridge) };
    br.painter().rect_filled(br.rect(x, y, w, h), 1.0, rgba(r, g, b, a));
}

extern "C" fn tr_rect_stroke(ctx: *mut c_void, x: f32, y: f32, w: f32, h: f32, width: f32, r: f32, g: f32, b: f32, a: f32) {
    let br = unsafe { &mut *(ctx as *mut Bridge) };
    br.ui.painter().rect_stroke(
        br.rect(x, y, w, h),
        1.0,
        Stroke::new(stroke_w(width), rgba(r, g, b, a)),
        StrokeKind::Inside,
    );
}

extern "C" fn tr_text(
    ctx: *mut c_void,
    x: f32,
    y: f32,
    size: f32,
    align: u32,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
    s: *const c_char,
) {
    let br = unsafe { &mut *(ctx as *mut Bridge) };
    let s = cstr(s);
    if s.is_empty() {
        return;
    }
    let a2 = match align {
        1 => Align2::CENTER_CENTER,
        2 => Align2::RIGHT_CENTER,
        _ => Align2::LEFT_CENTER,
    };
    br.painter().text(br.pos(x, y), a2, s, FontId::proportional(size.clamp(6.0, 128.0)), rgba(r, g, b, a));
}

extern "C" fn tr_text_width(ctx: *mut c_void, size: f32, s: *const c_char) -> f32 {
    let br = unsafe { &mut *(ctx as *mut Bridge) };
    let s = cstr(s);
    let font = FontId::proportional(size.clamp(6.0, 128.0));
    let g = br.ui.fonts_mut(|f| f.layout_no_wrap(s, font, Color32::WHITE));
    g.size().x
}

extern "C" fn tr_slider(
    ctx: *mut c_void,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    label: *const c_char,
    value: *mut f32,
    min: f32,
    max: f32,
    curve: f32,
) -> u32 {
    if value.is_null() || !valid_range(min, max) {
        return 0;
    }
    let br = unsafe { &mut *(ctx as *mut Bridge) };
    let rect = br.rect(x, y, w, if h < 14.0 { 18.0 } else { h });
    let mut v = unsafe { *value };
    let mut sl = Slider::new(&mut v, min..=max).show_value(false);
    if curve <= 0.0 && min > 0.0 {
        sl = sl.logarithmic(true);
    }
    let label = cstr(label);
    if !label.is_empty() {
        sl = sl.text(label);
    }
    let r = br.ui.put(rect, sl);
    if r.changed() {
        unsafe { *value = v.clamp(min, max) };
        1
    } else {
        0
    }
}

extern "C" fn tr_knob(
    ctx: *mut c_void,
    x: f32,
    y: f32,
    size: f32,
    label: *const c_char,
    value: *mut f32,
    min: f32,
    max: f32,
    curve: f32,
) -> u32 {
    if value.is_null() || !valid_range(min, max) {
        return 0;
    }
    let br = unsafe { &mut *(ctx as *mut Bridge) };
    let d = size.clamp(20.0, 256.0);
    let rect = Rect::from_min_size(br.pos(x, y), Vec2::new(d, d));
    let id = Id::new(("fxknob", x as i32, y as i32, br.base.x as i32, br.base.y as i32));
    let resp = br.ui.interact(rect, id, Sense::drag().union(Sense::hover()));
    let mut v = unsafe { *value };
    if resp.dragged() {
        // Vertical drag; full range in ~180 px, log/curve-aware so a nudge
        // moves the same visual distance on log params (freq, Q).
        let n = (norm(v, min, max, curve) - resp.drag_delta().y / 180.0).clamp(0.0, 1.0);
        v = denorm(n, min, max, curve);
        unsafe { *value = v };
    }
    // Draw.
    let c = rect.center();
    let rad = d * 0.34;
    let hovered = resp.hovered() || resp.dragged();
    let p = br.ui.painter();
    p.circle_filled(c, rad, Color32::from_rgb(if hovered { 60 } else { 48 }, if hovered { 64 } else { 52 }, if hovered { 74 } else { 60 }));
    p.circle_stroke(c, rad, Stroke::new(1.2, Color32::from_gray(if hovered { 170 } else { 110 })));
    let ang = (std::f32::consts::FRAC_PI_4 * -3.0) + norm(v, min, max, curve) * (std::f32::consts::FRAC_PI_4 * 6.0);
    let dir = Vec2::angled(ang + std::f32::consts::FRAC_PI_2);
    p.line_segment([c, c + dir * (rad * 0.92)], Stroke::new(1.6, Color32::from_rgb(150, 200, 255)));
    let label = cstr(label);
    if !label.is_empty() {
        p.text(
            Pos2::new(c.x, rect.max.y + 8.0),
            Align2::CENTER_TOP,
            label,
            egui::TextStyle::Small.resolve(br.ui.style()),
            Color32::from_gray(170),
        );
    }
    if hovered {
        br.ui.ctx().request_repaint();
    }
    if resp.dragged() || resp.drag_stopped() {
        1
    } else {
        0
    }
}

extern "C" fn tr_button(ctx: *mut c_void, x: f32, y: f32, w: f32, label: *const c_char) -> u32 {
    let br = unsafe { &mut *(ctx as *mut Bridge) };
    let rect = Rect::from_min_size(br.pos(x, y), Vec2::new(w.max(24.0), 20.0));
    let label = cstr(label);
    let r = br.ui.put(rect, egui::Button::new(RichText::new(label).small()));
    if r.clicked() {
        1
    } else {
        0
    }
}

extern "C" fn tr_checkbox(ctx: *mut c_void, x: f32, y: f32, label: *const c_char, on: *mut f32) -> u32 {
    if on.is_null() {
        return 0;
    }
    let br = unsafe { &mut *(ctx as *mut Bridge) };
    let mut b = unsafe { *on != 0.0 };
    let label = cstr(label);
    let where_ = Rect::from_min_size(br.pos(x, y), Vec2::ZERO);
    let r = br.ui.put(where_, Checkbox::new(&mut b, label));
    if r.changed() {
        unsafe { *on = b as u32 as f32 };
        1
    } else {
        0
    }
}

extern "C" fn tr_get_param(ctx: *mut c_void, k: u32) -> f32 {
    let br = unsafe { &mut *(ctx as *mut Bridge) };
    br.inst.param(k as usize)
}

extern "C" fn tr_set_param(ctx: *mut c_void, k: u32, v: f32) {
    let br = unsafe { &mut *(ctx as *mut Bridge) };
    let k = k as usize;
    // Clamp by the declared range (the auto sliders do it via their bounds;
    // custom GUIs get it for free too), remember it for persistence.
    let v = match br.params.get(k) {
        Some(p) => v.clamp(p.min.min(p.max), p.max.max(p.min)),
        None => v,
    };
    br.inst.set_param(k, v);
    if k < br.params.len() {
        br.pending.push((k, v));
    }
}

/// The vtable every frame hands the effect (all trampolines are stateless;
/// `ctx` and `n_params` are filled per frame).
const API_FNS: GuiApiV1 = GuiApiV1 {
    abi: fx::GUI_ABI_VERSION,
    ctx: core::ptr::null_mut(),
    line: tr_line,
    polyline: tr_polyline,
    rect_filled: tr_rect_filled,
    rect_stroke: tr_rect_stroke,
    text: tr_text,
    text_width: tr_text_width,
    slider: tr_slider,
    knob: tr_knob,
    button: tr_button,
    checkbox: tr_checkbox,
    get_param: tr_get_param,
    set_param: tr_set_param,
    n_params: 0,
};

// ---------------------------------------------------------------------------
// The window itself
// ---------------------------------------------------------------------------

/// Draw the editor window; returns false once the user closes it. Param
/// changes accumulate in `ed.pending` (and ride the live instance
/// immediately); the caller persists them and may drop the editor.
pub fn window(ui: &mut Ui, ed: &mut FxEditor) -> bool {
    // Live effects GUIs (spectrum meters, animated curves) repaint always.
    ui.ctx().request_repaint();
    let mut open = true;
    let (hw, hh) = ed.size_hint();
    let title = format!("{} — loop {}", ed.name.clone(), ed.loop_i + 1);
    egui::Window::new(RichText::new(title).strong())
        .open(&mut open)
        .resizable(!ed.is_custom())
        .default_width(hw + 16.0)
        .min_width(240.0)
        .auto_sized()
        .show(ui, |ui| {
            if ed.is_custom() {
                custom_ui(ui, ed, hh);
            } else {
                auto_ui(ui, ed);
            }
        });
    open
}

/// The effect-drawn canvas: one fixed-height area, frame info built fresh,
/// `draw` invoked with the trampoline vtable.
fn custom_ui(ui: &mut Ui, ed: &mut FxEditor, height: f32) {
    let w = (ui.available_width() - 8.0).max(220.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, height), Sense::hover());
    ui.painter().rect_filled(rect, 3.0, Color32::from_rgb(21, 23, 28));
    ui.painter().rect_stroke(rect, 3.0, Stroke::new(1.0, Color32::from_gray(58)), StrokeKind::Inside);

    // Frame bookkeeping.
    let now = ed.t0.elapsed().as_secs_f64();
    let dt = (now - ed.last).clamp(0.0, 0.1);
    ed.last = now;
    ed.frame_no += 1;
    let ptr = ui.input(|i| {
        (
            i.pointer.interact_pos(),
            i.pointer.any_down(),
            i.pointer.button_down(egui::PointerButton::Primary),
            i.pointer.button_pressed(egui::PointerButton::Primary),
            i.pointer.button_released(egui::PointerButton::Primary),
        )
    });
    let inside = ptr.0.is_some_and(|p| rect.contains(p));
    let (mx, my) = ptr.0.map(|p| (p.x - rect.min.x, p.y - rect.min.y)).unwrap_or((0.0, 0.0));
    let frame = GuiFrameV1 {
        width: rect.width(),
        height: rect.height(),
        mouse_x: mx,
        mouse_y: my,
        mouse_inside: inside as u32,
        mouse_down: (inside && ptr.2) as u32,
        mouse_pressed: (inside && ptr.3) as u32,
        mouse_released: (inside && ptr.4) as u32,
        time: now,
        dt,
        frame: ed.frame_no,
        sample_rate: ed.inst.sample_rate(),
    };

    // Split-borrow: every field touched here is distinct.
    let custom = ed.custom.as_mut().expect("custom_ui only when custom");
    let mut bridge = Bridge {
        ui,
        base: rect.min,
        inst: &ed.inst,
        params: &ed.desc.params,
        pending: &mut ed.pending,
    };
    let mut api = API_FNS;
    api.n_params = ed.inst.n_params() as u32;
    api.ctx = &mut bridge as *mut Bridge as *mut c_void;
    unsafe { custom.gui.draw(custom.gs, ed.inst.state_ptr(), &frame, &api) };
}

/// The default editor: label | slider | value, one row per param.
fn auto_ui(ui: &mut Ui, ed: &mut FxEditor) {
    if let Some(d) = ed.desc.description.as_deref() {
        ui.label(RichText::new(d).weak().small());
        ui.separator();
    }
    if ed.desc.params.is_empty() {
        ui.label(RichText::new("This effect has no parameters.").weak());
        return;
    }
    egui::Grid::new(("fxauto", ed.loop_i, ed.slot)).num_columns(3).spacing([8.0, 4.0]).show(ui, |ui| {
        for (k, p) in ed.desc.params.iter().enumerate() {
            let mut v = ed.inst.param(k);
            let mut sl = Slider::new(&mut v, p.min..=p.max);
            if p.min > 0.0 && p.max / p.min > 50.0 {
                sl = sl.logarithmic(true);
            }
            let r = ui.add_sized(Vec2::new(150.0, 0.0), sl).on_hover_text(format!("{:.2} … {:.2}", p.min, p.max));
            ui.label(&p.label);
            ui.label(RichText::new(format!("{v:.2}")).monospace().small());
            if r.changed() {
                ed.inst.set_param(k, v);
                ed.pending.push((k, v));
            }
            ui.end_row();
        }
    });
}
