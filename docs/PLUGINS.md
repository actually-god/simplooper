# Plugins: synths and VST hosting

Slooper hosts two kinds of plugin: **scripted synth plugins** (plain Rust
files in `synths/`, compiled on first load into `~/.local/share/slooper/synths`)
and **binary VST2/VST3 plugins** loaded from `vst_dirs`.

## VST loading

- Scan folders: `vst_dirs` in the config (default `~/.vst`, `~/.vst3`,
  `/usr/lib/vst`, `/usr/lib/vst3`, `/usr/local/lib/vst*`).
- An instrument (a plugin with no audio inputs) can be bound to a track's
  synthesizer slot (`vst2:/path/to/plugin.so`, `vst3:/path/to/plugin.vst3`)
  and is played by that track's MIDI clips; its parameter sliders live in
  the track's synth window.
- VST2 plugins load through the vendored, host-patched `vendor/vst2` crate
  (see `VENDOR-NOTES.md`); VST3 through `vst3-host`.
- Instrument playback keeps the plugin PROCESSING through event-free
  blocks, so sustained notes, long pads and the plugin's own release tails
  sound fully — and StopAll/seek flush both the plugin's staging ring and
  its voices, so nothing leaks into the next run.
- distrho-ports / JUCE-wrapper builds (Vitalium, SwankyAmp, …) advertise
  their audio buses as inactive, which naive hosts honour into permanent
  silence; slooper detects the zero-channel case and activates the output
  buses itself, so these plugins play normally as track instruments.

## Installing plugins: just drop them in

No import step, no rescan button needed: put a plugin file in any scan
folder and it appears in the picker on the next scan (rescan happens on
window open and after every drop). Scan folders: `~/.vst`, `~/.vst3`,
`/usr/lib/vst(3)`, `/usr/local/lib/vst*`, `$VST_PATH`, plus anything in
`vst_dirs` — and Wine prefixes' `Plugin DLLs`/`VSTPlugins` folders too.
Understood layouts: loose `.so`, `.vst`/`.vst3` bundle directories, and
**Windows `.dll` plugins via yabridge** (next section).

### Windows-only VSTs (`.dll`) — automatic Wine bridge

