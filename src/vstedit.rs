//! Hosting VST2 custom editor windows (X11 embedding).
//!
//! # The standard protocol (VST2.4 spec, verified against Ardour, Carla,
//! JUCE, LMMS and yabridge host implementations)
//!
//! * `effEditOpen` (opcode 14) takes a native window handle in `ptr`. On
//!   X11 that is the **XID** of the parent window cast to a pointer; the
//!   plugin creates its editor window as a child of it (reparenting works
//!   because X object IDs are server-global — the plugin uses its own X
//!   connection and the window manager keeps decorating our container).
//! * `effEditIdle` (opcode 19) must be pumped from the GUI thread at a
//!   steady rate while the editor is open; FLTK- and DPF-based editors run
//!   their event handling from it.
//! * `effEditGetRect` (opcode 13) receives an `ERect**` — the plugin writes
//!   the address of its rect there; `ERect` holds 16-bit
//!   `top/left/bottom/right`. The host sizes its container to it; that is
//!   the resize path for fixed-size editors, while VST2.4 resizable plugins
//!   additionally call back with `audioMasterSizeWindow`.
//! * `effEditClose` (opcode 15) must be dispatched before the plugin is
//!   unloaded; the editor thread owns that ordering.
//!
//! The open is VERIFIED, not trusted: the plugin must either reparent a
//! child window with real geometry into the container or paint pixels on
//! it (compared against a sentinel background — never "non-black", the
//! container's own background is not content). Nothing appearing within
//! ~3 s means the editor is refused, the window destroyed, and the UI
//! falls back to sliders — a blank, dead panel is never left on screen.
//! The container pumps X events and honours `WM_DELETE_WINDOW` /
//! `DestroyNotify`, so the window-manager close button always works.
//!
//! THREADING: every plugin control-plane call — instantiation included —
//! runs on ONE shared "vst2-gui" thread (see `vsthost::open`), while the
//! audio thread keeps processing. This is not optional: GUI toolkits
//! (JUCE's `MessageManager`, FLTK) bind their global event loop to the
//! thread that first touches the plugin and silently refuse to show any
//! window opened elsewhere. Opening editors on a per-editor thread while
//! instantiation ran on the UI thread left every JUCE editor permanently
//! unmapped (the v10.2 "no plugin GUIs at all" report). One thread makes
//! the affinity assumption true by construction; editors are pumped
//! there in rotation (verify + idle) as state machines rather than
//! blocking loops, so several editors coexist.
//!
//! VST3 editors are not hosted through this module: the `vst3-host`
//! realtime runner confines the plugin to its processing thread, and the
//! crate's own `EmbeddedEditor` rejects Wayland window handles; see
//! docs/PLUGINS.md "VSTi custom GUIs" for the full diagnosis.

use std::ffi::{c_void, CString};
use std::os::raw::c_int;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::vsthost::Vst2Handle;

// ------------------------------------------------------------------ xlib FFI
#[repr(C)]
struct Display {
    _private: [u8; 0],
}
type XWindow = u64;
type Atom = u64;

#[link(name = "X11")]
extern "C" {
    fn XOpenDisplay(name: *const i8) -> *mut Display;
    fn XCloseDisplay(d: *mut Display) -> c_int;
    fn XCreateSimpleWindow(
        d: *mut Display,
        parent: XWindow,
        x: c_int,
        y: c_int,
        w: c_int,
        h: c_int,
        border: c_int,
        border_color: u64,
        background: u64,
    ) -> XWindow;
    fn XDestroyWindow(d: *mut Display, w: XWindow) -> c_int;
    fn XDestroySubwindows(d: *mut Display, w: XWindow) -> c_int;
    fn XMapWindow(d: *mut Display, w: XWindow) -> c_int;
    fn XSelectInput(d: *mut Display, w: XWindow, mask: u64) -> c_int;
    fn XStoreName(d: *mut Display, w: XWindow, name: *const i8) -> c_int;
    fn XInternAtom(d: *mut Display, name: *const i8, only_if_exists: c_int) -> Atom;
    fn XSetWMProtocols(d: *mut Display, w: XWindow, protocols: *mut Atom, count: c_int) -> c_int;
    fn XSync(d: *mut Display, discard: c_int) -> c_int;
    fn XFlush(d: *mut Display) -> c_int;
    fn XPending(d: *mut Display) -> c_int;
    fn XNextEvent(d: *mut Display, ev: *mut [u64; 24]) -> c_int;
    fn XResizeWindow(d: *mut Display, w: XWindow, width: c_int, height: c_int) -> c_int;
    fn XQueryTree(
        d: *mut Display,
        w: XWindow,
        root: *mut XWindow,
        parent: *mut XWindow,
        children: *mut *mut XWindow,
        n_children: *mut c_int,
    ) -> c_int;
    fn XFree(data: *mut c_void) -> c_int;
    fn XGetGeometry(
        d: *mut Display,
        w: XWindow,
        root: *mut XWindow,
        x: *mut c_int,
        y: *mut c_int,
        ww: *mut u32,
        hh: *mut u32,
        border: *mut u32,
        depth: *mut u32,
    ) -> c_int;
    fn XGetWindowAttributes(
        d: *mut Display,
        w: XWindow,
        attrs: *mut XWindowAttributes,
    ) -> c_int;
    fn XSetErrorHandler(
        handler: Option<extern "C" fn(*mut c_void, *mut c_void) -> c_int>,
    ) -> Option<extern "C" fn(*mut c_void, *mut c_void) -> c_int>;
    fn XSendEvent(
        d: *mut Display,
        w: XWindow,
        propagate: c_int,
        mask: u64,
        ev: *const u64,
    ) -> c_int;
}

