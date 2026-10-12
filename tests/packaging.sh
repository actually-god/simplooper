#!/usr/bin/env bash
# Packaging regression test for the Simplooper AUR package.
#
# Catches the class of failure where `makepkg -si` cannot resolve the
# PKGBUILD's dependencies on the current system — e.g. depending on a
# package name that only exists in some distros' repos (a bare `libjack`
# package does not exist on CachyOS; JACK comes from pipewire-jack there).
#
# Usage:
#   ./tests/packaging.sh          fast checks: syntax, metadata, files, deps
#   ./tests/packaging.sh --full   additionally build the package in a sandbox
#                                 and verify its contents (takes ~2 min)
#
# Exits non-zero if any check fails.

set -u
cd "$(dirname "$0")/.."

fail=0
note() { printf '== %s\n' "$*"; }
err()  { printf 'FAIL: %s\n' "$*"; fail=1; }

# --- 1. PKGBUILD syntax + required fields ----------------------------------
bash -n PKGBUILD || err "PKGBUILD has a bash syntax error"
for f in pkgname pkgver pkgrel pkgdesc arch license depends makedepends source sha256sums build package; do
  grep -qE "^${f}[=(]" PKGBUILD || err "PKGBUILD missing field: $f"
done

# --- 2. version sync --------------------------------------------------------
cargo_ver=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
pkgbuild_ver=$(sed -n 's/^pkgver=//p' PKGBUILD)
[ "$cargo_ver" = "$pkgbuild_ver" ] \
  || err "pkgver '$pkgbuild_ver' != Cargo.toml version '$cargo_ver'"

# --- 3. source/checksum arrays match ----------------------------------------
src_line=$(grep '^source=' PKGBUILD | sed 's/  #.*$//')
sha_line=$(grep '^sha256sums=' PKGBUILD | sed 's/  #.*$//')
n_src=$(printf '%s' "$src_line" | tr -cd '"' | wc -c)
n_sha=$(printf '%s' "$sha_line" | tr -cd "'" | wc -c)
[ $((n_src / 2)) -eq $((n_sha / 2)) ] \
  || err "source has $((n_src / 2)) entries but sha256sums has $((n_sha / 2))"

# --- 4. files referenced by package() ---------------------------------------
for f in simplooper.desktop assets/icon.png Cargo.lock; do
  [ -f "$f" ] || err "file required by PKGBUILD is missing: $f"
done

# --- 5. desktop entry --------------------------------------------------------
grep -q "^Type=Application$" simplooper.desktop || err "desktop: missing Type=Application"
grep -q "^Exec=" simplooper.desktop || err "desktop: missing Exec="
grep -q "^Icon=simplooper$" simplooper.desktop \
  || err "desktop: Icon must be 'simplooper' (matches installed icon file name)"

# --- 6. icon ------------------------------------------------------------------
file assets/icon.png | grep -q "PNG image data, 256 x 256" \
  || err "assets/icon.png must be a 256x256 PNG (placeholder or final artwork)"

# --- 7. dependency resolvability (the `makepkg -si` failure class) ----------
# A dep is OK if it is satisfied by an installed package (by name or via its
# Provides field) or installable from the enabled repos — exactly what
# `makepkg -si` does, checked here without needing sudo.
all_deps=$(sed -n 's/^depends=(\(.*\))$/\1/p; s/^makedepends=(\(.*\))$/\1/p' PKGBUILD | tr '\n' ' ')
[ -n "$all_deps" ] || err "could not parse depends/makedepends from PKGBUILD"

