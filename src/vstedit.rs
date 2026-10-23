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
//! Every editor dispatch runs on ONE dedicated GUI thread per editor (Xlib
//! is not thread-safe, and plugin UIs bind to the thread that opened them)
//! while the audio thread keeps processing — exactly how desktop Linux VST
//! hosts do it.
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
    fn XInitThreads();
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
    fn XSetErrorHandler(
        handler: Option<extern "C" fn(*mut c_void, *mut c_void) -> c_int>,
    ) -> Option<extern "C" fn(*mut c_void, *mut c_void) -> c_int>;
}

extern "C" fn x_error_noop(_display: *mut c_void, _event: *mut c_void) -> c_int {
    // X races (a plugin window vanishing mid-query) must never kill the
    // host; standard practice for plugin wrappers.
    0
}

const STRUCTURE_NOTIFY_MASK: u64 = 1 << 20;
const EVENT_DESTROY_NOTIFY: i32 = 17;
const EVENT_REPARENT_NOTIFY: i32 = 18;
const EVENT_CONFIGURE_NOTIFY: i32 = 22;
const EVENT_CLIENT_MESSAGE: i32 = 33;

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

/// True when a corner probe of `win` holds any non-black pixel — proof the
/// plugin painted DIRECTLY on the container (VSTGUI-style editors never
/// create a child window, so child polling alone false-negatives them).
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
            if XGetPixel(img, x, y) != 0 {
                hit = true;
                break;
            }
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

/// A hosted VST2 editor. The editor thread opens the plugin UI in a new
/// top-level X11 window, pumps `effEditIdle`, tracks the plugin's size,
/// and dispatches `effEditClose` when the window is closed or the handle
/// is dropped.
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
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EditorWindow {
    /// Plugin path this editor was opened for (stale-editor detection).
    pub fn path(&self) -> &str {
        &self.path
    }
    /// True once the editor thread exited (user closed the window, the
    /// plugin refused to open, or [`Self::close`] was called).
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
    /// Close the editor and join its thread. Idempotent.
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for EditorWindow {
    fn drop(&mut self) {
        // Join so `effEditClose` finishes before the plugin instance may
        // be dropped by the caller.
        self.close();
    }
}

/// Open a VST2 plugin's editor in a new X11 window. `ae` is the raw
/// `AEffect` dispatcher handle from `vsthost`; the editor thread keeps
/// using it, so the plugin instance must outlive the returned handle
/// (slooper closes editors before unloading, see `ui.rs`).
pub fn open_editor(handle: &Vst2Handle, path: &str, title: &str) -> Result<EditorWindow, String> {
    if !x11_available() {
        return Err("no X11 display (VST2 editors need X11/XWayland)".into());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    let open_failed = Arc::new(AtomicBool::new(false));
    let painted = Arc::new(AtomicBool::new(false));
    let children = Arc::new(AtomicUsize::new(0));
    let width = Arc::new(AtomicI32::new(0));
    let height = Arc::new(AtomicI32::new(0));
    // Seed the container size from a pre-open rect query when offered.
    let initial = edit_rect(handle).unwrap_or((640, 420));
    let ae = *handle;
    let title = title.to_string();
    let (t_stop, t_closed, t_failed, t_painted, t_children, t_w, t_h) = (
        stop.clone(),
        closed.clone(),
        open_failed.clone(),
        painted.clone(),
        children.clone(),
        width.clone(),
        height.clone(),
    );
    // Vst2Handle is explicitly Send: just the AEffect address.
    let thread = std::thread::Builder::new()
        .name(format!("vst-editor {title}"))
        .spawn(move || {
            editor_main(
                ae, &title, initial, t_stop, t_closed, t_failed, t_painted, t_children, t_w,
                t_h,
            );
        })
        .map_err(|e| format!("spawn editor thread: {e}"))?;
    Ok(EditorWindow {
        path: path.to_string(),
        stop,
        closed,
        open_failed,
        painted,
        children,
        width,
        height,
        thread: Some(thread),
    })
}

/// Xlib is only thread-safe after `XInitThreads`; the editor threads and
/// the UI thread's liveness probes open/close displays concurrently.
/// Every entry point that touches Xlib funnels through this first.
fn xlib_threads_ready() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        XInitThreads();
    });
}