extern "C" fn x_error_noop(_display: *mut c_void, _event: *mut c_void) -> c_int {
    // X races (a plugin window vanishing mid-query) must never kill the
    // host; standard practice for plugin wrappers.
    0
}

const STRUCTURE_NOTIFY_MASK: u64 = 1 << 20;
// X protocol event codes (XSendEvent(7X)): 17 DestroyNotify, 18 UnmapNotify,
// 21 ReparentNotify, 22 ConfigureNotify, 33 ClientMessage. (An earlier
// comment block here called 18 "ReparentNotify" — 18 is UnmapNotify.)
const EVENT_DESTROY_NOTIFY: i32 = 17;
const EVENT_REPARENT_NOTIFY: i32 = 21;
const EVENT_CONFIGURE_NOTIFY: i32 = 22;
const EVENT_CLIENT_MESSAGE: i32 = 33;

/// Distinctive container background. The "did the plugin paint?" probe
/// compares pixels against THIS, never against black: the old probe
/// treated the window's own grey background as painted content, so a
/// broken plugin that never drew a pixel was accepted and the user got a
/// permanent blank panel (the amsynth report, 2026-10). Black is equally
/// unusable — a mapped-but-never-exposed window reads all-zero under a
/// compositing manager, indistinguishable from drawn black.
const CONTAINER_BG: u64 = 0x2A16B4;

// VST2 editor opcodes (aeff.h / vestige.h, cross-checked against the
// vendored vst2 crate's OpCode enum).
const EFF_EDIT_GET_RECT: i32 = 13;
const EFF_EDIT_OPEN: i32 = 14;
const EFF_EDIT_CLOSE: i32 = 15;
const EFF_EDIT_IDLE: i32 = 19;

/// `ERect` from the VST2 SDK: four 16-bit screen coordinates.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ERect {
    top: i16,
    left: i16,
    bottom: i16,
    right: i16,
}

/// Opaque XImage handle (ZPixmap snapshot of the editor container).
#[repr(C)]
struct XImage {
    _private: [u8; 0],
}

#[link(name = "X11")]
extern "C" {
    fn XGetImage(
        d: *mut Display,
        w: XWindow,
        x: c_int,
        y: c_int,
        w2: u32,
        h2: u32,
        planemask: u64,
        format: c_int,
    ) -> *mut XImage;
    fn XGetPixel(img: *mut XImage, x: c_int, y: c_int) -> u64;
    fn XDestroyImage(img: *mut XImage) -> c_int;
}

/// `XWindowAttributes` (X11/X.h) treated as an opaque 136-byte buffer on
/// x86_64: we read only two fields, `width` (offset 8) and `map_state`
/// (offset 92), whose layout is pinned by the `v102_xattr_offsets_pinned`
/// test against the live X server. Declaring the real field types instead
/// would depend on a 40-year-old header layout staying frozen.
#[repr(C, align(8))]
struct XWindowAttributes {
    raw: [u8; 136],
}

/// X.h map-state enum.
const IS_VIEWABLE: i32 = 2;

impl XWindowAttributes {
    fn new() -> Self {
        Self { raw: [0; 136] }
    }
    fn width(&self) -> i32 {
        i32::from_ne_bytes(self.raw[8..12].try_into().unwrap())
    }
    fn map_state(&self) -> i32 {
        i32::from_ne_bytes(self.raw[92..96].try_into().unwrap())
    }
}

/// True when `win` exists and is currently mapped/visible (a plugin
/// child that was created and resized but never MAPPED is not evidence
/// of a live editor — the amsynth "solid blue forever" report: its
/// editor throws mid-build and leaves an unmapped sized child that
/// `XGetGeometry` alone happily accepted).
unsafe fn is_viewable(d: *mut Display, win: XWindow) -> bool {
    let mut attrs = XWindowAttributes::new();
    XGetWindowAttributes(d, win, &mut attrs) != 0
        && attrs.map_state() == IS_VIEWABLE
        && attrs.width() > 0
}

/// Drain and DISPATCH every pending X event on `d`, returning true when
/// one of them says the window must close: a `WM_DELETE_WINDOW` client
/// message (the window manager's close button), or the container's own
/// DestroyNotify (xkill / forced close).
///
/// The old pump broke on `XNextEvent`'s return value — XNextEvent returns
/// 0 on SUCCESS, so every event it read was popped off the queue and
/// dropped before the match, and the close button did nothing (the
/// "cannot close plugin window" report). XNextEvent blocking is not a
/// risk either: the caller only calls it while `XPending > 0`.
unsafe fn drain_events(d: *mut Display, win: XWindow, wm_protocols: Atom, wm_delete: Atom) -> bool {
    let mut drained = 0;
    while XPending(d) > 0 && drained < 256 {
        let mut ev = [0u64; 24];
        XNextEvent(d, &mut ev);
        drained += 1;
        match (ev[0] as u32) as i32 {
            EVENT_CLIENT_MESSAGE => {
                // XClientMessageEvent on x86_64: message_type atom
                // (unsigned long) at byte 40 -> ev[5]; data.l[0]
                // at byte 56 -> ev[7].
                if ev[5] == wm_protocols && ev[7] == wm_delete {
                    return true;
                }
            }
            EVENT_DESTROY_NOTIFY => {
                // XDestroyWindowEvent: the destroyed window is at byte 32.
                if ev[4] == win {
                    return true;
                }
            }
            EVENT_CONFIGURE_NOTIFY | EVENT_REPARENT_NOTIFY => {
                // Container resizes and plugin window churn: the child
                // count and rect refresh in the caller track them.
            }
            _ => {}
        }
    }
    false
}

