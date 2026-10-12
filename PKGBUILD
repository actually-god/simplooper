# Maintainer: <your name> <your email>
#
# Simplooper — lightweight JACK step looper (eframe/egui + jack crate).
#
# NOTE (pre-upload): the url/source below point at a placeholder repository.
# Replace with the real upstream git URL before submitting to the AUR, and
# replace SKIP with the sha256 of the source tarball or git commit.

pkgname=simplooper
# Keep in sync with Cargo.toml
pkgver=0.1.0
pkgrel=1
pkgdesc="Lightweight JACK step looper: four quantized loops, MIDI and keyboard control"
arch=(x86_64)
url="https://github.com/your-username/simplooper"  # TODO: real upstream URL
license=(MIT)

depends=(libjack)
makedepends=(cargo rust libjack pkgconf)
source=("https://github.com/your-username/simplooper/archive/refs/tags/v${pkgver}.tar.gz")
sha256sums=('SKIP')  # TODO: real checksum before AUR submission

build() {
  # Remap the build dir out of the binary so no $srcdir paths leak into
  # panic/track_caller strings (keeps makepkg's packaging check clean).
  export RUSTFLAGS="--remap-path-prefix=${srcdir}=/usr/src/${pkgname}"
  cargo build --release --locked
}

package() {
  install -Dm755 "target/release/${pkgname}" -t "${pkgdir}/usr/bin"

  install -Dm644 "${pkgname}.desktop" \
    -t "${pkgdir}/usr/share/applications"

  # Placeholder icon (a blank slate square) — replaced when the real
  # artwork is provided. Must be named after the app: Icon=simplooper.
  install -Dm644 "assets/icon.png" \
    "${pkgdir}/usr/share/icons/hicolor/256x256/apps/simplooper.png"
}
