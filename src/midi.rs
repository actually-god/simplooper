//! MIDI message parsing and display formatting for slooper.
//!
//! The realtime audio thread hands us already-split raw messages (JACK's
//! `jack_midi_event_get` handles running status internally), so each message
//! parsed here is self-contained.

/// A parsed, owned MIDI message.
#[derive(Clone, Debug, PartialEq)]
pub enum MidiMsg {
    NoteOn { channel: u8, note: u8, velocity: u8 },
    NoteOff { channel: u8, note: u8, velocity: u8 },
    PolyPressure { channel: u8, note: u8, pressure: u8 },
    ControlChange { channel: u8, number: u8, value: u8 },
    ProgramChange { channel: u8, program: u8 },
    ChannelPressure { channel: u8, pressure: u8 },
    PitchBend { channel: u8, value: i16 },
    SystemRealtime(u8),
    /// SysEx / system common — kept as raw bytes.
    SystemCommon { data: Vec<u8> },
}

impl MidiMsg {
    /// Parse a single raw MIDI message. Returns `None` for malformed input.
    pub fn parse(bytes: &[u8]) -> Option<MidiMsg> {
        let b0 = *bytes.first()?;

        // Real-time messages are single bytes (0xF8..=0xFF).
        if b0 >= 0xF8 {
            return Some(MidiMsg::SystemRealtime(b0));
        }
        // SysEx start/end and other system common.
        if b0 == 0xF0 || b0 == 0xF7 {
            return Some(MidiMsg::SystemCommon { data: bytes.to_vec() });
        }

        let channel = b0 & 0x0F;
        let kind = b0 >> 4;
        let b1 = *bytes.get(1)?;
        let b2 = bytes.get(2).copied().unwrap_or(0);

        match kind {
            0x8 => Some(MidiMsg::NoteOff { channel, note: b1, velocity: b2 }),
            0x9 if b2 == 0 => Some(MidiMsg::NoteOff { channel, note: b1, velocity: 0 }),
            0x9 => Some(MidiMsg::NoteOn { channel, note: b1, velocity: b2 }),
            0xA => Some(MidiMsg::PolyPressure { channel, note: b1, pressure: b2 }),
            0xB => Some(MidiMsg::ControlChange { channel, number: b1, value: b2 }),
            0xC => Some(MidiMsg::ProgramChange { channel, program: b1 }),
            0xD => Some(MidiMsg::ChannelPressure { channel, pressure: b1 }),
            // Raw 14-bit value (LSB | MSB<<7), re-centered so 0 = no bend.
            0xE => Some(MidiMsg::PitchBend {
                channel,
                value: (((b2 as i16) << 7) | (b1 as i16)) - 8192,
            }),
            _ => None, // status bytes 0x0..=0x7 are invalid without running status
        }
    }

    /// Short type name for display ("NOTE ON", "CC", ...).
    pub fn type_name(&self) -> &'static str {
        match self {
            MidiMsg::NoteOn { .. } => "NOTE ON",
            MidiMsg::NoteOff { .. } => "NOTE OFF",
            MidiMsg::PolyPressure { .. } => "POLY PRES",
            MidiMsg::ControlChange { .. } => "CC",
            MidiMsg::ProgramChange { .. } => "PC",
            MidiMsg::ChannelPressure { .. } => "CH PRESS",
            MidiMsg::PitchBend { .. } => "PITCH BEND",
            MidiMsg::SystemRealtime(_) => "REALTIME",
            MidiMsg::SystemCommon { .. } => "SYSEX",
        }
    }

    /// Human-readable one-line description (channel, data, values).
    pub fn describe(&self) -> String {
        match self {
            MidiMsg::NoteOn { channel, note, velocity } => format!(
                "ch {:02}  note {} ({})  vel {}",
                channel + 1,
                note_name(*note),
                note,
                velocity
            ),
            MidiMsg::NoteOff { channel, note, velocity } => format!(
                "ch {:02}  note {} ({})  vel {}",
                channel + 1,
                note_name(*note),
                note,
                velocity
            ),
            MidiMsg::PolyPressure { channel, note, pressure } => format!(
                "ch {:02}  note {} ({})  press {}",
                channel + 1,
                note_name(*note),
                note,
                pressure
            ),
            MidiMsg::ControlChange { channel, number, value } => format!(
                "ch {:02}  CC {} ({})  val {}",
                channel + 1,
                number,
                cc_name(*number),
                value
            ),
            MidiMsg::ProgramChange { channel, program } => format!(
                "ch {:02}  program {} (bank-relative)",
                channel + 1,
                program
            ),
            MidiMsg::ChannelPressure { channel, pressure } => {
                format!("ch {:02}  press {}", channel + 1, pressure)
            }
            MidiMsg::PitchBend { channel, value } => {
                format!("ch {:02}  bend {} (center 0)", channel + 1, value)
            }
            MidiMsg::SystemRealtime(code) => format!("real-time status 0x{:02X}", code),
            MidiMsg::SystemCommon { data } => format!(
                "sys common / sysex, {} bytes",
                data.len()
            ),
        }
    }
}

