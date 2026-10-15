//! MIDI message parsing and display formatting for simplooper.
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
}

/// A bindable *action* a MIDI trigger can fire. Mirrors the keyboard
/// shortcuts (selected-track actions) plus direct per-loop actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MidiAction {
    /// Select track `i` (0-based).
    Select(usize),
    /// Record / overdub / clear / play-stop the *selected* track.
    RecSelected,
    OdSelected,
    ClearSelected,
    PlaySelected,
    /// Direct per-track actions (independent of selection).
    RecLoop(usize),
    OdLoop(usize),
    ClearLoop(usize),
    PlayLoop(usize),
    /// Toggle dependent (ensemble) mode.
    Dependent,
}

impl MidiAction {
    /// Every bindable action, in UI order. `ALL[i]` is the action stored at
    /// index `i` of the binding table.
    pub const ALL: [MidiAction; 25] = [
        MidiAction::Select(0),
        MidiAction::Select(1),
        MidiAction::Select(2),
        MidiAction::Select(3),
        MidiAction::RecSelected,
        MidiAction::OdSelected,
        MidiAction::ClearSelected,
        MidiAction::PlaySelected,
        MidiAction::RecLoop(0),
        MidiAction::OdLoop(0),
        MidiAction::ClearLoop(0),
        MidiAction::PlayLoop(0),
        MidiAction::RecLoop(1),
        MidiAction::OdLoop(1),
        MidiAction::ClearLoop(1),
        MidiAction::PlayLoop(1),
        MidiAction::RecLoop(2),
        MidiAction::OdLoop(2),
        MidiAction::ClearLoop(2),
        MidiAction::PlayLoop(2),
        MidiAction::RecLoop(3),
        MidiAction::OdLoop(3),
        MidiAction::ClearLoop(3),
        MidiAction::PlayLoop(3),
        MidiAction::Dependent,
    ];

    /// Position of `a` in [`MidiAction::ALL`] (binding-table index).
    pub fn idx(a: MidiAction) -> usize {
        Self::ALL.iter().position(|x| *x == a).unwrap_or(0)
    }

    /// Short label for the settings UI.
    pub fn label(self) -> String {
        match self {
            MidiAction::Select(i) => format!("select loop {}", i + 1),
            MidiAction::RecSelected => "record (selected)".into(),
            MidiAction::OdSelected => "overdub (selected)".into(),
            MidiAction::ClearSelected => "clear (selected)".into(),
            MidiAction::PlaySelected => "play / stop (selected)".into(),
            MidiAction::RecLoop(i) => format!("loop {}: record", i + 1),
            MidiAction::OdLoop(i) => format!("loop {}: overdub", i + 1),
            MidiAction::ClearLoop(i) => format!("loop {}: clear", i + 1),
            MidiAction::PlayLoop(i) => format!("loop {}: play / stop", i + 1),
            MidiAction::Dependent => "dependent mode".into(),
        }
    }
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