/// Send a `WM_DELETE_WINDOW` client message to an X window from a fresh
/// connection — exactly what a window manager does when the user clicks
/// the close button. Used by the editor close-button regression pins.
pub fn request_wm_delete(xid: u64) -> bool {
    unsafe {
        let d = XOpenDisplay(ptr::null());
        if d.is_null() {
            return false;
        }
        XSetErrorHandler(Some(x_error_noop));
        let wm_delete = XInternAtom(d, c"WM_DELETE_WINDOW".as_ptr(), 0);
        let wm_protocols = XInternAtom(d, c"WM_PROTOCOLS".as_ptr(), 0);
        // XClientMessageEvent as u64 words (x86_64): [0] type (ClientMessage
        // = 33, int), [2] send_event, [4] window, [5] message_type,
        // [6] format = 32, [7] data.l[0] = the protocol atom.
        let mut ev = [0u64; 24];
        ev[0] = EVENT_CLIENT_MESSAGE as u64;
        ev[2] = 1;
        ev[4] = xid;
        ev[5] = wm_protocols;
        ev[6] = 32;
        ev[7] = wm_delete;
        XSendEvent(d, xid, 0, 0, ev.as_ptr());
        XFlush(d);
        XCloseDisplay(d);
        true
    }
}

/// True when a corner probe of `win` holds any pixel that is neither the
/// sentinel background nor the all-zero "nothing painted yet" state —
/// proof the plugin painted DIRECTLY on the container (VSTGUI-style
/// editors never create a child window, so child polling alone
/// false-negatives them).
unsafe fn container_painted(d: *mut Display, win: XWindow) -> bool {
    const ALL_PLANES: u64 = !0u64;
    const Z_PIXMAP: c_int = 2;
    let img = XGetImage(d, win, 0, 0, 24, 24, ALL_PLANES, Z_PIXMAP);
    if img.is_null() {
        return false;
    }
    let mut hit = false;
    for y in 0..24 {
        for x in 0..24 {
            let p = XGetPixel(img, x, y);
            if p != CONTAINER_BG && p != 0 {
                hit = true;
                break;
            }
        }
        if hit {
            break;
        }
    }
    XDestroyImage(img);
    hit
}

