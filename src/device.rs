//! Source-independent device model, and the tracker that merges scans over time.

use std::collections::HashSet;

/// Also the display order on the taskbar.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Kind {
    Headset,
    Earbuds,
    Mouse,
    Keyboard,
    Gamepad,
    Other,
}

impl Kind {
    pub fn from_name(name: &str) -> Kind {
        let n = name.to_ascii_lowercase();
        if ["headset", "headphone", "airpods max"].iter().any(|k| n.contains(k)) {
            Kind::Headset
        } else if ["earbud", "buds", "airpods", "earphone", "in-ear"].iter().any(|k| n.contains(k)) {
            Kind::Earbuds
        } else if n.contains("mouse") {
            Kind::Mouse
        } else if n.contains("keyboard") || n.contains("keys") {
            Kind::Keyboard
        } else if n.contains("controller") || n.contains("gamepad") {
            Kind::Gamepad
        } else {
            Kind::Other
        }
    }
}

/// Whether a native source (not HeadsetControl) already reports the device on USB product id
/// `pid`, so HeadsetControl doesn't list it twice. Native ids are `<source>:<pid hex>[:…]`.
pub fn read_natively(observations: &[Observation], pid: u16) -> bool {
    let pid = format!("{pid:04x}");
    observations.iter().any(|o| {
        let mut parts = o.id.split(':');
        matches!(parts.next(), Some("logi" | "mchose" | "corsair")) && parts.next() == Some(pid.as_str())
    })
}

/// What a source saw for one device during one scan.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Battery { percent: u8, charging: bool },
    /// Known to exist but didn't answer (asleep, or off; often indistinguishable).
    Unreachable,
    /// Definitely not connected (e.g. headset off while its dongle is plugged).
    Off,
}

