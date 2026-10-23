#!/usr/bin/env python3
"""Build slooper's bundled piano: extract the 'Piano 1' instrument from
TimGM6mb.sf2 (GPL-2, Tim Brechbill — see synths/THIRD-PARTY-NOTICES.md)
into a compact binary blob and splice it (base64) into synths/piano.rs
via synths/piano.rs.template.

Source soundfont (NOT vendored here; 5.3 MB):
    http://deb.debian.org/debian/pool/main/t/timgm6mb-soundfont/timgm6mb-soundfont_1.3.orig.tar.gz
    (sha256 c5378b62028c920cb11e4803327983fee2f2cdff5dc89c708e39da417e51c854)

Requires numpy (build-time only):  pip install numpy

Why measured pitch: this soundfont's SF2 generator set (overridingRootKey
+ coarseTune combinations) does not follow the naive spec reading, and
fluidsynth renders every key at its tempered pitch regardless. We therefore
MEASURE each sample's natural f0 at build time and store it in the blob;
the player derives playback rate from it, which reproduces equal
temperament everywhere (verified against `fluidsynth -F` reference
renders, within 10 cents = the recording's own temperament).

Usage:  python3 tools/build_piano.py TimGM6mb.sf2 [--check]
Writes synths/piano.rs from the template + blob.
"""
import argparse, base64, hashlib, math, os, struct, sys

try:
    import numpy as np
except ImportError:
    sys.exit("this build tool needs numpy:  pip install numpy")

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
TEMPLATE = os.path.join(ROOT, "synths", "piano.rs.template")
OUT = os.path.join(ROOT, "synths", "piano.rs")

GEN_ATTENUATION = 48
GEN_COARSE_TUNE = 52
GEN_FINE_TUNE = 51
GEN_SAMPLE_ID = 53
GEN_LOOPMODE = 54
GEN_ORIG_PITCH = 58
GEN_KEYRANGE = 43
PRESET_INSTRUMENT = 41

# End-to-end pitch calibration: after the spectral estimate, shift these
# samples' stored natural pitch by N cents. Values were measured by
# rendering every key through slooper's own player and comparing against
# equal temperament (all other samples land within ±4 cents).
CALIBRATION_CENTS = {4: -12.0, 8: -60.5}


def chunks(buf, p, end):
    out = []
    while p + 8 <= end:
        cid = buf[p:p + 4]
        sz = struct.unpack("<I", buf[p + 4:p + 8])[0]
        out.append((cid, p + 8, sz))
        p += 8 + sz + (sz & 1)
    return out


def load(sf2):
    data = open(sf2, "rb").read()
    top = chunks(data, 12, len(data))
    pdta = next(x for x in top if x[0] == b"LIST" and data[x[1]:x[1] + 4] == b"pdta")
    sdta = next(x for x in top if x[0] == b"LIST" and data[x[1]:x[1] + 4] == b"sdta")
    offs = {cid: (o, s) for cid, o, s in chunks(data, pdta[1] + 4, pdta[1] + pdta[2])}
    smpl = next(x for x in chunks(data, sdta[1] + 4, sdta[1] + sdta[2]) if x[0] == b"smpl")

    def rec(cid, i, size, stride=None):
        base = offs[cid][0]
        stride = stride or size
        return data[base + i * stride: base + (i + 1) * stride]
    return data, rec, offs, smpl


def s16(v):
    return v - 65536 if v >= 32768 else v