/// Standard note name with scientific octave (MIDI 60 = C4).
pub fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!("{}{}", NAMES[(note % 12) as usize], ((note / 12) as i8 - 1))
}

/// Standard names for the most common CC numbers; "GENERIC" otherwise.
pub fn cc_name(number: u8) -> &'static str {
    const TABLE: [(u8, &str); 47] = [
        (0, "BANK SEL MSB"),
        (1, "MODULATION"),
        (2, "BREATH"),
        (3, "FOOT CTRL"),
        (4, "PORTAMENTO TIME"),
        (5, "DATA ENTRY"),
        (6, "VOLUME"),
        (7, "BALANCE"),
        (8, "PAN"),
        (9, "EXPRESSION"),
        (10, "EFFECTS 1"),
        (11, "EFFECTS 2"),
        (16, "BANK SEL LSB"),
        (17, "MOD FINE TUNE"),
        (20, "NRPN HI"),
        (21, "NRPN LO"),
        (32, "GENERIC SLIDER 1"),
        (64, "SUSTAIN"),
        (65, "PORTAMENTO ON/OFF"),
        (66, "TREMOLO"),
        (67, "VIBRATO DEPTH"),
        (68, "THICKNESS"),
        (69, "RELEASE TIME"),
        (70, "ATTACK"),
        (71, "BRIGHTNESS"),
        (72, "DECAY TIME"),
        (73, "VIBRATO RATE"),
        (74, "VIBRATO DEPTH 2"),
        (75, "RELEASE/ATTACK"),
        (79, "ARP RATE"),
        (84, "REGISTER UP"),
        (85, "REGISTER DN"),
        (91, "HOLD 2"),
        (92, "SOUND VAR"),
        (93, "HARMONIC CONTENT"),
        (94, "ADDITIVE TIMBRE"),
        (95, "NOISE LEVEL"),
        (96, "EFFECTS SPACE"),
        (97, "ECHO TIME"),
        (98, "FEEDBACK"),
        (99, "REVERB DEPTH"),
        (100, "TREMOLO RATE"),
        (120, "ALL SOUND OFF"),
        (123, "ALL NOTES OFF"),
        (124, "RESET CONTROLLERS"),
        (125, "MONO ON"),
        (126, "POLY ON"),
    ];
    TABLE
        .iter()
        .find(|(n, _)| *n == number)
        .map(|(_, s)| *s)
        .unwrap_or(if number < 32 || (40..=64).contains(&number) || (91..=120).contains(&number) {
            "GENERIC"
        } else {
            "RESERVED"
        })
}

/// Space-separated uppercase hex, e.g. `90 3C 64`.
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A bindable MIDI trigger: message type + channel + first data byte.
///
/// Only the three most useful live-controller triggers are bindable:
/// NOTE ON (note number), CC (controller number) and PC (program). The second
/// data byte (velocity / CC value) is deliberately ignored so a pedal or knob
/// fires regardless of its exact position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MidiBind {
    /// 0 = NOTE ON, 1 = CC, 2 = PC.
    pub ty: u8,
    pub ch: u8,
    pub d1: u8,
}

