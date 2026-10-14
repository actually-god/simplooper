//! Port-list filtering: hide audio ports that belong to *disabled* devices.
//!
//! A device counts as enabled when its PulseAudio/PipeWire card has an active
//! profile that is not `off` (i.e. it has not been switched off in the system
//! audio settings). The mapping chain is:
//!
//! ```text
//! JACK port "client:port"
//!   -> PipeWire node  (node.description == client name)
//!   -> card           (node's device.id, or the card key embedded in the
//!                      node name, e.g. alsa_output.usb-MOTU_M2...sink)
//!   -> enabled?       (pactl: Active Profile != "off")
//! ```
//!
//! Nodes without a card (Easy Effects, level meters, spectrum, other JACK
//! clients, ...) are always shown. If any lookup step fails (no `pactl` or
//! `pw-cli`, non-Pulse backend, ...), filtering is skipped entirely and every
//! port is visible — the filter must never hide ports it can't reason about.

use std::collections::{HashMap, HashSet};
use std::process::Command;

/// JACK client names (PipeWire node descriptions) that belong to disabled
/// cards — i.e. should be hidden from the input/output pickers.
///
/// `None` means "cannot determine device state" → show everything.
pub fn hidden_clients() -> Option<HashSet<String>> {
    let enabled = enabled_cards()?;
    let devices = pw_devices()?; // device.id -> card name
    let nodes = pw_nodes()?; // node.description -> (device.id, node.name)
    Some(compute_hidden(&enabled, &devices, &nodes))
}

/// Pure decision core (unit-testable without shelling out): which client
/// descriptions to hide given the enabled cards, device table and node list.
pub fn compute_hidden(
    enabled: &HashSet<String>,
    devices: &HashMap<u32, String>,
    nodes: &[(String, Option<u32>, String)],
) -> HashSet<String> {
    let mut hidden = HashSet::new();
    for (desc, dev_id, node_name) in nodes {
        // Card name via the device table first; fall back to the card key
        // embedded in the node name (`alsa_card.<key>` — the same form pactl
        // reports).
        let card = dev_id.and_then(|id| devices.get(&id).cloned()).or_else(|| {
            card_key_from_node(node_name).map(|k| format!("alsa_card.{k}"))
        });
        if let Some(card) = card {
            if !enabled.contains(&card) {
                hidden.insert(desc.clone());
            }
        }
    }
    hidden
}

/// Card names (`alsa_card.xxx`) whose active profile is not `off`.
pub fn enabled_cards() -> Option<HashSet<String>> {
    let text = run("pactl", &["list", "cards"])?;
    let mut out = HashSet::new();
    for (name, on) in parse_pactl_cards(&text) {
        if on {
            out.insert(name);
        }
    }
    Some(out)
}

/// Parse `pactl list cards` text into (card name -> enabled).
pub fn parse_pactl_cards(text: &str) -> Vec<(String, bool)> {
    let mut current = None;
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(name) = t.strip_prefix("Name: ") {
            current = Some(name.trim().to_string());
        } else if let Some(profile) = t.strip_prefix("Active Profile: ") {
            let on = profile.trim() != "off";
            out.push((current.clone().unwrap_or_default(), on));
        }
    }
    out.retain(|(n, _)| !n.is_empty());
    out
}

/// Parse `pw-cli list-objects Device` text into (device id -> card name).
pub fn pw_devices() -> Option<HashMap<u32, String>> {
    let text = run("pw-cli", &["list-objects", "Device"])?;
    Some(parse_pw_devices(&text))
}

pub fn parse_pw_devices(text: &str) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    for block in split_blocks(text) {
        let id = match find_key(&block, "object.serial").and_then(|v| v.parse().ok()) {
            Some(i) => i,
            None => continue,
        };
        if let Some(name) = find_key(&block, "device.name") {
            out.insert(id, name);
        }
    }
    out
}

/// Parse `pw-cli list-objects Node` text into (node.description ->
/// (device id, node.name)) for audio nodes.
pub fn pw_nodes() -> Option<Vec<(String, Option<u32>, String)>> {
    let text = run("pw-cli", &["list-objects", "Node"])?;
    Some(parse_pw_nodes(&text))
}

pub fn parse_pw_nodes(text: &str) -> Vec<(String, Option<u32>, String)> {
    let mut out = Vec::new();
    for block in split_blocks(text) {
        let name = match find_key(&block, "node.name") {
            Some(n) => n,
            None => continue,
        };
        let cls = find_key(&block, "media.class").unwrap_or_default();
        if !cls.contains("Audio") {
            continue;
        }
        let desc = find_key(&block, "node.description").unwrap_or(name.clone());
        let dev = find_key(&block, "device.id").and_then(|v| v.parse().ok());
        out.push((desc, dev, name));
    }
    out
}

/// The card key embedded in a PipeWire-ALSA node name, if any:
/// `alsa_output.usb-MOTU_M2_...-00.HiFi__Line__sink` -> `usb-MOTU_M2_...-00`.
pub fn card_key_from_node(node_name: &str) -> Option<String> {
    let rest = node_name.strip_prefix("alsa_")?;
    let key = rest.split('.').nth(1)?;
    // `hw_*` names are raw hardware-profile nodes, not card keys — their card
    // mapping must come from the device table, never from the name.
    if key.is_empty() || key.starts_with("hw_") {
        None
    } else {
        Some(key.to_string())
    }
}

