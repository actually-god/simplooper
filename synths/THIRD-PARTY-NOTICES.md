# Third-party notices

## TimGM6mb soundfont — the recorded piano samples

`slooper/synths/piano.rs` embeds a PCM multisample blob (built by
`tools/build_piano.py`) derived from:

> **TimGM6mb.sf2** — a compact General-MIDI soundfont by Tim Brechbill,
> taken from Debian upstream:
> http://deb.debian.org/debian/pool/main/t/timgm6mb-soundfont/
> (timgm6mb-soundfont 1.3, `TimGM6mb.sf2`)

The blob contains resampled grand-piano zones (pitch-verified per key)
from that soundfont; the zone selection, loop points and player code are
slooper's own. The soundfont is licensed under the GNU GPL version 2 (see
the `debian/copyright` file of the upstream source package for the
authoritative notice); the sample data shipped inside `piano.rs` is
therefore covered by GPL-2 as well. The full GPL-2 text is at
https://www.gnu.org/licenses/old-licenses/gpl-2.0.txt .

## fan455_piano_synthesis — model of earlier releases (≤ 0.7)

Versions up to 0.7 bundled a modal-synthesis piano translated from the
open-source project:

> **fan455_piano_synthesis** — piano sound synthesis using physical
> modelling. Copyright (c) 2024 fan455
> https://github.com/fan455/fan455_piano_synthesis (arXiv:2409.03481)

That model (MIT-licensed) is no longer shipped; the notice remains for
historical accuracy of those releases.
