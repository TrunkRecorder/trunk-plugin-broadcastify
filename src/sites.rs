//! One Broadcastify system ID per site. Broadcastify Calls records every call a node uploads as
//! coming from the site that node is registered to, so a system ID uploading several sites labels
//! the others' calls as the wrong site. The plugin says so (uploads go on as they are): at start,
//! from the settings, and later from the health reports, for sites that appear while recording.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// What to do about it, after what's wrong.
pub const ADVICE: &str = "Broadcastify Calls needs a system ID for each site you upload, or calls from the other sites are labeled as the wrong site. Give each site its own Broadcastify system ID on its row in these settings. If you need more system IDs, apply at https://www.broadcastify.com/apply/calls (you can use your existing API key).";

/// What one system ID uploads: (recorder system, its site; None: a system with no sites).
pub type Covers = Vec<(String, Option<String>)>;

/// "Broadcastify system 77 is uploading 3 sites: north, south, east." Sites of
/// several recorder systems are named with their system: "dcfd (north, south), dcfd2".
pub fn several(id: u32, on: &Covers) -> String {
    let mut systems: Vec<(&str, Vec<&str>)> = Vec::new();
    for (system, site) in on {
        let i = systems.iter().position(|(s, _)| s == system).unwrap_or_else(|| {
            systems.push((system, Vec::new()));
            systems.len() - 1
        });
        systems[i].1.extend(site.as_deref());
    }
    let names: Vec<String> = match systems.as_slice() {
        [(_, sites)] => sites.iter().map(|s| s.to_string()).collect(),
        _ => systems.iter().map(|(s, sites)| if sites.is_empty() { s.to_string() } else { format!("{s} ({})", sites.join(", ")) }).collect(),
    };
    format!("Broadcastify system {id} is uploading {} sites: {}.", on.len(), names.join(", "))
}

/// The system IDs uploading several sites, and what's said of each: shared by the settings check
/// and the health reports, and shown as the plugin's status until recording starts again.
#[derive(Clone, Default)]
pub struct Notices(Arc<Mutex<BTreeMap<u32, String>>>);

impl Notices {
    /// Note that `id` uploads several sites; false if it already was.
    pub fn add(&self, id: u32, what: String) -> bool {
        let mut m = self.0.lock().unwrap();
        if m.contains_key(&id) {
            return false;
        }
        m.insert(id, what);
        true
    }

    /// The plugin's status: every ID's, then what to do (None: nothing to say).
    pub fn status(&self) -> Option<String> {
        let m = self.0.lock().unwrap();
        (!m.is_empty()).then(|| format!("{} {ADVICE}", m.values().cloned().collect::<Vec<_>>().join(" ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(v: &[(&str, Option<&str>)]) -> Covers {
        v.iter().map(|(s, t)| (s.to_string(), t.map(str::to_string))).collect()
    }

    #[test]
    fn what_is_said() {
        let one = on(&[("dcfd", Some("north")), ("dcfd", Some("south")), ("dcfd", Some("east"))]);
        assert_eq!(several(77, &one), "Broadcastify system 77 is uploading 3 sites: north, south, east.");
        let two = on(&[("dcfd", Some("north")), ("dcfd", Some("south")), ("dcfd2", None)]);
        assert_eq!(several(42, &two), "Broadcastify system 42 is uploading 3 sites: dcfd (north, south), dcfd2.");
        let n = Notices::default();
        assert_eq!(n.status(), None);
        assert!(n.add(77, several(77, &one)) && !n.add(77, "again".into()));
        assert!(n.status().unwrap().starts_with("Broadcastify system 77 is uploading 3 sites") && n.status().unwrap().ends_with("existing API key)."));
    }
}
