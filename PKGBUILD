# Maintainer: actually-god <actually-god@users.noreply.github.com>
#
# Simplooper — lightweight JACK step looper (eframe/egui + jack crate).
# Upstream: https://github.com/actually-god/simplooper

pkgname=simplooper
# Keep in sync with Cargo.toml
pkgver=0.1.0
# Bumped for the re-cut v0.1.0 tag (configurable metronome beats per
# measure, three-tone click, instant downbeat start, play/stop metronome
# button; on top of import/export, metronome, latency compensation) so
# `pacman -U` upgrades existing 0.1.0-6 installs.
pkgrel=7
pkgdesc="Lightweight JACK step looper: four quantized loops, MIDI and keyboard control"
arch=(x86_64)
url="https://github.com/actually-god/simplooper"
license=(MIT)

# Runtime: the binary dlopens libjack at runtime (no NEEDED entry), so we
# depend on the shared library itself rather than a specific JACK package.
# Both pipewire-jack and the official libjack provide `libjack.so` (the same
# convention as ardour), which keeps `makepkg -si` working no matter which
# JACK implementation is installed or present in the enabled repos — a plain
# `libjack` package name does not exist on every distro (e.g. CachyOS).
depends=(libjack.so)
# Build: cargo/rust may come from rustup or pacman; `jack` asks for the
# headers + .pc file, provided by either JACK implementation.
makedepends=(cargo rust pkgconf jack)
source=("https://github.com/actually-god/simplooper/archive/refs/tags/v${pkgver}.tar.gz")
sha256sums=('a6c07444ba6afd11e7f385a4ec7830c82519f5626c707aae911227163fd26ee7')

build() {
  # Remap the build dir out of the binary so no $srcdir paths leak into
  # panic/track_caller strings (keeps makepkg's packaging check clean).
  export RUSTFLAGS="--remap-path-prefix=${srcdir}=/usr/src/${pkgname}"
  # GitHub tag tarballs extract into a top-level directory. cd into it so
  # cargo uses THIS Cargo.toml (not one in a parent dir) and puts target/
  # inside the source tree — package() below refers to that exact path.
  cd "${srcdir}/${pkgname}-${pkgver}"
  cargo build --release --locked
}

package() {
  local s="${srcdir}/${pkgname}-${pkgver}"
  install -Dm755 "${s}/target/release/${pkgname}" -t "${pkgdir}/usr/bin"

  install -Dm644 "${s}/${pkgname}.desktop" \
    -t "${pkgdir}/usr/share/applications"

  # Placeholder icon (a blank slate square) — replaced when the real
  # artwork is provided. Must be named after the app: Icon=simplooper.
  install -Dm644 "${s}/assets/icon.png" \
    "${pkgdir}/usr/share/icons/hicolor/256x256/apps/simplooper.png"
}