/// Query the plugin's preferred editor size via `effEditGetRect`.
pub fn edit_rect(handle: &Vst2Handle) -> Option<(i32, i32)> {
    let mut rect: *mut ERect = ptr::null_mut();
    handle.dispatch(
        EFF_EDIT_GET_RECT,
        0,
        0,
        &mut rect as *mut *mut ERect as *mut c_void,
        0.0,
    );
    if rect.is_null() {
        return None;
    }
    let r = unsafe { *rect };
    let w = (r.right as i32 - r.left as i32).max(0);
    let h = (r.bottom as i32 - r.top as i32).max(0);
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

/// Shared state between an `EditorWindow` handle and its `Editor` running
/// on the VST GUI thread.
#[derive(Clone)]
struct Shared {
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    open_failed: Arc<AtomicBool>,
    painted: Arc<AtomicBool>,
    children: Arc<AtomicUsize>,
    width: Arc<AtomicI32>,
    height: Arc<AtomicI32>,
    xid: Arc<AtomicUsize>,
}

impl Shared {
    fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
            open_failed: Arc::new(AtomicBool::new(false)),
            painted: Arc::new(AtomicBool::new(false)),
            children: Arc::new(AtomicUsize::new(0)),
            width: Arc::new(AtomicI32::new(0)),
            height: Arc::new(AtomicI32::new(0)),
            xid: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// A hosted VST2 editor. Every editor operation (open, idle pump, resize,
/// close) runs on the ONE shared VST GUI thread (see `vsthost::open`) —
/// the same thread that instantiated the plugin, because GUI toolkits
/// bind their event loop to that thread and nowhere else.
pub struct EditorWindow {
    path: String,
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    open_failed: Arc<AtomicBool>,
    /// The plugin's editor window appeared and was sized — real pixels.
    painted: Arc<AtomicBool>,
    children: Arc<AtomicUsize>,
    width: Arc<AtomicI32>,
    height: Arc<AtomicI32>,
    /// The host container's XID (0 until the GUI thread creates it).
    xid: Arc<AtomicUsize>,
}

impl EditorWindow {
    /// Plugin path this editor was opened for (stale-editor detection).
    pub fn path(&self) -> &str {
        &self.path
    }
    /// True once the editor has retired (user closed the window, the
    /// plugin refused to open, or [`Self::close`] completed).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
    /// The plugin accepted `effEditOpen` but its editor returned failure
    /// (or it created no window at all).
    pub fn open_failed(&self) -> bool {
        self.open_failed.load(Ordering::SeqCst)
    }
    /// True once the plugin's own editor window has been seen mapped with
    /// real geometry (the container is showing live plugin UI, not a
    /// blank panel). False while waiting, or forever on broken editors.
    pub fn painted(&self) -> bool {
        self.painted.load(Ordering::SeqCst)
    }
    /// Windows the plugin has parented into our container. Zero a while
    /// after opening means the plugin did not create an editor.
    pub fn child_count(&self) -> usize {
        self.children.load(Ordering::SeqCst)
    }
    /// Editor size as last reported by the plugin (`effEditGetRect`).
    pub fn editor_size(&self) -> (i32, i32) {
        (
            self.width.load(Ordering::SeqCst),
            self.height.load(Ordering::SeqCst),
        )
    }
    /// The host container window's XID, or 0 before the GUI thread has
    /// created it (integration tests / WM-protocol close injection).
    pub fn xid(&self) -> u64 {
        self.xid.load(Ordering::SeqCst) as u64
    }
    /// Retire the editor: ask the GUI thread to close it and wait (with a
    /// bound) for the `effEditClose` + window teardown to finish, so the
    /// plugin instance may be dropped safely afterwards. Idempotent.
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for _ in 0..500 {
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Only reachable if the GUI thread is wedged inside plugin code;
        // report rather than hang the host forever.
        eprintln!("[vstedit] {} close did not settle in 5 s", self.path);
    }
}

impl Drop for EditorWindow {
    fn drop(&mut self) {
        // Wait so `effEditClose` finishes before the plugin instance may
        // be dropped by the caller.
        self.close();
    }
}

/// Open a VST2 plugin's editor in a new X11 window. `ae` is the raw
/// `AEffect` dispatcher handle from `vsthost`; the GUI thread keeps
/// using it, so the plugin instance must outlive the returned handle
/// (slooper closes editors before unloading, see `ui.rs`).
pub fn open_editor(handle: &Vst2Handle, path: &str, title: &str) -> Result<EditorWindow, String> {
    if !x11_available() {
        return Err("no X11 display (VST2 editors need X11/XWayland)".into());
    }
    let shared = Shared::new();
    let ae = *handle;
    let title = title.to_string();
    let s2 = shared.clone();
    // Creation (container window + effEditOpen) happens ON the GUI
    // thread; blocking here means the returned handle already has an
    // XID and either a live verification state machine or a verdict.
    crate::vsthost::on_gui_thread(move || {
        REG.with(|reg| {
            // SAFETY: runs on the GUI thread, the sole owner of all
            // editor state (see module docs).
            if let Some(e) = unsafe { make_editor(ae, &title, s2) } {
                reg.borrow_mut().push(e);
            }
        });
    })?;
    Ok(EditorWindow {
        path: path.to_string(),
        stop: shared.stop.clone(),
        closed: shared.closed.clone(),
        open_failed: shared.open_failed.clone(),
        painted: shared.painted.clone(),
        children: shared.children.clone(),
        width: shared.width.clone(),
        height: shared.height.clone(),
        xid: shared.xid.clone(),
    })
}

// ---------------------------------------------------------------- registry
//
// The live editors, owned exclusively by the VST GUI thread; every
// `pump_editors` call advances their state machines once (the shared
// 30 ms cadence the toolkits expect from `effEditIdle`).

thread_local! {
    static REG: std::cell::RefCell<Vec<Editor>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// One step for every open editor; retires finished ones. Called by the
/// VST GUI thread between jobs — never from another thread.
pub fn pump_editors() {
    REG.with(|reg| {
        let mut eds = reg.borrow_mut();
        for e in eds.iter_mut() {
            if e.shared.stop.load(Ordering::SeqCst) {
                if !matches!(e.phase, Phase::Done) {
                    e.teardown(false);
                }
                continue;
            }
            e.step();
        }
        eds.retain(|e| !matches!(e.phase, Phase::Done));
    });
}

enum Phase {
    /// Waiting for the plugin to prove it painted — the per-frame form
    /// of what used to be `editor_main`'s verification loop.
    Verify { it: u32 },
    Live {
        last_size: (i32, i32),
        since_rect: u32,
        dead_streak: u32,
    },
    Done,
}

struct Editor {
    handle: Vst2Handle,
    d: *mut Display,
    win: XWindow,
    title: String,
    wm_protocols: Atom,
    wm_delete: Atom,
    phase: Phase,
    shared: Shared,
    dbg: bool,
}

/// Create the container, map it, and dispatch `effEditOpen` — all on the
/// GUI thread. `None` (with the failure flags already set) when X or the
/// plugin refuses outright.
unsafe fn make_editor(handle: Vst2Handle, title: &str, shared: Shared) -> Option<Editor> {
    // NO XInitThreads: hosts that host plugin UIs on a dedicated thread
    // (Carla, Y/Plugins) deliberately leave libX11 unthreaded — JUCE's
    // X initialization inside plugins breaks with it (verified here:
    // editors never map the window while XInitThreads has been called).
    // Our plugin-side Xlib use is strictly the GUI thread; the UI thread
    // only opens short-lived independent connections (WM-delete probes).
    let d = XOpenDisplay(ptr::null());
    if d.is_null() {
        shared.open_failed.store(true, Ordering::SeqCst);
        shared.closed.store(true, Ordering::SeqCst);
        return None;
    }
    let root = root_window(d);
    // (bisect) fixed seed size
    let (w0, h0) = (640i32, 420i32);
    // Border width 0 and a SENTINEL background/border (see
    // CONTAINER_BG): the pixel probe must be able to tell "plugin
    // drew here" apart from "empty window furniture".
    let win = XCreateSimpleWindow(d, root, 0, 0, w0, h0, 0, CONTAINER_BG, CONTAINER_BG);
    // The container is mapped BARE — no window name, no WM protocols,
    // no event selection — and that is load-bearing: any of those
    // requests issued before `effEditOpen` change how the window manager
    // engages the window in time, and plugin editors (JUCE verified,
    // FLTK consistent) then create and size their child but never map
    // it, leaving the sentinel panel forever (the v10.2 solid-blue
    // report). Decoration lands after the editor proves itself live,
    // in `Editor::decorate`.
    shared.xid.store(win as usize, Ordering::SeqCst);
    let wm_delete = XInternAtom(d, c"WM_DELETE_WINDOW".as_ptr(), 0);
    let wm_protocols = XInternAtom(d, c"WM_PROTOCOLS".as_ptr(), 0);
    XMapWindow(d, win);
    XSync(d, 0);
    let dbg = std::env::var_os("SLOOPER_VSTEDIT_DEBUG").is_some();
    let opened = handle.dispatch(EFF_EDIT_OPEN, 0, 0, win as usize as *mut c_void, 0.0);
    if dbg {
        eprintln!("[vstedit] {title}: effEditOpen -> {opened} (win {win:#x}, gui thread)");
    }
    // The VST2 spec says 0 == success, but plenty of toolkits return a
    // "true" flag (1) for a SUCCESSFUL open — the verification below is
    // the real judge: if the plugin draws, it opened, whatever it said.
    Some(Editor {
        handle,
        d,
        win,
        title: title.to_string(),
        wm_protocols,
        wm_delete,
        phase: Phase::Verify { it: 0 },
        shared,
        dbg,
    })
}

/// `edit_rect` under the loader lock (every GUI-thread AEffect call
/// serializes with instantiation through it).
fn edit_rect_locked(handle: &Vst2Handle) -> Option<(i32, i32)> {
    let _loader = crate::vsthost::vst_loader_lock();
    edit_rect(handle)
}

impl Editor {
    /// One pump iteration: X events, idle dispatch, state advance.
    fn step(&mut self) {
        unsafe {
            match self.phase {
                Phase::Done => {}
                Phase::Verify { it } => {
                    // The user can hit the close button while we are
                    // still verifying: honour it instead of holding a
                    // doomed window.
                    if drain_events(self.d, self.win, self.wm_protocols, self.wm_delete) {
                        self.teardown(false);
                        return;
                    }
                    self.handle.dispatch(EFF_EDIT_IDLE, 0, 0, ptr::null_mut(), 0.0);
                    // A child counts as a live editor only when MAPPED
                    // with real geometry: created-and-sized-but-unmapped
                    // children are what a mid-build crash leaves behind,
                    // and accepting one kept the sentinel-blue container
                    // on screen forever (v10.2 report). Direct-draw
                    // editors (VSTGUI et al.) paint ON the container and
                    // never create a child, so check real pixels too.
                    let child_live = first_sized_child(self.d, self.win)
                        .is_some_and(|(k, w, h)| w >= 8 && h >= 8 && is_viewable(self.d, k));
                    let px = container_painted(self.d, self.win);
                    if self.dbg && (it < 3 || it % 10 == 0) {
                        let (kid, w, h) =
                            first_sized_child(self.d, self.win).unwrap_or((0, 0, 0));
                        eprintln!(
                            "[vstedit] verify {it}: child=({kid:x} {w}x{h}) px={px} live={child_live}"
                        );
                    }
                    if child_live || px {
                        self.shared.painted.store(true, Ordering::SeqCst);
                        self.decorate();
                        self.phase = Phase::Live {
                            last_size: (0, 0),
                            since_rect: 0,
                            dead_streak: 0,
                        };
                        return;
                    }
                    // 100 steps at the 30 ms cadence ~ 3 s of patience.
                    if it >= 100 {
                        // No live editor appeared: close cleanly so NO
                        // blank panel is left behind; the UI falls back
                        // to the slider panel.
                        self.teardown(true);
                    } else {
                        self.phase = Phase::Verify { it: it + 1 };
                    }
                }
                Phase::Live {
                    ref mut last_size,
                    ref mut since_rect,
                    ref mut dead_streak,
                } => {
                    // Dispatch X events (close button / window
                    // destruction); bounded per step.
                    if drain_events(self.d, self.win, self.wm_protocols, self.wm_delete) {
                        self.teardown(false);
                        return;
                    }
                    let kids = count_children_local(self.d, self.win);
                    self.shared.children.store(kids, Ordering::SeqCst);
                    // Dead-editor watchdog: nothing viewable for ~4 s
                    // straight means the editor crashed or tore itself
                    // down — close cleanly instead of keeping a dead
                    // sentinel panel around.
                    let alive = first_sized_child(self.d, self.win)
                        .is_some_and(|(k, w, h)| w >= 8 && h >= 8 && is_viewable(self.d, k))
                        || container_painted(self.d, self.win);
                    *dead_streak = if alive { 0 } else { *dead_streak + 1 };
                    if *dead_streak > 120 {
                        self.teardown(true);
                        return;
                    }
                    // Periodic rect + size handling; the plugin owns its size.
                    *since_rect += 1;
                    if *since_rect >= 16 {
                        *since_rect = 0;
                        if let Some((w, h)) = edit_rect_locked(&self.handle) {
                            self.shared.width.store(w, Ordering::SeqCst);
                            self.shared.height.store(h, Ordering::SeqCst);
                            if (w, h) != *last_size && w > 0 && h > 0 {
                                *last_size = (w, h);
                                XResizeWindow(self.d, self.win, w, h);
                            }
                        }
                    }
                    // The plugin's editor event loop runs here.
                    self.handle.dispatch(EFF_EDIT_IDLE, 0, 0, ptr::null_mut(), 0.0);
                }
            }
        }
    }

    /// Name, WM close protocol, and destroy-notification — added only
    /// once the editor has proven itself (see the bare-container note in
    /// `make_editor`: issuing these before `effEditOpen` breaks plugin
    /// window mapping).
    unsafe fn decorate(&mut self) {
        XSelectInput(self.d, self.win, STRUCTURE_NOTIFY_MASK);
        if let Ok(cname) = CString::new(format!("slooper \u{2014} {}", self.title)) {
            XStoreName(self.d, self.win, cname.as_ptr());
        }
        let mut protocols = [self.wm_delete];
        XSetWMProtocols(self.d, self.win, protocols.as_mut_ptr(), 1);
        XFlush(self.d);
    }

    /// `effEditClose`, destroy the container, release the display, mark
    /// retired. `refused` reports the death as an open failure so the UI
    /// drops to (or keeps) the slider panel.
    fn teardown(&mut self, refused: bool) {
        unsafe {
            if !self.d.is_null() {
                {
                    let _loader = crate::vsthost::vst_loader_lock();
                    self.handle.dispatch(EFF_EDIT_CLOSE, 0, 0, ptr::null_mut(), 0.0);
                }
                XDestroySubwindows(self.d, self.win);
                XDestroyWindow(self.d, self.win);
                XFlush(self.d);
                XCloseDisplay(self.d);
                self.d = ptr::null_mut();
            }
            self.shared.open_failed.store(refused, Ordering::SeqCst);
            self.shared.closed.store(true, Ordering::SeqCst);
            self.phase = Phase::Done;
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        // Every retire path runs teardown() before removal from REG;
        // this is the belt for any path that forgets — never leak the
        // display, and never leave the container alive without an owner.
        if !matches!(self.phase, Phase::Done) {
            self.teardown(true);
        }
    }
}

/// Whether an X display is configured for the session. Env-only by
/// design: opening a probe display here would drag the UI thread into
/// libX11 alongside the GUI thread's live connections.
pub fn x11_available() -> bool {
    std::env::var_os("DISPLAY").is_some()
}

/// Read the child-window count of an XID from a fresh connection
/// (integration tests; `0` on any failure).
pub fn count_children(xid: u64) -> usize {
    unsafe {
        let d = XOpenDisplay(ptr::null());
        if d.is_null() {
            return 0;
        }
        let mut root: XWindow = 0;
        let mut parent: XWindow = 0;
        let mut kids: *mut XWindow = ptr::null_mut();
        let mut n: c_int = 0;
        let ok = XQueryTree(d, xid, &mut root, &mut parent, &mut kids, &mut n);
        let count = if ok != 0 { n.max(0) as usize } else { 0 };
        if !kids.is_null() {
            XFree(kids as *mut c_void);
        }
        XCloseDisplay(d);
        count
    }
}

/// Child count on the thread's own connection (XQueryTree needs no round
/// trip through a new connection here).
unsafe fn count_children_local(d: *mut Display, win: XWindow) -> usize {
    let mut root: XWindow = 0;
    let mut parent: XWindow = 0;
    let mut kids: *mut XWindow = ptr::null_mut();
    let mut n: c_int = 0;
    let ok = XQueryTree(d, win, &mut root, &mut parent, &mut kids, &mut n);
    let count = if ok != 0 { n.max(0) as usize } else { 0 };
    if !kids.is_null() {
        XFree(kids as *mut c_void);
    }
    count
}

/// First plugin child window with non-degenerate geometry (created AND
/// resized — an editor that created a window but crashed mid-build leaves
/// it 1x1, the tell-tale of the "blank editor window" bug).
unsafe fn first_sized_child(d: *mut Display, win: XWindow) -> Option<(XWindow, u32, u32)> {
    let mut root: XWindow = 0;
    let mut parent: XWindow = 0;
    let mut kids: *mut XWindow = ptr::null_mut();
    let mut n: c_int = 0;
    if XQueryTree(d, win, &mut root, &mut parent, &mut kids, &mut n) == 0 {
        return None;
    }
    let list = if kids.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(kids, n.max(0) as usize).to_vec()
    };
    if !kids.is_null() {
        XFree(kids as *mut c_void);
    }
    for k in &list {
        let mut rr: XWindow = 0;
        let (mut x, mut y, mut w, mut h, mut b, mut depth) = (0, 0, 0u32, 0u32, 0u32, 0u32);
        if XGetGeometry(d, *k, &mut rr, &mut x, &mut y, &mut w, &mut h, &mut b, &mut depth) != 0
            && w > 0
            && h > 0
        {
            return Some((*k, w, h));
        }
    }
    list.first().copied().map(|k| (k, 0, 0))
}

// `XRootWindow`/`XDefaultScreen` are legacy-but-exported libX11 functions
// (verified against the system libX11 symbol table); `DefaultRootWindow`
// itself is an Xlib macro, so go through the functions instead.
extern "C" {
    fn XRootWindow(d: *mut Display, screen: c_int) -> XWindow;
    fn XDefaultScreen(d: *mut Display) -> c_int;
}
unsafe fn root_window(d: *mut Display) -> XWindow {
    XRootWindow(d, XDefaultScreen(d))
}

// ---------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;

    /// Real-plugin editor smoke: loads amsynth (or any VST2 present),
    /// opens its editor in a live X11 window, pumps idle, and verifies the
    /// plugin actually parented a window into our container. Gated on
    /// `SLOOPER_VST_SMOKE=1` + an installed plugin + DISPLAY, so CI skips
    /// it. AC4.2 of REQUIREMENTS.md.
    #[test]
    fn vst2_editor_smoke_if_present() {
        if std::env::var("SLOOPER_VST_SMOKE").unwrap_or_default() != "1" {
            eprintln!("skip: set SLOOPER_VST_SMOKE=1 to run");
            return;
        }
        if !x11_available() {
            eprintln!("skip: no X11 display");
            return;
        }
        let cand = [
            dirs_home().join(".vst/amsynth_vst.so"),
            dirs_home().join(".vst/wolf-shaper-vst.so"),
        ];
        let Some(path) = cand.iter().find(|p| p.exists()).cloned() else {
            eprintln!("skip: no known VST2 plugin installed");
            return;
        };
        let load = crate::vsthost::open(&path, crate::vsthost::VstKind::V2, 48_000, None)
            .expect("plugin loads");
        let entry = &load.entry;
        assert!(entry.has_editor(), "{} advertises an editor", path.display());
        let h = entry.vst2_handle().expect("vst2 handle");
        let mut win =
            match open_editor(&h, &path.to_string_lossy(), "smoke") {
                Ok(w) => w,
                Err(e) => panic!("open_editor failed: {e}"),
            };
        // Give the editor thread time to embed (verify) + pump.
        std::thread::sleep(Duration::from_millis(4200));
        // The invariant slooper guarantees: the window shows a LIVE
        // plugin editor, or it is gone with an honest failure — never a
        // stuck blank panel (the historical "uncloseable blank window"
        // complaint from a plugin that opened but never painted).
        if win.open_failed() {
            eprintln!(
                "note: {} refused/did not paint an editor (thread closed cleanly) \u{2014} \
                 UI will fall back to sliders",
                path.display()
            );
            assert!(win.is_closed(), "failed editor must close its window");
            assert_eq!(win.child_count(), 0, "failed editor must destroy children");
        } else {
            assert!(!win.is_closed(), "editor thread died while painted");
            assert!(win.painted(), "editor window shows no live plugin UI");
            assert!(win.editor_size().0 > 0, "no editor size negotiated");
        }
        win.close();
        assert!(win.is_closed());
    }

    fn dirs_home() -> std::path::PathBuf {
        std::env::var("HOME").map(std::path::PathBuf::from).unwrap_or_default()
    }

    /// Shared gate for the live-X pins: explicit opt-in + X11 + a plugin.
    fn smoke_gate(plugin: &str) -> Option<std::path::PathBuf> {
        if std::env::var("SLOOPER_VST_SMOKE").unwrap_or_default() != "1" || !x11_available() {
            return None;
        }
        let p = dirs_home().join(plugin);
        p.exists().then_some(p)
    }

    /// v10.1 pin (amsynth blank-panel report): a plugin whose editor dies
    /// mid-build — it throws `unordered_map::at` and its child window
    /// stays 1x1 — must be REFUSED: container destroyed, thread exited,
    /// `open_failed` set so the UI falls back to sliders. The historical
    /// bug: the paint probe compared against black, and the container's
    /// own background counted as "painted", leaving a permanent blank,
    /// uncloseable window.
    #[test]
    fn v101_vstedit_refuses_unpainted_editor() {
        let Some(path) = smoke_gate(".vst/amsynth_vst.so") else {
            eprintln!("skip: SLOOPER_VST_SMOKE=1 + ~/.vst/amsynth_vst.so required");
            return;
        };
        let load = crate::vsthost::open(&path, crate::vsthost::VstKind::V2, 48_000, None)
            .expect("plugin loads");
        let entry = &load.entry;
        let h = entry.vst2_handle().expect("vst2 handle");
        let mut win = open_editor(&h, &path.to_string_lossy(), "pin-refuse").expect("open_editor");
        // Verification is 100 x 30 ms (shared pump); slack ok.
        let mut waited = 0u32;
        while !win.open_failed() && !win.painted() && waited < 8000 {
            std::thread::sleep(Duration::from_millis(100));
            waited += 100;
        }
        if win.painted() {
            eprintln!(
                "note: {} paints a live editor on this build; pin passes trivially",
                path.display()
            );
            win.close();
            return;
        }
        assert!(win.open_failed(), "unpainted editor must report open_failed");
        assert!(win.is_closed(), "refused editor must destroy its window and exit");
        assert_eq!(win.child_count(), 0, "no children may survive the refuse");
        win.close();
    }

    /// v10.1 pin (uncloseable-window report): a `WM_DELETE_WINDOW` client
    /// message — exactly what the window manager sends for the title-bar
    /// X — must close a painted editor within seconds. The old pump tested
    /// `XNextEvent(...) == 0` as an error, but 0 IS success, so every
    /// event was popped and dropped and the close button did nothing.
    #[test]
    fn v101_vstedit_wm_delete_closes_editor() {
        let Some(path) = smoke_gate(".vst/wolf-shaper-vst.so") else {
            eprintln!("skip: SLOOPER_VST_SMOKE=1 + ~/.vst/wolf-shaper-vst.so required");
            return;
        };
        let load = crate::vsthost::open(&path, crate::vsthost::VstKind::V2, 48_000, None)
            .expect("plugin loads");
        let entry = &load.entry;
        let h = entry.vst2_handle().expect("vst2 handle");
        let mut win = open_editor(&h, &path.to_string_lossy(), "pin-close").expect("open_editor");
        let mut waited = 0u32;
        while !win.painted() && !win.is_closed() && waited < 8000 {
            std::thread::sleep(Duration::from_millis(100));
            waited += 100;
        }
        if !win.painted() {
            eprintln!("skip: wolf-shaper editor did not paint in 8 s (no X session?)");
            win.close();
            return;
        }
        let xid = win.xid();
        assert_ne!(xid, 0, "container XID must be published");
        assert!(request_wm_delete(xid), "WM_DELETE injection failed");
        let mut waited = 0u32;
        while !win.is_closed() && waited < 4000 {
            std::thread::sleep(Duration::from_millis(100));
            waited += 100;
        }
        assert!(
            win.is_closed(),
            "editor must exit on WM_DELETE_WINDOW (close button)"
        );
        win.close();
    }

    /// v10.2 pin: `is_viewable` must distinguish a MAPPED window from a
    /// merely-created one, and the opaque `XWindowAttributes` field
    /// offsets (width@8, map_state@92) must match the live libX11 struct.
    /// A created-but-unmapped window is exactly what the amsynth crash
    /// leaves behind, and the old geometry-only probe accepted it.
    #[test]
    fn v102_xattr_map_state_pin() {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("skip: no X11 display");
            return;
        }
        unsafe {
            let d = XOpenDisplay(ptr::null());
            assert!(!d.is_null());
            XSetErrorHandler(Some(x_error_noop));
            let root = root_window(d);
            let mapped = XCreateSimpleWindow(d, root, 0, 0, 64, 48, 0, 0, 0);
            let unmapped = XCreateSimpleWindow(d, root, 0, 0, 64, 48, 0, 0, 0);
            XMapWindow(d, mapped);
            XSync(d, 0);
            // Map is asynchronous: poll briefly.
            let mut is_m = false;
            for _ in 0..50 {
                is_m = is_viewable(d, mapped);
                if is_m {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(is_m, "mapped window must read viewable (offsets pinned)");
            assert!(!is_viewable(d, unmapped), "unmapped window must NOT read viewable");
            // Garbage XID must fail closed, not crash.
            assert!(!is_viewable(d, 0x0F00_0000_0000_0001));
            XDestroyWindow(d, mapped);
            XDestroyWindow(d, unmapped);
            XFlush(d);
            XCloseDisplay(d);
        }
    }

    /// v10.2 ACCEPTANCE pin (the "VST plugin GUIs don't work" report):
    /// every installed VST2 with an editor must reach PAINTED through the
    /// public API — proof that the shared GUI-thread model (load, open,
    /// idle all on one thread) actually shows plugin UI. This used to be
    /// impossible: editors opened on their own thread stayed unmapped
    /// forever because JUCE/FLTK had bound their event loops elsewhere.
    /// Fails (does not skip) when a present plugin never paints.
    #[test]
    fn v102_every_vst2_editor_paints() {
        if std::env::var("SLOOPER_VST_SMOKE").unwrap_or_default() != "1" {
            eprintln!("skip: SLOOPER_VST_SMOKE=1 required");
            return;
        }
        if !x11_available() {
            eprintln!("skip: no X11 display");
            return;
        }
        let dir = dirs_home().join(".vst");
        let Ok(rd) = std::fs::read_dir(&dir) else {
            eprintln!("skip: no ~/.vst");
            return;
        };
        let mut files: Vec<_> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "so"))
            .collect();
        files.sort();
        let mut opened = 0usize;
        let mut fails = Vec::new();
        for path in files {
            let Ok(load) = crate::vsthost::open(&path, crate::vsthost::VstKind::V2, 48_000, None)
            else {
                continue; // broken plugin: not this pin's problem
            };
            let entry = &load.entry;
            if !entry.has_editor() {
                continue;
            }
            let h = entry.vst2_handle().expect("vst2 handle");
            let mut win =
                open_editor(&h, &path.to_string_lossy(), "pin-paint").expect("open_editor");
            let mut waited = 0u32;
            while !win.painted() && !win.is_closed() && waited < 10_000 {
                std::thread::sleep(Duration::from_millis(100));
                waited += 100;
            }
            if !win.painted() {
                fails.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
            win.close();
            opened += 1;
        }
        assert_eq!(fails, Vec::<String>::new(), "editors that never painted");
        assert!(opened >= 3, "expected the installed VST2 editors, got {opened}");
        eprintln!("[v102] {opened} VST2 editors painted through the shared GUI thread");
    }
}
