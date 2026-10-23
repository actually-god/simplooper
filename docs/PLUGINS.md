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

## VSTi custom GUIs (VST2)

Opening a track-bound VST2 with a GUI: track synth window →
“open plugin window”. The editor runs in its **own OS window** (slooper's
main window is GPU-drawn by egui and has no child-window support), titled
`slooper — <plugin>`.

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
- A plugin that refuses `effEditOpen` (returns 0) is reported in the status
  bar and its slider panel keeps working.
