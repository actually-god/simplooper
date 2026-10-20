# Installing slooper

slooper is a Linux desktop application (x86_64). It talks to a **JACK**
audio server, so everything below assumes you have — or will install — either
**PipeWire** (recommended; its `pipewire-jack` shim exposes the JACK API) or
classic **jack2**. The app connects with `NO_START`: it **never spawns an
audio server itself**; if none is running it reports the error and stays idle.

For day-to-day operation see [USAGE.md](USAGE.md); for repo/development
notes see [AGENT-GUIDE.md](AGENT-GUIDE.md).

## Arch Linux (and Arch derivatives, e.g. CachyOS/Manjaro)

Once published, slooper installs from the **AUR** like any other package:

```sh
yay -S slooper        # or: paru -S slooper
```

To build the package yourself (or before the AUR package exists), the
`PKGBUILD` lives in the AUR package repository
(`https://aur.archlinux.org/slooper.git`) — **not** in this source repo:

```sh
git clone https://aur.archlinux.org/slooper.git
cd slooper
makepkg -si              # fetches the tagged source tarball from GitHub,
                         # builds with cargo, installs binary + desktop entry + icon
```

Notes about that PKGBUILD:

- It builds from the GitHub **tag tarball** `v<version>`, so the package always
  matches a released version (this is not a `-git` package).
- Runtime dep is the shared library `libjack.so` — satisfied by **either**
  `pipewire-jack` or the official JACK packages, so it does not force one
  implementation on you.
- `optdepends`: `rust` (only needed to compile user effect scripts at runtime)
  and `ffmpeg` (only for MP3/OGG export). The app runs fine without both.

## Any Linux distro — build from source

Requirements:

| What | Why | Example packages (Arch / Debian·Ubuntu / Fedora) |
| --- | --- | --- |
| Rust toolchain (stable) | build (and to compile effect scripts at runtime — optional feature) | `rust` / `rustc` + `cargo` / `rust` |
| cargo | build | (with toolchain) |
| JACK headers + pkg-config file | the `jack` crate links against libjack at build time | `jack` / `libjack-jackd2-dev` / `jackaudio-devel` |
| pkgconf | finds the JACK `.pc` file | `pkgconf` / `pkgconf` / `pkgconf-pkgconf` |
| A C toolchain (linker) | standard cargo linking | `base-devel` / `build-essential` / `@development-tools` |

No other system libraries are needed — the GUI stack (egui/eframe over
glow/winit) is pure Rust and dlopens GL/X11/Wayland at runtime.

```sh
git clone https://github.com/actually-god/slooper
cd slooper
cargo build --release
./target/release/slooper          # run directly, or install:
cargo install --path .               # puts the binary in ~/.cargo/bin
```

To get the application-menu entry and icon as well (system-wide install):

```sh
sudo install -Dm755 target/release/slooper /usr/bin/slooper
sudo install -Dm644 slooper.desktop /usr/share/applications/slooper.desktop
sudo install -Dm644 assets/icon.png \
  /usr/share/icons/hicolor/256x256/apps/slooper.png
```

## Audio server setup

slooper is just another JACK client — whatever works for other DAWs works
for it.

**PipeWire (recommended).** Install PipeWire with the JACK support package
(`pipewire-jack` on Arch/DP/Fedora — the package that provides `libjack.so`).
Start a session as usual; PipeWire answers JACK clients automatically.

**jack2.** Install `jack2`, then start a server before launching slooper,
e.g. `jackd -d alsa` or via your GUI of choice (qjackctl).

On launch, slooper prints `JACK client 'slooper' active @ <rate> Hz` and
shows `JACK ready` in the top bar. If no server is up, you'll see a connection
error instead — start your server, then relaunch.

## Optional runtime packages

- **rustc** — the per-loop effects framework compiles user effect scripts with
  the system `rustc` (`--edition 2021 --crate-type cdylib`). Without rustc the
  app runs normally; custom effects just list as "compiler not found".
- **ffmpeg** — enables exporting loops/projects as MP3 or OGG Vorbis. WAV and
  FLAC export are built in and never need ffmpeg.

## Files slooper creates

| Path | Purpose |
| --- | --- |
| `$XDG_CONFIG_HOME/slooper/config.json` (fallback `~/.config/slooper/config.json`) | port selections, MIDI/keyboard bindings, effect parameter values, metronome/quantize settings |
| `~/.local/share/slooper/effects/` | effect-script folder (created on first run, seeded with `template.rs` + the ABI contract file; the location is user-configurable in the FX window) |

## Uninstalling

- Package installs (`pacman -R slooper`): removes binary, desktop entry, icon.
- Remove `~/.config/slooper/` and `~/.local/share/slooper/` to discard
  your config and the effects folder.

## License

slooper is licensed under the **MIT license** — see
[LICENSE](../LICENSE). You are free to use, modify, and redistribute it,
including commercially, as long as the copyright notice is kept.
