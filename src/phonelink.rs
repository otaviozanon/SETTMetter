//! Microsoft Phone Link battery (the Windows "cross-device" Phone Link app, not BLE).
//!
//! Phone Link stores a localized Adaptive Card in its package folder; the card carries the phone
//! connection status and battery information. Only English and Japanese connection messages and
//! battery tooltips are recognized. Fields are read from their owning card object (never by
//! searching the whole document), so notification text or action payloads cannot be mistaken for a
//! battery reading. Malformed or ambiguous cards are rejected.
//!
//! The file is an undocumented Phone Link implementation detail and read-only. A card that has not
//! been rewritten for three hours is reported dimmed, as a last known value. Ported from
//! phonelink.cpp / phonelink_protocol.h.

use crate::device::{Kind, Observation, State};
use crate::diag;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const CARD: &str = r"Packages\Microsoft.YourPhone_8wekyb3d8bbwe\LocalState\StartMenu\StartMenuCompanion.json";
const STALE: Duration = Duration::from_secs(3 * 60 * 60);
const CONNECTED_EN: &str = " - Connected to your PC";
const CONNECTED_JA: &str = " - PC に接続しました";
const BATTERY_EN: &str = "Battery ";
const BATTERY_JA: &str = "バッテリー 残量 ";
/// Only these card fields are traversed: never actions, metadata or notification payloads.
const FIELDS: [&str; 3] = ["body", "items", "columns"];
const TYPES: [&str; 6] = ["AdaptiveCard", "Container", "ColumnSet", "Column", "TextBlock", "Image"];

#[derive(Default)]
pub struct PhoneLink;

impl PhoneLink {
    pub fn scan(&mut self) -> Vec<Observation> {
        let Some(dir) = std::env::var_os("LOCALAPPDATA") else { return Vec::new() };
        let path = PathBuf::from(dir).join(CARD);
        let Ok(meta) = std::fs::metadata(&path) else { return Vec::new() };
        let Ok(json) = std::fs::read_to_string(&path) else { return Vec::new() };
        let Some(reading) = parse(&json) else {
            diag::event("phonelink", "card has no unambiguous connected-phone battery");
            return Vec::new();
        };
        let age = meta.modified().ok().and_then(|t| SystemTime::now().duration_since(t).ok()).unwrap_or_default();
        let state = if age > STALE {
            State::Unreachable // the card wasn't rewritten: show it dimmed, as last known
        } else {
            State::Battery { percent: reading.percent, charging: reading.charging }
        };
        vec![Observation { id: "phonelink".into(), name: Some(reading.name), kind: Some(Kind::Other), state }]
    }
}

struct Reading {
    name: String,
    percent: u8,
    charging: bool,
}

fn parse(json: &str) -> Option<Reading> {
    let root: Value = serde_json::from_str(json).ok()?;
    let speak = root.get("speak")?.as_str()?;
    let name = connected_name(speak)?;
    let japanese = speak.ends_with(CONNECTED_JA);
    let mut batteries = Batteries::default();
    walk(&root, japanese, &mut batteries);
    if batteries.ambiguous {
        return None;
    }
    Some(Reading { name, percent: batteries.percent?, charging: batteries.charging })
}

#[derive(Default)]
struct Batteries {
    percent: Option<u8>,
    charging: bool,
    ambiguous: bool,
}