installed_names_provides=$(pacman -Qii 2>/dev/null | awk '
  /^Name[ \t]+:/ { print $NF }
  /^Provides[ \t]+:/ {
    sub(/^Provides[ \t]+:[ \t]*/, "")
    n = split($0, a, " ")
    for (i = 1; i <= n; i++) { p = a[i]; sub(/=.*/, "", p); if (p != "") print p }
  }')

for d in $all_deps; do
  if printf '%s\n' "$installed_names_provides" | grep -qx "$d"; then
    : # satisfied locally (name or Provides) — nothing to do
  elif pacman -Sp "$d" >/dev/null 2>&1; then
    note "dep '$d' not installed, but installable from enabled repos (-si would fetch it)"
  else
    err "dep '$d' is neither installed nor available in the enabled repos — makepkg -si will fail"
  fi
done

# --- 8. source layout + explicit paths (in-tree build regression) -----------
# GitHub tag tarballs extract into a top-level ${pkgname}-${pkgver}/ dir, and
# running makepkg inside a project copy makes $srcdir=$PWD/src — so
# build()/package() must reference the extracted dir by absolute path.
grep -q 'cd "\${srcdir}/\${pkgname}-\${pkgver}"' PKGBUILD \
  || err "build() must cd into \${srcdir}/\${pkgname}-\${pkgver} (GitHub tarballs have a top-level dir)"
sed -n '/^package()/,$p' PKGBUILD | grep -q '\${srcdir}' \
  || err "package() must reference \${srcdir} (install from absolute paths, not CWD-relative)"

# --- 9. optional full build ---------------------------------------------------
if [ "${1:-}" = "--full" ]; then
  tmp=$(mktemp -d) || exit 1
  trap 'rm -rf "$tmp"' EXIT
  note "sandbox makepkg build in $tmp (mirrors an in-tree project copy)"
  # Reproduce the user's exact layout: a copy of the project folder with the
  # PKGBUILD inside it, so makepkg's default $srcdir=$PWD/src collides with
  # Rust's own src/ — the scenario that broke package() before.
  mkdir -p "$tmp/proj" "$tmp/stage/simplooper-${pkgbuild_ver}"
  tar cf - --exclude=target --exclude=.git . | tar xf - -C "$tmp/proj" \
    || { err "could not stage project copy"; }
  # GitHub-layout source tarball: files under a top-level dir.
  tar cf - --exclude=target --exclude=.git . | tar xf - -C "$tmp/stage/simplooper-${pkgbuild_ver}" \
    || { err "could not stage source tree"; }
  tar czf "$tmp/proj/simplooper-${pkgbuild_ver}.tar.gz" -C "$tmp/stage" "simplooper-${pkgbuild_ver}" \
    || { err "could not create source tarball"; }
  sum=$(sha256sum "$tmp/proj/simplooper-${pkgbuild_ver}.tar.gz" | cut -d' ' -f1)
  cp PKGBUILD "$tmp/proj/PKGBUILD"
  sed -i "s|^source=.*|source=(\"simplooper-${pkgbuild_ver}.tar.gz\")|" "$tmp/proj/PKGBUILD"
  sed -i "s|^sha256sums=.*|sha256sums=('$sum')|" "$tmp/proj/PKGBUILD"
  # --nodeps: dep resolvability was already verified in step 7; we must not
  # let makepkg install pacman rust/cargo over a rustup toolchain.
  if ! (cd "$tmp/proj" && makepkg --nodeps >build.log 2>&1); then
    err "sandbox makepkg build failed (see $tmp/proj/build.log)"
    tail -20 "$tmp/proj/build.log" >&2
  fi
  pkg=$(ls "$tmp"/proj/*.pkg.tar.zst "$tmp"/proj/pkg/*.pkg.tar.zst 2>/dev/null | head -1)
  if [ -z "$pkg" ]; then
    err "no .pkg.tar.zst produced"
  else
    tar -tf "$pkg" | grep -q "usr/bin/simplooper$" \
      || err "package missing usr/bin/simplooper"
    tar -tf "$pkg" | grep -q "usr/share/applications/simplooper.desktop$" \
      || err "package missing desktop entry"
    tar -tf "$pkg" | grep -q "usr/share/icons/hicolor/256x256/apps/simplooper.png$" \
      || err "package missing icon (must install as simplooper.png)"
    if grep -q 'reference to \$srcdir' "$tmp/proj/build.log"; then
      err "build log contains a \$srcdir path leak"
    fi
  fi
fi

# --- summary -------------------------------------------------------------------
if [ $fail -ne 0 ]; then
  echo
  echo "packaging test FAILED"
  exit 1
fi
echo "packaging checks passed"