impl MidiBind {
    pub const TYPE_NAMES: [&'static str; 3] = ["NOTE", "CC", "PC"];

    /// Extract a binding from a parsed message, if the type is bindable.
    pub fn from_msg(m: &MidiMsg) -> Option<MidiBind> {
        match m {
            MidiMsg::NoteOn { channel, note, .. } => Some(MidiBind { ty: 0, ch: *channel, d1: *note }),
            MidiMsg::ControlChange { channel, number, .. } => {
                Some(MidiBind { ty: 1, ch: *channel, d1: *number })
            }
            MidiMsg::ProgramChange { channel, program } => {
                Some(MidiBind { ty: 2, ch: *channel, d1: *program })
            }
            _ => None,
        }
    }

    /// True when an incoming message would fire this binding.
    pub fn matches(&self, m: &MidiMsg) -> bool {
        MidiBind::from_msg(m) == Some(*self)
    }

    /// Short label for the settings UI, e.g. `CC 64 ch01`.
    pub fn label(&self) -> String {
        match self.ty {
            0 => format!("NOTE {} ({}) ch{:02}", note_name(self.d1), self.d1, self.ch + 1),
            1 => format!("CC {} ({}) ch{:02}", self.d1, cc_name(self.d1), self.ch + 1),
            _ => format!("PC {} ch{:02}", self.d1, self.ch + 1),
        }
    }

    /// True when two bindings sit on the same AXIS: message TYPE + CHANNEL.
    /// A continuous (knob) binding owns its whole axis — it deliberately
    /// does NOT bind the data byte, so a controller whose CC number changes
    /// mid-sweep still drives it, and the min/max wizard can learn "any
    /// 0..=127 value" without freezing on the first message it saw.
    pub fn same_axis(&self, o: &MidiBind) -> bool {
        self.ty == o.ty && self.ch == o.ch
    }

    /// Axis name for the knob learner: message TYPE + CHANNEL only. The
    /// data byte (CC number for normal knobs, velocity for notes) is the
    /// VALUE the learner reads — never part of the axis identity, so a
    /// controller that changes numbers mid-sweep still tracks.
    pub fn axis_label(&self) -> String {
        match self.ty {
            0 => format!("NOTE ch{:02}", self.ch + 1),
            1 => format!("CC ch{:02}", self.ch + 1),
            _ => format!("PC ch{:02}", self.ch + 1),
        }
    }
}

/// Rescale a raw controller value from a learned axis range onto 0..=127.
///
/// `lo`/`hi` are the raw values captured at the control's minimum and
/// maximum positions. A full-range knob (`0`, `127`) is the identity; a
/// knob that only ever sends, say, 9..=110 is stretched so 9 still means
/// "fully off" and 110 "fully on". A control wired backwards (min sends the
/// HIGHER number) scales inverted, which is exactly right — the axis still
/// reads low→high as the user moves the control.
pub fn scale_axis(raw: u8, lo: u8, hi: u8) -> u8 {
    if lo == hi {
        // Degenerate calibration (min == max): no usable span. Treat the
        // raw value as already being in program space.
        return raw;
    }
    let (lo, hi) = (i32::from(lo), i32::from(hi));
    let span = hi - lo; // negative when the axis is wired inverted
    let num = (i32::from(raw) - lo) * 127;
    // Integer round-to-nearest; works for either sign of span because
    // truncating division rounds toward zero.
    let q = (num + span / 2) / span;
    q.clamp(0, 127) as u8
}

/// A bindable *action* a MIDI trigger can fire. Mirrors the keyboard
/// shortcuts (selected-track actions) plus direct per-track actions and the
/// global transport controls.
///
/// The removed v0.1 actions (overdub, import, dependent mode, undo/redo) are
/// gone from the table; a persisted config that still names one deserializes
/// to [`MidiAction::Reserved`] and the binding is dropped on load.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MidiAction {
    /// Record the *selected* track (capture starts now; the take becomes an
    /// asset placed at the playhead when recording stops).
    RecSelected,
    ClearSelected,
    /// Toggle the selected track's play/stop (its lane in the transport).
    PlaySelected,
    /// Open the export prompt (one track or the whole project).
    Export,
    /// Select track `i` (0-based).
    Select(usize),
    /// Direct per-track actions (independent of selection).
    RecLoop(usize),
    ClearLoop(usize),
    PlayLoop(usize),
    /// Toggle the metronome (click goes to the outputs only, never recorded).
    Metronome,
    /// Select the next track (wraps around).
    SelectNext,
    /// Select the previous track (wraps around).
    SelectPrev,
    /// Start the global transport (all *sounding* tracks play together).
    PlayAll,
    /// Stop the global transport (playhead holds its position).
    StopAll,
    /// Set track `i`'s playback volume from a continuous controller: the
    /// binding fires on the message VALUE (CC / note velocity, 0..=127;
    /// 0 = -inf dB, 127 = 0 dB). Keyboard bindings cannot carry a value and
    /// are never fired for these actions.
    VolLoop(usize),
    /// Set track `i`'s gain trim from a continuous controller (0 = -inf dB,
    /// 80 = 0 dB, 127 = +10 dB). Value semantics as [`MidiAction::VolLoop`].
    GainLoop(usize),
    /// Set track `i`'s stereo pan from a continuous controller (0 = hard
    /// left, 64 = centre, 127 = hard right; equal-power law). Value
    /// semantics as [`MidiAction::VolLoop`].
    PanLoop(usize),
    /// Catch-all for action names this build no longer knows (see the enum
    /// doc); never bindable, never fired.
    #[serde(other)]
    Reserved,
}

