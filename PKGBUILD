# Maintainer: actually-god <actually-god@users.noreply.github.com>
#
# Simplooper — lightweight JACK audio looper (eframe/egui + jack crate).
# Upstream: https://github.com/actually-god/simplooper
pkgname=simplooper
# Keep in sync with Cargo.toml
pkgver=0.1.0
# Bumped for the re-cut v0.1.0 tag (RT-loop compaction, eframe feature trim
# to the glow renderer, panic=abort, cached tiled waveforms, doc sync):
# 0.1.0-10 upgrades existing 0.1.0-9 installs.
pkgrel=10
pkgdesc="Lightweight JACK audio looper: 16 quantized loops, MIDI and keyboard control"
arch=(x86_64)
url="https://github.com/actually-god/simplooper"
license=(MIT)
# Runtime: the binary dlopens libjack at runtime (no NEEDED entry — verified
# with readelf -d), so we depend on the shared library itself rather than a
# specific JACK package. Both pipewire-jack and the official libjack provide
# `libjack.so` (the same convention as ardour), which keeps `makepkg -si`
# working no matter which JACK implementation is installed — a plain `libjack`
# package name does not exist on every distro (e.g. CachyOS). The GUI stack
# (winit/glutin/wayland) also dlopens its libraries, so nothing else is a
# hard runtime dep.
depends=(libjack.so)
# Build: cargo/rust may come from rustup or pacman; `jack` provides the
# headers + .pc file, `pkgconf` finds it; either JACK implementation works.
makedepends=(cargo rust pkgconf jack)
source=("https://github.com/actually-god/simplooper/archive/refs/tags/v${pkgver}.tar.gz")
sha256sums=('38775da46bd1dd0c503779b34ccb635496f38888816b76dcbd85ab35640996cd')
# No check(): the test suite's e2e half drives a live JACK server in real
# time and self-skips without one, so a build-time run would test nothing
# new on build machines and double compile time on top of the release build.
# Run ./tests/packaging.sh --full plus `cargo test` in a JACK session.
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
  local _src="${srcdir}/${pkgname}-${pkgver}"
  install -Dm755 "${_src}/target/release/${pkgname}" -t "${pkgdir}/usr/bin"
  install -Dm644 "${_src}/${pkgname}.desktop" \
    -t "${pkgdir}/usr/share/applications"
  # Placeholder icon (a blank slate square) — replaced when the real
  # artwork is provided. Must be named after the app: Icon=simplooper.
  install -Dm644 "${_src}/assets/icon.png" \
    "${pkgdir}/usr/share/icons/hicolor/256x256/apps/simplooper.png"
}