fn connected_name(speak: &str) -> Option<String> {
    for suffix in [CONNECTED_EN, CONNECTED_JA] {
        if let Some(name) = speak.strip_suffix(suffix) {
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn walk(value: &Value, japanese: bool, out: &mut Batteries) {
    if let Some(items) = value.as_array() {
        for item in items {
            walk(item, japanese, out);
        }
        return;
    }
    let Some(object) = value.as_object() else { return };
    if let Some(kind) = object.get("type").and_then(Value::as_str) {
        if !TYPES.contains(&kind) {
            return;
        }
    }
    if let Some(tooltip) = object.get("tooltip").and_then(Value::as_str) {
        let is_japanese = tooltip.starts_with(BATTERY_JA);
        if is_japanese == japanese {
            let mut charging = false;
            if let Some(percent) = battery_from_tooltip(tooltip, &mut charging) {
                let text_matches = object.get("text").and_then(Value::as_str).is_none_or(|text| text == format!("{percent}%"));
                if text_matches {
                    let conflicting = out.percent.is_some_and(|p| p != percent || out.charging != charging);
                    out.ambiguous |= conflicting;
                    out.percent = Some(percent);
                    out.charging = charging;
                }
            }
        }
    }
    for field in FIELDS {
        if let Some(child) = object.get(field) {
            walk(child, japanese, out);
        }
    }
}

fn battery_from_tooltip(tooltip: &str, charging: &mut bool) -> Option<u8> {
    let (rest, japanese) = if let Some(rest) = tooltip.strip_prefix(BATTERY_EN) {
        (rest, false)
    } else if let Some(rest) = tooltip.strip_prefix(BATTERY_JA) {
        (rest, true)
    } else {
        return None;
    };
    let digits = rest.bytes().take(3).take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let percent: u8 = rest[..digits].parse().ok()?;
    if percent > 100 {
        return None;
    }
    let remainder = rest[digits..].strip_prefix('%')?;
    if japanese {
        if !remainder.is_empty() {
            return None;
        }
        *charging = false; // charging state not established for Japanese cards
    } else {
        *charging = match remainder {
            " remaining" => false,
            " remaining, charging" => true,
            _ => return None,
        };
    }
    Some(percent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_card_with_battery() {
        let json = r#"{"speak":"Pixel 8 - Connected to your PC","body":[{"type":"TextBlock","tooltip":"Battery 80% remaining, charging","text":"80%"}]}"#;
        let r = parse(json).expect("card parses");
        assert_eq!((r.name.as_str(), r.percent, r.charging), ("Pixel 8", 80, true));
    }

    #[test]
    fn japanese_card_with_battery() {
        let json = r#"{"speak":"Pixel 8 - PC に接続しました","body":[{"type":"Container","items":[{"type":"TextBlock","tooltip":"バッテリー 残量 65%"}]}]}"#;
        let r = parse(json).expect("card parses");
        assert_eq!((r.name.as_str(), r.percent, r.charging), ("Pixel 8", 65, false));
    }

    #[test]
    fn rejects_disconnected_ambiguous_or_malformed_cards() {
        // Not connected to this PC.
        assert!(parse(r#"{"speak":"Pixel 8","body":[{"type":"TextBlock","tooltip":"Battery 80% remaining"}]}"#).is_none());
        // Two different readings: reject rather than guess.
        let json = r#"{"speak":"Pixel 8 - Connected to your PC","body":[{"type":"TextBlock","tooltip":"Battery 80% remaining"},{"type":"TextBlock","tooltip":"Battery 40% remaining"}]}"#;
        assert!(parse(json).is_none());
        // Connected, but no battery field.
        assert!(parse(r#"{"speak":"Pixel 8 - Connected to your PC","body":[]}"#).is_none());
        // Tooltip only counts on card layout objects, not on an action payload.
        assert!(parse(r#"{"speak":"Pixel 8 - Connected to your PC","actions":[{"tooltip":"Battery 80% remaining"}]}"#).is_none());
        assert!(parse("{not json").is_none());
    }

    #[test]
    fn tooltip_variants() {
        let mut charging = false;
        assert_eq!(battery_from_tooltip("Battery 100% remaining", &mut charging), Some(100));
        assert!(!charging);
        assert_eq!(battery_from_tooltip("Battery 7% remaining, charging", &mut charging), Some(7));
        assert!(charging);
        // A different locale, a missing percent sign, or an unexpected suffix is not a reading.
        assert_eq!(battery_from_tooltip("Akku 80% verbleibend", &mut charging), None);
        assert_eq!(battery_from_tooltip("Battery 80", &mut charging), None);
        assert_eq!(battery_from_tooltip("Battery 80% left", &mut charging), None);
        assert_eq!(battery_from_tooltip("Battery 101% remaining", &mut charging), None);
    }
}