impl MidiAction {
    /// Every bindable action, in UI order. `ALL[i]` is the action stored at
    /// index `i` of the binding table: selected-track globals, one `Select`
    /// per track, record/clear/play for each of the 16 tracks, the metronome
    /// and selection movers, the global transport (play all / stop all), then
    /// the per-track continuous (knob/slider) bindings.
    ///
    /// v0.2 reshaped this table when overdub/import/dependent/undo/redo were
    /// removed; the old positional tables (length 140 / 25 / 9) are remapped
    /// on load (see `ui::migrate_shortcuts`). New actions must be APPENDED.
    pub const ALL: [MidiAction; 121] = {
        // const-friendly construction: the layout is fully arithmetic.
        let mut a = [MidiAction::Reserved; 121];
        a[0] = MidiAction::RecSelected;
        a[1] = MidiAction::ClearSelected;
        a[2] = MidiAction::PlaySelected;
        a[3] = MidiAction::Export;
        let mut i = 0usize;
        while i < 16 {
            a[4 + i] = MidiAction::Select(i);
            a[20 + i * 3] = MidiAction::RecLoop(i);
            a[21 + i * 3] = MidiAction::ClearLoop(i);
            a[22 + i * 3] = MidiAction::PlayLoop(i);
            a[73 + i * 2] = MidiAction::VolLoop(i);
            a[74 + i * 2] = MidiAction::GainLoop(i);
            a[105 + i] = MidiAction::PanLoop(i);
            i += 1;
        }
        a[68] = MidiAction::Metronome;
        a[69] = MidiAction::SelectNext;
        a[70] = MidiAction::SelectPrev;
        a[71] = MidiAction::PlayAll;
        a[72] = MidiAction::StopAll;
        a
    };

    /// Position of `a` in [`MidiAction::ALL`] (binding-table index).
    pub fn idx(a: MidiAction) -> usize {
        Self::ALL.iter().position(|x| *x == a).unwrap_or(0)
    }

    /// Short label for the settings UI.
    pub fn label(self) -> String {
        match self {
            MidiAction::RecSelected => "record (selected)".into(),
            MidiAction::ClearSelected => "clear (selected)".into(),
            MidiAction::PlaySelected => "play / stop (selected)".into(),
            MidiAction::Export => "export".into(),
            MidiAction::Select(i) => format!("select track {}", i + 1),
            MidiAction::RecLoop(i) => format!("track {}: record", i + 1),
            MidiAction::ClearLoop(i) => format!("track {}: clear", i + 1),
            MidiAction::PlayLoop(i) => format!("track {}: play / stop", i + 1),
            MidiAction::Metronome => "metronome on / off".into(),
            MidiAction::SelectNext => "select next track".into(),
            MidiAction::SelectPrev => "select previous track".into(),
            MidiAction::PlayAll => "play all".into(),
            MidiAction::StopAll => "stop all".into(),
            MidiAction::VolLoop(i) => format!("track {}: volume (knob)", i + 1),
            MidiAction::GainLoop(i) => format!("track {}: gain (knob)", i + 1),
            MidiAction::PanLoop(i) => format!("track {}: pan (knob)", i + 1),
            MidiAction::Reserved => String::new(),
        }
    }