/// Whether an X server (or XWayland) is reachable.
pub fn x11_available() -> bool {
    if std::env::var_os("DISPLAY").is_none() {
        return false;
    }
    xlib_threads_ready();
    unsafe {
        let d = XOpenDisplay(ptr::null());
        if d.is_null() {
            false
        } else {
            XCloseDisplay(d);
            true
        }
    }
}

/// Read the child-window count of an XID from a fresh connection
/// (integration tests; `0` on any failure).
pub fn count_children(xid: u64) -> usize {
    xlib_threads_ready();
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

/// The editor thread body. Owns its X connection for its whole life.
#[allow(clippy::too_many_arguments)]
fn editor_main(
    handle: Vst2Handle,
    title: &str,
    initial: (i32, i32),
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    open_failed: Arc<AtomicBool>,
    painted: Arc<AtomicBool>,
    children: Arc<AtomicUsize>,
    width: Arc<AtomicI32>,
    height: Arc<AtomicI32>,
) {
    unsafe {
        if std::env::var_os("SLOOPER_VSTEDIT_DEBUG").is_some() {
            eprintln!("[vstedit] {title}: editor thread alive");
        }
        xlib_threads_ready();
        XSetErrorHandler(Some(x_error_noop));
        let d = XOpenDisplay(ptr::null());
        if d.is_null() {
            open_failed.store(true, Ordering::SeqCst);
            closed.store(true, Ordering::SeqCst);
            return;
        }
        let root = root_window(d);
        let (w0, h0) = (initial.0.clamp(120, 16384), initial.1.clamp(60, 16384));
        let win = XCreateSimpleWindow(d, root, 0, 0, w0, h0, 1, 0x202020, 0x181818);
        XSelectInput(d, win, STRUCTURE_NOTIFY_MASK);
        let cname = CString::new(format!("slooper \\u2014 {title}"))
            .or_else(|_| CString::new(format!("slooper - {title}")))
            .unwrap();
        XStoreName(d, win, cname.as_ptr());
        let wm_delete = XInternAtom(d, c"WM_DELETE_WINDOW".as_ptr(), 0);
        let mut protocols = [wm_delete];
        XSetWMProtocols(d, win, protocols.as_mut_ptr(), 1);
        XMapWindow(d, win);
        XSync(d, 0);
        let dbg = std::env::var_os("SLOOPER_VSTEDIT_DEBUG").is_some();

        // effEditOpen: the XID travels as a pointer-sized value.
        if dbg {
            eprintln!("[vstedit] {title}: pre-effEditOpen (win mapped, ready)");
        }
        // Instantiation/editor-bootstrap serialization (see vsthost).
        let _loader = crate::vsthost::vst_loader_lock();
        let opened = handle.dispatch(
            EFF_EDIT_OPEN,
            0,
            0,
            win as usize as *mut c_void,
            0.0,
        );
        drop(_loader);
        if dbg {
            eprintln!("[vstedit] {title}: effEditOpen -> {opened} (win {win:#x})");
        }
        // The VST2 spec says 0 == success, but plenty of toolkits return a
        // "true" flag (1) for a SUCCESSFUL open — and refusing on a
        // non-zero return silently killed editors that DO create their
        // window (Wolf Shaper, Dragonfly: they returned 1 and were then
        // torn down before their child ever mapped). Treat the return as
        // advisory; the verification loop below is the real judge: if the
        // plugin draws, it opened, whatever it said.

        // VERIFY THE EMBED. Some plugins accept effEditOpen yet never
        // draw (broken toolkits, missing plugin data, XEmbed assumptions
        // a plain container cannot satisfy) — the user is left staring at
        // a blank panel that shows nothing and reacts to nothing. Pump
        // idle and poll the plugin's child windows: only once one shows
        // up with real geometry do we consider the editor alive.
        let mut painted_now = false;
        for _it in 0..60u32 {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            handle.dispatch(EFF_EDIT_IDLE, 0, 0, ptr::null_mut(), 0.0);
            if dbg && (_it % 10 == 0 || _it < 3) {
                let (kid, w, h) = first_sized_child(d, win).unwrap_or((0, 0, 0));
                eprintln!(
                    "[vstedit] {title}: idle {_it} child=({kid:x} {w}x{h}) px={}",
                    container_painted(d, win)
                );
            }
            if let Some((_kid, w, h)) = first_sized_child(d, win) {
                if w >= 8 && h >= 8 {
                    painted_now = true;
                    break;
                }
            }
            // Direct-draw editors (VSTGUI et al.) paint ON the container
            // and never create a child: check real pixels too.
            if container_painted(d, win) {
                painted_now = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !painted_now {
            // No live editor appeared: close cleanly so NO blank window is
            // left behind, and let the UI fall back to the slider panel.
            let _loader = crate::vsthost::vst_loader_lock();
            handle.dispatch(EFF_EDIT_CLOSE, 0, 0, ptr::null_mut(), 0.0);
            drop(_loader);
            XDestroySubwindows(d, win);
            XDestroyWindow(d, win);
            XFlush(d);
            XCloseDisplay(d);
            open_failed.store(true, Ordering::SeqCst);
            closed.store(true, Ordering::SeqCst);
            return;
        }
        painted.store(true, Ordering::SeqCst);
        let mut last_size = (0i32, 0i32);
        let mut since_rect = 0u32;
        let mut quit = false;
        while !quit {
            // Drain pending X events (bounded so a chatty plugin cannot
            // starve the idle pump).
            let mut drained = 0;
            while XPending(d) > 0 && drained < 64 {
                let mut ev = [0u64; 24];
                if XNextEvent(d, &mut ev) == 0 {
                    break;
                }
                drained += 1;
                let etype = (ev[0] as u32) as i32;
                match etype {
                    EVENT_CLIENT_MESSAGE => {
                        // XClientMessageEvent on x86_64: message_type atom
                        // (unsigned long) at byte 40 -> ev[5]; data.l[0]
                        // at byte 56 -> ev[7].
                        if ev[7] == wm_delete {
                            quit = true;
                        }
                    }
                    EVENT_CONFIGURE_NOTIFY
                    | EVENT_DESTROY_NOTIFY
                    | EVENT_REPARENT_NOTIFY => {
                        // Container resizes and plugin window churn: the
                        // child count and rect refresh below track them.
                    }
                    _ => {}
                }
            }
            if quit || stop.load(Ordering::SeqCst) {
                break;
            }
            // Count plugin windows parented into the container.
            let kids = count_children_local(d, win);
            children.store(kids, Ordering::SeqCst);
            // Periodic rect + size handling; the plugin owns its size.
            since_rect += 1;
            if since_rect >= 16 {
                since_rect = 0;
                if let Some((w, h)) = edit_rect(&handle) {
                    width.store(w, Ordering::SeqCst);
                    height.store(h, Ordering::SeqCst);
                    if (w, h) != last_size && w > 0 && h > 0 {
                        last_size = (w, h);
                        XResizeWindow(d, win, w, h);
                    }
                }
            }
            // The plugin's editor event loop runs here.
            handle.dispatch(EFF_EDIT_IDLE, 0, 0, ptr::null_mut(), 0.0);
            std::thread::sleep(Duration::from_millis(30));
        }
        {
            let _loader = crate::vsthost::vst_loader_lock();
            handle.dispatch(EFF_EDIT_CLOSE, 0, 0, ptr::null_mut(), 0.0);
        }
        XDestroyWindow(d, win);
        XFlush(d);
        XCloseDisplay(d);
        closed.store(true, Ordering::SeqCst);
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
}
