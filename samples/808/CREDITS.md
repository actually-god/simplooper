# TR-808 starter pack — provenance & licensing

Fourteen mono 16-bit / 44.1 kHz WAV one-shots sampled **directly from a real
Roland TR-808 Rhythm Composer (serial no. 103852)** by Michael Fischer
(Technopolis, fischer1@student.msu.edu) in September 1994 — the legendary
"TR808WAV" sample set, originally published on
`machines.hyperreal.org` with the author's own note that the samples are
*"ABSOLUTELY FREE"* and ship with **no licensing restrictions**.

Retrieved October 2026 from the ISC-licensed npm repackaging
[`@fluid-music/tr-808`](https://www.npmjs.com/package/@fluid-music/tr-808)
(git: [fluid-music/open-drums](https://github.com/fluid-music/open-drums)),
whose README documents the same no-restrictions origin.

The app's filenames map to the original knob-coded names (`<sound><tuning><decay>`,
knob positions 0/2.5/5/7.5/10):

| Pack file            | Original file | Sound                              |
| -------------------- | ------------- | ---------------------------------- |
| `808-kick.wav`       | `BD2550.WAV`  | Bass drum, mid tuning / mid decay  |
| `808-sub.wav`        | `BD2575.WAV`  | Bass drum, mid tuning / long decay |
| `808-snare.wav`      | `SD5050.WAV`  | Snare drum, mid tone / mid snappy  |
| `808-snare-long.wav` | `SD7575.WAV`  | Snare drum, bright / long          |
| `808-clap.wav`       | `CP.WAV`      | Hand clap                          |
| `808-hat-closed.wav` | `CH.WAV`      | Closed hi-hat                      |
| `808-hat-open.wav`   | `OH50.WAV`    | Open hi-hat, mid decay             |
| `808-crash.wav`      | `CY2550.WAV`  | Cymbal                             |
| `808-cowbell.wav`    | `CB.WAV`      | Cow bell                           |
| `808-tom-low.wav`    | `LT50.WAV`    | Low tom                            |
| `808-tom-mid.wav`    | `MT50.WAV`    | Mid tom                            |
| `808-tom-high.wav`   | `HT50.WAV`    | Hi tom                             |
| `808-rimshot.wav`    | `RS.WAV`      | Rim shot                           |
| `808-clave.wav`      | `CL.WAV`      | Claves                             |

The app embeds these files and seeds them to
`$XDG_DATA_HOME/slooper/samples/808` on first launch.

With thanks to Michael Fischer — and to every 808 track since 1980.