def measure_cents(pcm, sr):
    """Natural f0 of a sample recording (cents rel. A4=440).

    Harmonic-comb salience on the magnitude spectrum with 1/k weights:
    a candidate f0 scores the (log) energy at f0, 2f0, 3f0…; piano
    spectra are rich enough that this nails the fundamental where
    autocorrelation and YIN pick up spurious periodicities. Refine using
    parabolic peaks of harmonics 2..10 (weighted mean of f_k/k)."""
    x = np.asarray(pcm, dtype=np.float64)
    n0 = min(int(0.05 * len(x)), int(0.05 * sr))
    seg = x[n0:n0 + int(min(0.55, len(x) / sr - 0.05) * sr)]
    if len(seg) < 2048:
        raise ValueError("sample too short to measure pitch")
    seg = (seg - seg.mean()) * np.hanning(len(seg))
    nfft = 1 << 17
    X = np.abs(np.fft.rfft(seg, nfft))
    binhz = sr / nfft
    logX = np.log(X + X.max() * 1e-6)

    def at(f):  # log magnitude at f (nearest bin)
        i = int(round(f / binhz))
        return logX[i] if 0 <= i < len(logX) else logX[0]

    ks = np.arange(1, 11)
    w = 1.0 / ks
    cand = np.arange(24.0, 2400.0, 0.5)
    idx = np.clip((np.outer(cand, ks) / binhz).round().astype(int), 0, len(logX) - 1)
    sc = (logX[idx] * w).sum(axis=1)
    f0 = float(cand[int(np.argmax(sc))])

    def peak(f):
        i = int(round(f / binhz))
        if i < 2 or i + 2 >= len(X):
            return 0.0, 0.0
        j = i - 2 + int(np.argmax(X[i - 2:i + 3]))
        a, b, c = X[j - 1], X[j], X[j + 1]
        den = a - 2 * b + c
        d = 0.5 * (a - c) / den if den != 0 else 0.0
        return b, (j + d) * binhz

    num = den = 0.0
    for k in range(2, 11):
        mag, fh = peak(k * f0)
        if mag > 0 and fh > 0:
            num += mag * fh / k
            den += mag
    if den > 0:
        f0 = num / den
    return 1200.0 * math.log2(f0 / 440.0)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("sf2")
    ap.add_argument("--check", action="store_true", help="inventory only, write nothing")
    args = ap.parse_args()

    data, rec, offs, smpl = load(args.sf2)
    print("sha256", hashlib.sha256(data).hexdigest())

    npres = offs[b"phdr"][1] // 38 - 1
    piano_bag = None
    for i in range(npres):
        r = rec(b"phdr", i, 38)
        if r[:20].split(b"\0")[0] == b"Piano 1":
            piano_bag = struct.unpack("<H", r[24:26])[0]
    assert piano_bag is not None, "preset 'Piano 1' not found"
    stride = None
    for st in (2, 4):
        b0 = struct.unpack("<H", rec(b"pbag", piano_bag, 2, st)[0:2])[0]
        b1 = struct.unpack("<H", rec(b"pbag", piano_bag + 1, 2, st)[0:2])[0]
        if b1 > b0:
            inst = None
            for g in range(b0, b1):
                op, am = struct.unpack("<2H", rec(b"pgen", g, 4))
                if op == PRESET_INSTRUMENT:
                    inst = am
            if inst is not None and rec(b"inst", inst, 22)[:20].split(b"\0")[0] == b"Piano 1":
                stride = st
    assert stride, "could not resolve preset instrument"
    b0 = struct.unpack("<H", rec(b"pbag", piano_bag, 2, stride)[0:2])[0]
    b1 = struct.unpack("<H", rec(b"pbag", piano_bag + 1, 2, stride)[0:2])[0]
    inst = None
    for g in range(b0, b1):
        op, am = struct.unpack("<2H", rec(b"pgen", g, 4))
        if op == PRESET_INSTRUMENT:
            inst = am
    print("Piano 1 instrument", inst)

    i0 = struct.unpack("<H", rec(b"inst", inst, 22)[20:22])[0]
    i1 = struct.unpack("<H", rec(b"inst", inst + 1, 22)[20:22])[0]

    zones = []
    sample_order = []
    sample_map = {}
    for b in range(i0, i1):
        z0 = struct.unpack("<H", rec(b"ibag", b, 4)[0:2])[0]
        z1 = struct.unpack("<H", rec(b"ibag", b + 1, 4)[0:2])[0]
        d = {}
        for g in range(z0, z1):
            op, am = struct.unpack("<2H", rec(b"igen", g, 4))
            d[op] = am
        sid = d.get(GEN_SAMPLE_ID)
        if sid is None:
            continue
        r = rec(b"shdr", sid, 46)
        st, en, ls, le, sr = struct.unpack("<5I", r[20:40])
        orig = r[40]
        corr = struct.unpack("<b", r[41:42])[0]
        root = d.get(GEN_ORIG_PITCH, orig)
        kr = d.get(GEN_KEYRANGE, 0x7F00)
        looping = (d.get(GEN_LOOPMODE, 0) & 1) == 1 and le > ls and en > ls
        fine = s16(d.get(GEN_FINE_TUNE, 0)) + corr
        coarse = s16(d.get(GEN_COARSE_TUNE, 0))
        if sid not in sample_map:
            sample_map[sid] = len(sample_order)
            sample_order.append([st, en, ls, le, sr])
        zones.append(dict(
            lo=kr & 255, hi=(kr >> 8) & 255,
            sample=sample_map[sid],
            root=root & 255,
            looping=1 if looping else 0,
            ls=max(0, ls - st) if looping else 0,
            le=min(en - st, le - st) if looping else 0,
            att=max(0, min(1000, s16(d.get(GEN_ATTENUATION, 0)))),
            fine=fine, coarse=coarse,
        ))
    zones.sort(key=lambda z: (z["lo"], z["hi"]))

    cov = 0
    for z in zones:
        if z["lo"] > cov:
            sys.exit(f"key coverage gap {cov}..{z['lo']}")
        cov = max(cov, z["hi"] + 1)
    if cov < 109:
        sys.exit(f"coverage ends at {cov}")

    # measure each sample's natural pitch once
    cents = []
    for st, en, ls, le, sr in sample_order:
        raw = data[smpl[1] + st * 2: smpl[1] + en * 2]
        pcm = np.frombuffer(raw, dtype="<i2").astype(np.float64)
        c = measure_cents(pcm, sr)
        c += CALIBRATION_CENTS.get(len(cents), 0.0)
        midi = 69 + c / 100
        cents.append(int(round(c)))
        print(f"sample {len(cents)-1}: sr {sr} natural midi {midi:6.1f} ({int(round(c)):+6d} cents rel A4)")

    blob = bytearray()
    blob += b"SLPZ"
    blob += struct.pack("<III", 2, len(zones), len(sample_order))
    for z in zones:
        blob += struct.pack(
            "<BBHBBIIhhh4x",
            z["lo"], z["hi"], z["sample"], z["root"], z["looping"],
            z["ls"], z["le"], z["att"], z["fine"], z["coarse"],
        )
    pcm_all = bytearray()
    table = bytearray()
    for (st, en, ls, le, sr), c in zip(sample_order, cents):
        raw = data[smpl[1] + st * 2: smpl[1] + en * 2]
        table += struct.pack("<IIi", len(pcm_all) // 2, en - st, c)
        pcm_all += raw
    # sample rates live in a small side table (u32 each) after the zone table
    blob += bytes(table)
    for _, _, _, _, sr in sample_order:
        blob += struct.pack("<I", sr)
    blob += bytes(pcm_all)

    print(f"zones {len(zones)}  samples {len(sample_order)}  blob {len(blob)} B")
    b64 = base64.b64encode(bytes(blob)).decode()
    if args.check:
        return
    tpl = open(TEMPLATE).read()
    assert "%%BLOB%%" in tpl
    out = tpl.replace("%%BLOB%%", b64)
    open(OUT, "w").write(out)
    print("wrote", OUT, len(out), "bytes")


if __name__ == "__main__":
    main()