fn split_blocks(text: &str) -> Vec<String> {
    // Each object starts with a line like: `\tid 63, type PipeWire:...`
    let mut blocks = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with("id ") && line.contains(", type ") {
            if !cur.is_empty() {
                blocks.push(std::mem::take(&mut cur));
            }
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }
    blocks
}

fn find_key(block: &str, key: &str) -> Option<String> {
    for line in block.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix(&format!("{key} = \"")) {
            return Some(v.strip_suffix('"').unwrap_or(v).to_string());
        }
    }
    None
}

fn run(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(prog).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACTL: &str = r#"Card #49
	Name: alsa_card.usb-MOTU_M2_M2MA08B8XA-00
	Active Profile: HiFi (Line, Mic1)

Card #50
	Name: alsa_card.pci-0000_12_00.6
	Active Profile: output:iec958-stereo+input:analog-stereo

Card #46
	Name: alsa_card.pci-0000_03_00.1
	Active Profile: off
"#;

    #[test]
    fn pactl_parse() {
        let cards = parse_pactl_cards(PACTL);
        assert_eq!(cards.len(), 3);
        let map: HashMap<_, _> = cards.into_iter().collect();
        assert!(map["alsa_card.usb-MOTU_M2_M2MA08B8XA-00"]);
        assert!(map["alsa_card.pci-0000_12_00.6"]);
        assert!(!map["alsa_card.pci-0000_03_00.1"]);
    }

    const DEVICES: &str = r#"	id 49, type PipeWire:Interface:Device/3
 		object.serial = "49"
 		device.api = "alsa"
 		device.name = "alsa_card.usb-MOTU_M2_M2MA08B8XA-00"
		id 50, type PipeWire:Interface:Device/3
 		object.serial = "50"
 		device.name = "alsa_card.pci-0000_12_00.6"
"#;

    #[test]
    fn devices_parse() {
        let d = parse_pw_devices(DEVICES);
        assert_eq!(d.get(&49).map(String::as_str), Some("alsa_card.usb-MOTU_M2_M2MA08B8XA-00"));
        assert_eq!(d.get(&50).map(String::as_str), Some("alsa_card.pci-0000_12_00.6"));
    }

    const NODES: &str = r#"	id 61, type PipeWire:Interface:Node/3
 		object.serial = "61"
 		device.id = "49"
 		node.description = "M Series Mic In 1L"
 		node.name = "alsa_input.usb-MOTU_M2_M2MA08B8XA-00.HiFi__Mic1__source"
 		media.class = "Audio/Source"
	id 62, type PipeWire:Interface:Node/3
 		object.serial = "62"
 		node.description = "Easy Effects Sink"
 		node.name = "easyeffects_sink"
 		media.class = "Audio/Sink"
	id 63, type PipeWire:Interface:Node/3
 		object.serial = "63"
 		device.id = "49"
 		node.description = "M Series 0"
 		node.name = "alsa_input.hw_M2_0"
 		media.class = "Audio/Source/Internal"
	id 64, type PipeWire:Interface:Node/3
 		object.serial = "64"
 		node.description = "BLE MIDI 1"
 		node.name = "bluez_midi.server"
 		media.class = "Midi/Bridge"
"#;

    #[test]
    fn nodes_parse() {
        let n = parse_pw_nodes(NODES);
        // MIDI node excluded, 3 audio nodes.
        assert_eq!(n.len(), 3);
        let by_desc: HashMap<_, _> = n.iter().map(|(d, _, _)| (d.as_str(), d)).collect();
        assert!(by_desc.contains_key("M Series Mic In 1L"));
        assert!(by_desc.contains_key("Easy Effects Sink"));
    }

    #[test]
    fn node_card_keys() {
        assert_eq!(
            card_key_from_node("alsa_output.usb-MOTU_M2_X-00.HiFi__Line__sink").as_deref(),
            Some("usb-MOTU_M2_X-00")
        );
        assert_eq!(card_key_from_node("easyeffects_sink"), None);
        assert_eq!(card_key_from_node("alsa_input.hw_M2_0"), None);
    }

    #[test]
    fn hidden_logic() {
        let nodes = parse_pw_nodes(NODES);
        let devices = parse_pw_devices(DEVICES);
        // Card 49 (M2) enabled, card 50 disabled.
        let mut enabled = HashSet::new();
        enabled.insert("alsa_card.usb-MOTU_M2_M2MA08B8XA-00".to_string());
        let hidden = compute_hidden(&enabled, &devices, &nodes);
        // M2 nodes (device 49) visible; virtuals have no card → visible.
        assert!(!hidden.contains("M Series Mic In 1L"));
        assert!(!hidden.contains("M Series 0"));
        assert!(!hidden.contains("Easy Effects Sink"));
        assert!(hidden.is_empty());

        // Now disable the M2 card too: its nodes must be hidden, virtuals not.
        let empty: HashSet<String> = HashSet::new();
        let hidden = compute_hidden(&empty, &devices, &nodes);
        assert!(hidden.contains("M Series Mic In 1L"));
        assert!(hidden.contains("M Series 0"));
        assert!(!hidden.contains("Easy Effects Sink"));
    }
}