#[derive(Clone)]
pub struct Observation {
    /// Stable per device across scans, e.g. "logi:c539:2:01" or "ble:f4ee25fbba46".
    pub id: String,
    /// None when the source can't identify the device right now (only updates known devices).
    pub name: Option<String>,
    pub kind: Option<Kind>,
    pub state: State,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status {
    /// `stale`: last known reading of a device that's asleep right now.
    Battery { percent: u8, charging: bool, stale: bool },
    NotConnected,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Battery { percent, charging, stale } => {
                write!(f, "{percent}%")?;
                if *charging {
                    write!(f, " (charging)")?;
                }
                if *stale {
                    write!(f, " (asleep)")?;
                }
                Ok(())
            }
            Status::NotConnected => write!(f, "not connected"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Reading {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub status: Status,
}

/// Remembers every device seen this session, so a device that disappears shows as
/// "not connected" and a sleeping one keeps its last battery reading.
#[derive(Default)]
pub struct Tracker {
    known: Vec<Reading>,
}

impl Tracker {
    pub fn update(&mut self, observations: Vec<Observation>) -> Vec<Reading> {
        let mut seen = HashSet::new();
        for o in observations {
            seen.insert(o.id.clone());
            let pos = self.known.iter().position(|r| r.id == o.id);
            let status = match (o.state, pos.map(|p| self.known[p].status)) {
                (State::Battery { percent, charging }, _) => Status::Battery { percent, charging, stale: false },
                // Asleep: battery hasn't changed, keep the last reading but mark it.
                (State::Unreachable, Some(Status::Battery { percent, charging, .. })) => {
                    Status::Battery { percent, charging, stale: true }
                }
                (State::Unreachable | State::Off, _) => Status::NotConnected,
            };
            match pos {
                Some(p) => {
                    let r = &mut self.known[p];
                    r.status = status;
                    if let Some(name) = o.name {
                        r.name = name;
                    }
                    if let Some(kind) = o.kind {
                        r.kind = kind;
                    }
                }
                None => {
                    let Some(name) = o.name else { continue };
                    let kind = o.kind.unwrap_or_else(|| Kind::from_name(&name));
                    self.known.push(Reading { id: o.id, name, kind, status });
                }
            }
        }
        for r in &mut self.known {
            if !seen.contains(&r.id) {
                r.status = Status::NotConnected;
            }
        }
        // A device that changed connection mode enumerates under a new id and leaves the old
        // row behind (e.g. a mouse over its cable, then over its dongle). Drop the offline row
        // while a live one with the same name and kind exists: they can't be told apart anyway.
        let live: Vec<(String, Kind)> = self
            .known
            .iter()
            .filter(|r| r.status != Status::NotConnected)
            .map(|r| (r.name.clone(), r.kind))
            .collect();
        self.known.retain(|r| r.status != Status::NotConnected || !live.contains(&(r.name.clone(), r.kind)));
        self.known.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
        self.known.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(state: State) -> Observation {
        Observation { id: "x".into(), name: Some("Mouse".into()), kind: Some(Kind::Mouse), state }
    }

    #[test]
    fn read_natively_matches_source_and_pid() {
        let with_id = |id: &str| Observation { id: id.into(), ..obs(State::Off) };
        let o = [with_id("logi:0ab5:3:ff"), with_id("mchose:385d"), with_id("corsair:0a97"), with_id("hc:1b1c:2a08")];
        assert!(read_natively(&o, 0x0AB5));
        assert!(read_natively(&o, 0x385D));
        assert!(read_natively(&o, 0x0A97));
        assert!(!read_natively(&o, 0x2A08)); // only HeadsetControl has it
        assert!(!read_natively(&o, 0x0AB6));
    }

    #[test]
    fn kind_from_name() {
        assert_eq!(Kind::from_name("Renan's AirPods"), Kind::Earbuds);
        assert_eq!(Kind::from_name("AirPods Max"), Kind::Headset);
        assert_eq!(Kind::from_name("Galaxy Buds2 Pro"), Kind::Earbuds);
        assert_eq!(Kind::from_name("PRO X Wireless Gaming Headset"), Kind::Headset);
        assert_eq!(Kind::from_name("MX Master 3S Mouse"), Kind::Mouse);
    }

    #[test]
    fn sleeping_device_keeps_last_reading_dimmed() {
        let mut t = Tracker::default();
        t.update(vec![obs(State::Battery { percent: 80, charging: false })]);
        let r = t.update(vec![obs(State::Unreachable)]);
        assert_eq!(r[0].status, Status::Battery { percent: 80, charging: false, stale: true });
    }

    #[test]
    fn vanished_device_is_not_connected() {
        let mut t = Tracker::default();
        t.update(vec![obs(State::Battery { percent: 80, charging: false })]);
        assert_eq!(t.update(vec![])[0].status, Status::NotConnected);
    }

    #[test]
    fn unknown_unreachable_device_is_ignored() {
        let mut t = Tracker::default();
        let mut o = obs(State::Unreachable);
        o.name = None;
        assert!(t.update(vec![o]).is_empty());
    }

    fn mouse(id: &str, percent: u8, charging: bool) -> Observation {
        Observation {
            id: id.into(),
            name: Some("Wireless mouse".into()),
            kind: Some(Kind::Mouse),
            state: State::Battery { percent, charging },
        }
    }

    #[test]
    fn offline_duplicate_is_dropped_when_a_live_one_appears() {
        // Cable unplugged, same mouse now on its dongle: a different id for the same device.
        let mut t = Tracker::default();
        t.update(vec![mouse("areson:fa7b", 95, true)]);
        let r = t.update(vec![mouse("areson:fa7c", 100, false)]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].id, "areson:fa7c");
    }

    #[test]
    fn two_live_devices_with_the_same_name_are_kept() {
        let mut t = Tracker::default();
        let r = t.update(vec![mouse("a", 80, false), mouse("b", 90, false)]);
        assert_eq!(r.len(), 2);
    }
}
