# AGENT-GUIDE — simplooper for AI agents & automation

Machine-readable orientation for agents asked to install, use, build, or
extend this repo. Humans: see [../README.md](../README.md),
[INSTALL.md](INSTALL.md), [USAGE.md](USAGE.md).
(This file follows the `AGENTS.md` convention — if your tooling looks for a
root `AGENTS.md`, this is it.)

## What this is

- **simplooper**: a minimalist live-audio looper for Linux — Rust, egui/eframe
  GUI, JACK audio (works against PipeWire's JACK shim or jack2).
- 16 loops × ~60 s, record/overdub/undo-redo, MIDI + keyboard binding,
  quantizer, metronome, per-loop runtime-compiled Rust effects, import/export.
- License: **MIT** ([../LICENSE](../LICENSE)). Upstream:
  `https://github.com/actually-god/simplooper` (branch `main`, releases are
  semver tags `v<X.Y.Z>`; version of record is `version` in `Cargo.toml`).

## Install (headless-safe summary)

```sh
# Arch: PKGBUILD lives in the AUR package repo, NOT here:
git clone https://aur.archlinux.org/simplooper.git && (cd simplooper && makepkg -si)

# Any distro: needs stable Rust, cargo, pkgconf, JACK headers (see INSTALL.md)
cargo build --release && ./target/release/simplooper
```

The app connects to JACK with `NO_START` — it never spawns a server; a running
PipeWire/jack2 is required, or it logs an error and idles. GUI needs a
display (X11 or Wayland).

## Repository layout

```
src/            application source (Rust 2021 edition)
  main.rs         entry point, window setup
  lib.rs          crate root: pub mods + loop-engine re-exports
  audio.rs        JACK backend + realtime LoopEngine (Cmd/Evt channels);
                  stereo mix buses with per-loop equal-power pan, per-output
                  L/R routing (Cmd::Apply outs = Vec<(name, bits)>)
  audio_io.rs     import (symphonia: wav/mp3/ogg/flac/m4a/aac/aiff…) +
                  export (WAV/FLAC native; MP3/OGG shell out to ffmpeg)
  config.rs       JSON persistence: $XDG_CONFIG_HOME/simplooper/config.json
  effect_api.rs   the effect-script ABI contract (also copied into every
                  user effects folder; scripts include! this file)
  fx.rs           effects scan/compile/cache/load, ABI validation, chains
  fxgui.rs        child editor windows (auto sliders or scripted GUIs)
  midi.rs         MIDI message parsing, MidiBind matching, MidiAction table
  ports.rs        JACK port enumeration helpers
  quantize.rs     grid snap + zero-crossing refinement + seam crossfade
  ui.rs           the whole egui app (top bar, settings, dashboard, windows)
effects/        effect-script authoring kit: template.rs (start here),
                effect_api.rs (ABI), parametric_eq.rs, space_echo.rs examples
examples/       small dev utilities run via `cargo run --example <name>`
tests/e2e.rs    end-to-end tests that drive a LIVE JACK server
tests/packaging.sh  packaging regression test (needs a PKGBUILD; see below)
assets/icon.png 256x256 app icon (placeholder artwork)
simplooper.desktop  freedesktop menu entry
docs/           INSTALL.md, USAGE.md (human docs)
```

## Build & test commands

| Command | Notes |
| --- | --- |
| `cargo build --release` | release profile: LTO + panic=abort; ~13 MB binary |
| `cargo test -- --test-threads=1` | full suite; e2e tests **self-skip** without a live JACK server |
| `cargo clippy --all-targets` | repo standard: zero warnings |
| `PKGBUILD_PATH=/path/to/aur/checkout/PKGBUILD ./tests/packaging.sh` | packaging checks (the PKGBUILD now lives in the AUR package repo) |

Test suite = ~50 unit tests (MIDI, quantize math, engine) + e2e tests that
record/play through real JACK clients in real time.
**Never remove the `E2E_LOCK` mutex serialization in `tests/e2e.rs`** —
concurrent e2e runs starve each other's JACK clients ("ensemble stalled").

## Runtime state (outside the repo)

- Config: `$XDG_CONFIG_HOME/simplooper/config.json` (fallback `~/.config/…`)
- Effects folder: `~/.local/share/simplooper/effects/` (configurable)

## Invariants an agent must not break

1. **Realtime safety**: the JACK `ProcessHandler` must stay lock-free and
   allocation-free; GUI↔audio only via bounded Cmd/Evt channels + atomics.
2. **`MidiAction::ALL` is position-persistent** — persisted bindings index it
   by slot. New actions may only be APPENDED, never inserted or reordered.
3. **Behavior specs live in tests and docs**: `README.md`, `IDEA.md`, and
   `tests/e2e.rs` pin intended behavior (e.g. additive overdub, latency =
   recording-start skip not input delay, dependent mode loops forever,
   metronome never recorded, loop selection clamps at 1/16, the loop-mode
   checkbox is the INDEPENDENT switch — ticked = independent, the engine
   `dependent` flag keeps its old meaning; keybinds are gated globally on
   egui's `text_edit_focused()`, not per-widget flags; EQ graph vertical axis
   is log-scaled, −∞ bottom / +6 dB top, gains clamp −120..+6 (−120 dB =
   effective −∞: within 0.3 % of the bottom edge; handle centres clamp by
   half the 8 px node so the body never leaves the window). Code aligns
   to them; changing behavior means deliberately updating tests + docs.
4. **Audio server**: keep `NO_START`; never auto-spawn jackd or bypass JACK.
5. **Packaging**: the AUR `PKGBUILD` (separate repo) fetches the GitHub
   **tag tarball**; `pkgver` must equal the `Cargo.toml` version, and
   `sha256sums` must match the tarball of the tag currently pushed. GitHub
   regenerates tag assets lazily — download and hash the live tarball to
   confirm before trusting a checksum. Never run `makepkg -C` inside a
   checkout (`-C` wipes `$PWD/src` = the Rust sources).
6. **Releases**: source commit on `main` → tag `vX.Y.Z` → push tag → update
   the AUR repo (`pkgver`/`pkgrel` + fresh `makepkg --printsrcinfo`). Do not
   force-move public tags after the AUR package references them.

## Where things are wired (quick grep targets)

- Commands/events: `Cmd::` / `Evt::` in `src/audio.rs`
- All bindable actions + key defaults: `MidiAction::ALL`, `SHORTCUT_DEFAULTS` (`src/ui.rs`)
- Effect ABI (audio v1 + GUI v1): `src/effect_api.rs`
- Import formats: `IMPORT_EXTENSIONS` (`src/audio_io.rs`)
- Export formats: `ExportFormat` (`src/audio_io.rs`)

## Docs map

| Audience | Read |
| --- | --- |
| end user | [USAGE.md](USAGE.md) |
| sysadmin / packager | [INSTALL.md](INSTALL.md) |
| effect-script author | `effects/template.rs` (self-documenting) + *Per-loop effects* in USAGE.md |
| CI/automation agents | this file |