If [yabridge](https://github.com/robbert-vdh/yabridge) is installed
(`yabridgectl` on `$PATH` or `~/.local/bin`), slooper registers every folder
that contains `.dll` plugins with `yabridgectl add` + `sync` automatically
during its scan — no terminal needed. yabridge then generates Linux
sidecars (`foo.dll.so`) that load the Windows plugin under Wine and also
bridge its **GUI** over X11, so Windows synths/effects appear and behave
exactly like native ones: sound, sliders, and their custom editor window.
(Install: AUR `yabridge`, or the release tarball + `./yabridgectl sync`.)

## VST custom GUIs (VST2) — open automatically

Binding a plugin instrument (track synth popup) or adding a plugin effect
to a chain **opens the plugin's own GUI window immediately** — that window
is now the standard editing surface for plugins; the parameter-slider
window only appears when the plugin has no editor or the editor refuses
(see below), and the track synth popup's "open plugin window" button just
reopens whatever surface the plugin uses. The editor runs in its **own OS
window** (slooper's main window is GPU-drawn by egui and has no
child-window support), titled `slooper — <plugin>`.

How it works (`src/vstedit.rs`) — the standard Linux VST2 hosting protocol,
cross-checked against the VST2.4 SDK (`aeff.h` opcodes), Ardour
(`linux_vst_gui_support.cc`), Carla, JUCE, LMMS and yabridge:

| Step | Opcode | Notes |
| --- | --- | --- |
| Open | `effEditOpen` (14) | `ptr` = the X11 **XID** of our container window cast to a pointer; the plugin creates/reparents its editor as a child. X object IDs are server-global, so the plugin may use its own X connection. |
| Pump | `effEditIdle` (19) | dispatched ~33×/s from one dedicated GUI thread per editor (FLTK/DPF/Steinberg-SDK editors run their event loops from it). The audio thread keeps processing meanwhile — the normal host split. |
| Size | `effEditGetRect` (13) | returns `ERect**` (16-bit `top/left/bottom/right`); the container is resized to the plugin's rect when it changes. VST2.4 resizable plugins instead call back `audioMasterSizeWindow` (15). |
| Close | `effEditClose` (15) | dispatched on the editor thread when the window is closed (button, window manager, or a plugin swap) **before** the plugin instance can be unloaded. |

Requirements: an X11 display (`$DISPLAY`; XWayland under a Wayland session
counts). Without X11 the button reports it and the slider fallback remains.

### Diagnostics / limitations

- **No-GUI plugins** (`effFlagsHasEditor` clear) keep the automatic
  parameter-slider panel — that panel is always available, GUI or not.
- **VST3 editors are not hosted yet.** Two blockers, both architectural:
  1. slooper runs VST3 plugins on `vst3-host`'s realtime runner, which
     confines the `IPlugin` to its processing thread, while VST3 editors
     (`IPlugView::attached`) must be created and serviced on the host's
     main thread with `IComponent`/`IEditController` access; the crate's
     `RtControl` exposes no editor bridge.
  2. `vst3-host`'s own `EmbeddedEditor` needs the host window's
     `RawWindowHandle` and **rejects Wayland handles** (VST3.8 requires an
     `IWaylandHost` compositor proxy, not the app surface), and slooper's
     main window is egui/winit — an X11 handle only exists under
     XWayland/X11.
  The upgrade path is a non-realtime editor thread owning a second
  `IPluginView` on a container window (mirroring our VST2 module), plus
  parameter-change synchronization via `IEditController::setComponentHandler`
  and `Plugin::service_host_requests`. Until then VST3 instruments load and
  play, but only expose sliders.
- The `effEditOpen` **return value is advisory**: the VST2 spec says 0
  means success, but many toolkits (VSTGUI-based plugins like Wolf Shaper
  or Dragonfly) return 1 as a "true" success flag. Trusting the number
  refused every working editor; slooper instead **verifies the embed**: the
  plugin must actually create its child window or paint pixels within a
  few seconds. Only when nothing appears is the open refused — reported in
  the status bar, window torn down cleanly, sliders take over.
- The embed probe is exact, never "non-black pixels": the container is
  painted a sentinel background colour and counts as showing live UI only
  when a probe pixel is neither that sentinel nor the all-zero state of a
  not-yet-exposed window. An earlier probe accepted the container's own
  grey background as "painted", which turned broken editors (e.g. amsynth
  2.0's VST2 GUI, whose editor build dies mid-construction and leaves a
  1×1 child window) into permanent blank panels instead of the slider
  fallback (v0.10.1).
- The title-bar **close button works**: the editor thread dispatches X
  events instead of discarding them (`XNextEvent` returns 0 on *success*;
  an early pump treated that as an error and dropped every
  `WM_DELETE_WINDOW`, so the window ignored the window manager's close),
  and it also exits when the container is destroyed outright. `cargo run
  --release --example editor_smoke [so]` opens a real editor, verifies it,
  injects the same `WM_DELETE_WINDOW` client message a close button sends,
  and asserts the editor exits.
- Known broken plugin GUIs fall back rather than hang: **amsynth 2.0's
  VST2** (`amsynth_vst.so`) throws `unordered_map::at` during
  instantiation/editor build in this environment — its editor window never
  appears, slooper refuses the open within ~3 s and its parameter sliders
  drive the plugin.
- Probe and live instances of the same plugin binary intentionally share
  one process-lifetime load (see `vst_loader_lock` / probe-leak comments):
  plugin toolkits carry process-global state that does not survive
  terminate/dlclose cycles inside a running host.