    /// True for value-driven actions (volume / gain knobs): they fire with
    /// the message's second data byte and keyboard bindings can never fire
    /// them (a key press carries no value).
    pub fn is_continuous(self) -> bool {
        matches!(self, MidiAction::VolLoop(_) | MidiAction::GainLoop(_) | MidiAction::PanLoop(_))
    }
}

/// Number of bindable actions — `MidiAction::ALL.len()`.
pub const N_ACTIONS: usize = MidiAction::ALL.len();

/// Remap a persisted keyboard-shortcut table onto the v0.2 action layout.
///
/// The table is positional, so a config from an older build arrives with the
/// old layout:
/// Width **140** (v0.1.x: selected-track globals, import, overdub, dependent,
/// undo/redo, a 4-slot per-loop block, vol/gain, pan) maps forward by
/// remembering each surviving action's old slot; actions that no longer exist
/// (overdub, import, dependent, undo, redo) are dropped. Width **25** (early
/// v0.0.x, the 1-4-loop era) maps the few globals it had. Any other width
/// cannot be trusted — the caller starts from the new defaults instead.
///
/// Returns exactly `N_ACTIONS` entries.
pub fn migrate_shortcuts(old: &[Option<egui::Key>]) -> Vec<Option<egui::Key>> {
    let mut out = vec![None; N_ACTIONS];
    match old.len() {
        n if n == N_ACTIONS => return old.to_vec(),
        140 => {
            // (old, new) slot pairs; removed actions have no entry.
            for (o, n) in [(0, 0), (2, 1), (3, 2), (5, 3), (87, 68), (88, 69), (89, 70)] {
                out[n] = old[o];
            }
            for i in 0..16 {
                out[4 + i] = old[6 + i]; // Select
                out[20 + i * 3] = old[22 + i * 4]; // record
                out[21 + i * 3] = old[24 + i * 4]; // clear
                out[22 + i * 3] = old[25 + i * 4]; // play
                out[73 + i * 2] = old[92 + i * 2]; // volume knob
                out[74 + i * 2] = old[93 + i * 2]; // gain knob
                out[105 + i] = old[124 + i]; // pan knob
            }
        }
        25 => {
            for (o, n) in [(0, 4), (1, 5), (2, 6), (3, 7), (4, 0), (6, 1), (7, 2)] {
                out[n] = old[o];
            }
            for i in 0..4 {
                out[20 + i * 3] = old[8 + i * 4]; // record
                out[21 + i * 3] = old[10 + i * 4]; // clear
                out[22 + i * 3] = old[11 + i * 4]; // play
            }
        }
        _ => return vec![None; N_ACTIONS], // unknown layout: caller applies defaults
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_on_off() {
        let m = MidiMsg::parse(&[0x90, 60, 100]).unwrap();
        assert_eq!(
            m,
            MidiMsg::NoteOn { channel: 0, note: 60, velocity: 100 }
        );
        // Note-on with velocity 0 is a note-off.
        let m = MidiMsg::parse(&[0x93, 64, 0]).unwrap();
        assert_eq!(
            m,
            MidiMsg::NoteOff { channel: 3, note: 64, velocity: 0 }
        );
    }

    #[test]
    fn cc_and_pc() {
        let m = MidiMsg::parse(&[0xB2, 7, 128]).unwrap();
        assert_eq!(
            m,
            MidiMsg::ControlChange { channel: 2, number: 7, value: 128 }
        );
        let m = MidiMsg::parse(&[0xC4, 11]).unwrap();
        assert_eq!(m, MidiMsg::ProgramChange { channel: 4, program: 11 });
    }

    #[test]
    fn pitch_bend_14bit() {
        // Centered: raw 0x2000 -> 0.
        let m = MidiMsg::parse(&[0xE0, 0x00, 0x40]).unwrap();
        assert_eq!(m, MidiMsg::PitchBend { channel: 0, value: 0 });
        // Max positive: raw 0x7FFF -> +8191.
        let m = MidiMsg::parse(&[0xE0, 0x7F, 0x7F]).unwrap();
        assert_eq!(m, MidiMsg::PitchBend { channel: 0, value: 8191 });
        // One below center: raw 0x1FFF -> -1.
        let m = MidiMsg::parse(&[0xE0, 0xFF, 0x3F]).unwrap();
        assert_eq!(m, MidiMsg::PitchBend { channel: 0, value: -1 });
        // Min: raw 0x0000 -> -8192.
        let m = MidiMsg::parse(&[0xE0, 0x00, 0x00]).unwrap();
        assert_eq!(m, MidiMsg::PitchBend { channel: 0, value: -8192 });
    }

    #[test]
    fn system() {
        assert_eq!(MidiMsg::parse(&[0xF8]), Some(MidiMsg::SystemRealtime(0xF8)));
        let m = MidiMsg::parse(&[0xF0, 0x7F, 0x09, 0x01, 0xF7]).unwrap();
        assert!(matches!(m, MidiMsg::SystemCommon { .. }));
    }

    #[test]
    fn note_names() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(0), "C-1");
        assert_eq!(note_name(127), "G9");
    }

    #[test]
    fn scale_axis_full_range_is_identity() {
        for v in 0..=127u8 {
            assert_eq!(scale_axis(v, 0, 127), v);
        }
    }

    #[test]
    fn scale_axis_stretches_partial_travel() {
        // A slider wired 9..=110 still spans the program's whole range.
        assert_eq!(scale_axis(9, 9, 110), 0);
        assert_eq!(scale_axis(110, 9, 110), 127);
        let mid = scale_axis(60, 9, 110);
        assert!((mid as i16 - 64).abs() <= 1, "midpoint got {mid}");
        // Out-of-travel values clamp, they never wrap or go negative.
        assert_eq!(scale_axis(0, 9, 110), 0);
        assert_eq!(scale_axis(127, 9, 110), 127);
    }

    #[test]
    fn scale_axis_handles_inverted_wiring() {
        // Min position sending the HIGHER number just inverts the axis.
        assert_eq!(scale_axis(110, 110, 9), 0);
        assert_eq!(scale_axis(9, 110, 9), 127);
    }

    #[test]
    fn scale_axis_degenerate_passthrough() {
        assert_eq!(scale_axis(64, 42, 42), 64);
    }

    #[test]
    fn midi_bind() {
        // NOTE ON: note number binds, velocity is ignored.
        let note = MidiMsg::parse(&[0x90, 60, 100]).unwrap();
        let b = MidiBind::from_msg(&note).unwrap();
        assert_eq!(b, MidiBind { ty: 0, ch: 0, d1: 60 });
        assert!(b.matches(&note));
        assert!(b.matches(&MidiMsg::parse(&[0x90, 60, 3]).unwrap())); // same note, new vel
        assert!(!b.matches(&MidiMsg::parse(&[0x91, 60, 3]).unwrap())); // other channel
        assert!(!b.matches(&MidiMsg::parse(&[0x90, 61, 3]).unwrap())); // other note

        // CC: controller number binds, value is ignored (pedals/knobs).
        let cc = MidiMsg::parse(&[0xB1, 7, 128]).unwrap();
        let bcc = MidiBind::from_msg(&cc).unwrap();
        assert_eq!(bcc, MidiBind { ty: 1, ch: 1, d1: 7 });
        assert!(bcc.matches(&MidiMsg::parse(&[0xB1, 7, 0]).unwrap())); // any value
        assert!(!bcc.matches(&MidiMsg::parse(&[0xB2, 7, 0]).unwrap())); // other channel
        assert!(!bcc.matches(&note)); // different type

        // PC: program binds.
        let pc = MidiMsg::parse(&[0xC2, 11]).unwrap();
        let bpc = MidiBind::from_msg(&pc).unwrap();
        assert_eq!(bpc, MidiBind { ty: 2, ch: 2, d1: 11 });
        assert!(bpc.matches(&pc));

        // Non-bindable types.
        assert!(MidiBind::from_msg(&MidiMsg::parse(&[0xE0, 0x00, 0x40]).unwrap()).is_none());
        let off = MidiMsg::parse(&[0x80, 60, 0]).unwrap();
        assert!(MidiBind::from_msg(&off).is_none());

        // Labels carry the essentials for the settings UI.
        assert!(bcc.label().contains("CC 7"));
        assert!(bpc.label().contains("PC 11"));
    }
}
