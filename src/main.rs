//! Broadcastify — a Trunk Recorder Pro plugin that uploads recorded calls to
//! [Broadcastify Calls](https://www.broadcastify.com/calls/), as Trunk
//! Recorder's Broadcastify uploader does.

mod upload;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use trunk_recorder_plugin::{
    Attempt, CallQueue, ConcludedCall, Host, Manifest, Plugin, QueueOptions, Setup, State, TalkgroupFilter, Talkgroups, format, topic,
};

use upload::{Upload, Uploader};

pub const DEFAULT_SERVER: &str = "https://api.broadcastify.com/call-upload";

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
struct Config {
    /// Upload server
    ///
    /// Leave this as it is, unless Broadcastify tells you to change it.
    #[schemars(url)]
    #[serde(alias = "broadcastifyCallsServer")]
    server: String,
    /// Send talker aliases
    ///
    /// When the first radio on a call sent its name over the air, send that too.
    #[serde(alias = "broadcastifyOTA")]
    talker_aliases: bool,
    /// Skip certificate checks
    ///
    /// Upload even when Broadcastify's security certificate has expired. Leave it off unless uploads fail with a certificate error.
    #[serde(alias = "broadcastifySslVerifyDisable")]
    skip_certificate_check: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { server: DEFAULT_SERVER.into(), talker_aliases: true, skip_certificate_check: false }
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
struct SystemConfig {
    /// API key
    ///
    /// The upload key from your Broadcastify Calls node. Leave it empty to not upload this system.
    #[schemars(extend("x-secret" = true, "x-required" = true))]
    #[serde(alias = "broadcastifyApiKey")]
    api_key: String,
    /// System ID
    ///
    /// The system's number on Broadcastify Calls.
    #[serde(alias = "broadcastifySystemId")]
    #[schemars(extend("x-required" = true))]
    system_id: Option<u32>,
    // The talkgroups to upload: a menu, and the lists it asks for.
    #[serde(flatten)]
    talkgroups: Talkgroups,
    /// Sites of their own
    ///
    /// A site of this system that Broadcastify Calls knows as a system of its own: its calls go there instead.
    sites: Vec<SiteConfig>,
}

/// A site with a Broadcastify Calls system of its own.
#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
struct SiteConfig {
    /// Site
    ///
    /// Its name, as under Sites in Setup.
    site: String,
    /// System ID
    ///
    /// Its number on Broadcastify Calls.
    system_id: Option<u32>,
    /// API key
    ///
    /// Its upload key, when not the system's.
    #[schemars(extend("x-secret" = true))]
    api_key: String,
}

/// Where a system's calls go.
struct Target {
    /// The queue's lane for its server: its host ("" for Broadcastify's own).
    lane: String,
    uploader: Arc<Uploader>,
    /// Send talker aliases.
    aliases: bool,
    system_id: u32,
    api_key: String,
    /// Sites with a Broadcastify Calls system of their own: (system ID, API key), by site name.
    sites: HashMap<String, (u32, String)>,
    filter: TalkgroupFilter,
}

impl Target {
    /// Where a call heard on `site` goes: its own system, else the system's.
    fn of(&self, site: &str) -> (u32, &str) {
        self.sites.get(site).map_or((self.system_id, self.api_key.as_str()), |(id, key)| (*id, key.as_str()))
    }
}

struct Broadcastify {
    queue: CallQueue,
}

/// A server's lane in the queue, and the name it goes by on the dashboard
/// ("Broadcastify Calls at <host>"): Broadcastify's own is just "Broadcastify Calls".
fn lane_of(server: &str) -> String {
    if server == DEFAULT_SERVER {
        return String::new();
    }
    server.split_once("://").map_or(server, |(_, r)| r).split('/').next().unwrap_or("").to_string()
}

impl Plugin for Broadcastify {
    type Config = Config;
    type SystemConfig = SystemConfig;

    fn manifest() -> Manifest {
        Manifest {
            name: "Broadcastify Calls".into(),
            subscribe: vec![topic::CALL_CONCLUDED.into()],
            audio_formats: vec![format::M4A.into()],
            ..trunk_recorder_plugin::manifest!()
        }
    }

    fn start(host: Host, setup: Setup<Config, SystemConfig>) -> Result<Self, String> {
        // Broadcastify takes AAC audio, not WAV.
        if !setup.has_format(format::M4A) {
            return Err("Broadcastify needs calls as M4A, and there's no M4A encoder on this computer. Install ffmpeg, then start recording again.".into());
        }
        // One uploader for each server, and certificate check (a system can override either).
        let mut uploaders: HashMap<(String, bool), Arc<Uploader>> = HashMap::new();
        let mut targets = HashMap::new();
        for s in &setup.systems {
            let Some(c) = &s.config else { continue };
            let api_key = c.api_key.trim().to_string();
            if api_key.is_empty() {
                continue;
            }
            let Some(system_id) = c.system_id.filter(|&id| id != 0) else {
                return Err(format!("Add {}'s Broadcastify system ID (or clear its API key).", s.short_name));
            };
            let server = s.settings.server.trim().to_string();
            if !(server.starts_with("https://") || server.starts_with("http://")) {
                return Err(format!("The upload server has to be a web address (https://…), not \"{server}\""));
            }
            let skip = s.settings.skip_certificate_check;
            if skip {
                host.warn(format!("not checking the certificate of {server} for {}", s.short_name));
            }
            let uploader = uploaders.entry((server.clone(), skip)).or_insert_with(|| Arc::new(Uploader::new(&server, skip))).clone();
            let filter = c.talkgroups.filter_for(&s.short_name)?;
            let filtered = if filter.is_empty() { String::new() } else { format!(", talkgroups: {}", filter.describe()) };
            let lane = lane_of(&server);
            let to = if lane.is_empty() { String::new() } else { format!(" to {lane}") };
            host.info(format!("uploading {}{to} as system {system_id} (key …{}){filtered}", s.short_name, last2(&api_key)));
            let mut sites = HashMap::new();
            for x in &c.sites {
                let site = x.site.trim();
                if site.is_empty() {
                    return Err(format!("Name the site of {} that has a Broadcastify system of its own.", s.short_name));
                }
                let Some(id) = x.system_id.filter(|&id| id != 0) else {
                    return Err(format!("Add the Broadcastify system ID of {}'s site {site}.", s.short_name));
                };
                let key = if x.api_key.trim().is_empty() { api_key.clone() } else { x.api_key.trim().to_string() };
                host.info(format!("uploading {}'s site {site} as system {id} (key …{})", s.short_name, last2(&key)));
                sites.insert(site.to_string(), (id, key));
            }
            targets.insert(s.short_name.clone(), Target { lane, uploader, aliases: s.settings.talker_aliases, system_id, api_key, sites, filter });
        }
        if targets.is_empty() {
            return Err("Add your Broadcastify API key and system ID to the systems you want to upload.".into());
        }
        // Each server is a lane of the queue: one that's down holds up no other.
        let targets = Arc::new(targets);
        let lanes = targets.clone();
        let opts = QueueOptions { noun: "upload", endpoint: Some("Broadcastify Calls".into()), ..QueueOptions::saved_in(&setup.data_dir) };
        let lane = move |call: &ConcludedCall| lanes.get(&call.call.short_name).map(|t| t.lane.clone()).unwrap_or_default();
        // Systems whose settings Broadcastify turned down, said once each.
        let refused = Mutex::new(HashSet::new());
        let status = host.clone();
        let queue = CallQueue::start_by(host, opts, lane, move |call: &ConcludedCall| {
            // By short name, a system's identity: a call saved for a later run still finds its system.
            let Some(t) = targets.get(&call.call.short_name) else {
                return Attempt::Skip("no Broadcastify API key for this system".into());
            };
            if call.call.encrypted {
                return Attempt::Skip("encrypted".into());
            }
            if !t.filter.passes(call.call.talkgroup) {
                return Attempt::Skip(format!("talkgroup {} isn't uploaded (talkgroup filter)", call.call.talkgroup));
            }
            let Some(m4a) = &call.files.m4a else {
                return Attempt::Fail("this call couldn't be encoded as M4A".into());
            };
            let alias = if t.aliases { call.call.src_list.first().map(|s| s.tag_ota.trim()).filter(|a| !a.is_empty()) } else { None };
            let (system_id, api_key) = t.of(&call.call.site);
            match t.uploader.upload(&Upload { system_id, api_key, alias, call, audio: m4a }) {
                Ok(a) => a,
                // Trying again won't help (Trunk Recorder would): the call fails now, and the plugin's status says what to fix.
                Err(r) => {
                    // A site of its own is named so ("dcfd's site south").
                    let who = match t.sites.contains_key(&call.call.site) {
                        true => format!("{}'s site {}", call.call.short_name, call.call.site),
                        false => call.call.short_name.clone(),
                    };
                    let m = r.message(&who, system_id);
                    if refused.lock().unwrap().insert(who) {
                        status.status(State::Error, &m);
                    }
                    Attempt::Fail(m)
                }
            }
        });
        Ok(Broadcastify { queue })
    }

    fn call_concluded(&mut self, call: ConcludedCall) {
        self.queue.push(call);
    }

    fn shutdown(&mut self, grace: Duration) {
        self.queue.shutdown(grace);
    }
}

/// The key's last two characters, to tell keys apart in the log.
fn last2(key: &str) -> String {
    let k: Vec<char> = key.trim().chars().collect();
    k[k.len().saturating_sub(2)..].iter().collect()
}

fn main() {
    trunk_recorder_plugin::run::<Broadcastify>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use trunk_recorder_plugin::testing::{self, MockServer, Request};
    use trunk_recorder_plugin::{EXIT_CONFIG, HostMessage, Outcome, SystemInfo};

    /// Broadcastify as Trunk Recorder's uploader sees it: the metadata POST
    /// answers "0 <where to PUT the audio>" for key "good" and system 42.
    fn broadcastify() -> MockServer {
        MockServer::start(|req: &Request| {
            if req.method == "PUT" {
                return (200, String::new());
            }
            let field = |n: &str| String::from_utf8(req.form_field(n).unwrap_or_default()).unwrap();
            let meta: Value = serde_json::from_str(&field("metadata")).unwrap_or_default();
            match (field("apiKey").as_str(), field("systemId").as_str(), meta["talkgroup"].as_u64()) {
                (_, _, Some(999)) => (200, "1 SKIPPED: talkgroup not monitored".into()),
                ("good", "42", _) => (200, format!("0 http://{}/audio/abc.m4a?sig=1", req.header("host").unwrap())),
                ("good", _, _) => (200, "1 REJECTED: system mismatch".into()),
                _ => (200, "1 Invalid-API-Key".into()),
            }
        })
    }

    fn hello(dir: &std::path::Path, server: &str, system: Value) -> HostMessage {
        let mut h = testing::hello(dir, json!({ "server": format!("{server}/call-upload") }));
        h.systems[0].config = system;
        HostMessage::Hello(h)
    }

    #[test]
    fn uploads_metadata_then_audio_as_trunk_recorder_does() {
        let (dir, server) = (testing::temp_dir("bcfy"), broadcastify());
        let mut call = testing::call(&dir, "sys1", 101);
        call.call.src_list[0].tag_ota = "ENGINE 1".into();
        let out =
            testing::run::<Broadcastify>([hello(&dir, server.url(), json!({ "apiKey": "good", "systemId": 42 })), HostMessage::CallConcluded(call.clone())]);
        assert!(out.ready(), "{:?}", out.messages);
        assert_eq!(out.results(), vec![(call.path.clone(), Outcome::Ok, String::new(), String::new())]);
        let r = server.requests();
        assert_eq!((r[0].method.as_str(), r[0].path.as_str()), ("POST", "/call-upload"));
        let field = |n: &str| String::from_utf8(r[0].form_field(n).unwrap()).unwrap();
        assert_eq!(field("systemId"), "42");
        assert_eq!(field("apiKey"), "good");
        assert_eq!(field("callDuration"), "3.000000");
        assert_eq!(field("srcId_alias"), "ENGINE 1");
        assert_eq!(r[0].form_file_name("metadata").unwrap(), "call_meta.json");
        let meta: Value = serde_json::from_str(&field("metadata")).unwrap();
        assert_eq!(meta["talkgroup"], 101);
        assert_eq!(meta["srcList"][0]["src"], 1234);
        // Then the audio, to where the answer said.
        assert_eq!((r[1].method.as_str(), r[1].path.as_str()), ("PUT", "/audio/abc.m4a?sig=1"));
        assert_eq!(r[1].header("content-type"), Some("audio/aac"));
        assert_eq!(r[1].body, std::fs::read(call.files.m4a.unwrap()).unwrap());
    }

    #[test]
    fn the_timeline_ends_when_the_call_was_concluded() {
        let (dir, server) = (testing::temp_dir("bcfy"), broadcastify());
        let call = testing::call(&dir, "sys1", 101);
        // Concluded (its JSON written) 20 s after the last audio.
        let concluded = std::time::UNIX_EPOCH + Duration::from_secs(call.call.stop_time as u64 + 20);
        std::fs::File::options().write(true).open(&call.files.json).unwrap().set_modified(concluded).unwrap();
        testing::run::<Broadcastify>([hello(&dir, server.url(), json!({ "apiKey": "good", "systemId": 42 })), HostMessage::CallConcluded(call.clone())]);
        let meta: Value = serde_json::from_slice(&server.requests()[0].form_field("metadata").unwrap()).unwrap();
        let stop = call.call.stop_time + 20;
        assert_eq!(meta["stop_time"], stop);
        assert_eq!(meta["stop_time_ms"], stop * 1000);
        assert_eq!(meta["start_time"], stop - 3);
        assert_eq!(meta["freqList"][0]["time"], (stop - 3) as f64);
    }

    #[test]
    fn skipped_and_rejected() {
        let (dir, server) = (testing::temp_dir("bcfy"), broadcastify());
        let out = testing::run::<Broadcastify>([
            hello(&dir, server.url(), json!({ "apiKey": "good", "systemId": 7 })),
            HostMessage::CallConcluded(testing::call(&dir, "sys1", 999)),
            HostMessage::CallConcluded(testing::call(&dir, "sys1", 5)),
        ]);
        let mut r = out.results();
        r.sort_by_key(|x| x.0.clone());
        assert_eq!(r[0].1, Outcome::Failed, "{r:?}");
        assert!(r[0].2.contains("REJECTED"), "{}", r[0].2);
        assert_eq!(r[1].1, Outcome::Skipped);
        assert!(r[1].2.contains("not monitored"));
        // Neither sent audio.
        assert!(server.requests().iter().all(|q| q.method == "POST"));
    }

    #[test]
    fn a_wrong_api_key_fails_at_once_and_says_so() {
        let (dir, server) = (testing::temp_dir("bcfy"), broadcastify());
        let out = testing::run::<Broadcastify>([
            hello(&dir, server.url(), json!({ "apiKey": "typo", "systemId": 42 })),
            HostMessage::CallConcluded(testing::call(&dir, "sys1", 5)),
            HostMessage::CallConcluded(testing::call(&dir, "sys1", 6)),
        ]);
        let r = out.results();
        assert_eq!(r.len(), 2, "{r:?}");
        let refused = "Broadcastify refused the API key for sys1 — check the system's API key";
        assert!(r.iter().all(|x| x.1 == Outcome::Failed && x.2.starts_with(refused)), "{r:?}");
        // Not tried again, nor kept for later.
        assert_eq!(server.requests().len(), 2);
        assert!(!dir.join("data/queue.jsonl").exists());
        let (state, message) = out.status().unwrap();
        assert!(matches!(state, State::Error));
        assert!(message.contains("refused the API key for sys1"), "{message}");
    }

    #[test]
    fn talkgroup_filters_and_trunk_recorders_setting_names() {
        let (dir, server) = (testing::temp_dir("bcfy"), broadcastify());
        let system = json!({ "broadcastifyApiKey": "good", "broadcastifySystemId": 42, "talkgroupMode": "only", "talkgroupAllow": ["10*", 205], "talkgroupDeny": ["109"] });
        let mut h = testing::hello(&dir, json!({ "broadcastifyCallsServer": format!("{}/call-upload", server.url()), "broadcastifyOTA": false }));
        h.systems[0].config = system;
        let out = testing::run::<Broadcastify>([
            HostMessage::Hello(h),
            HostMessage::CallConcluded(testing::call(&dir, "sys1", 101)),
            HostMessage::CallConcluded(testing::call(&dir, "sys1", 109)),
            HostMessage::CallConcluded(testing::call(&dir, "sys1", 205)),
            HostMessage::CallConcluded(testing::call(&dir, "sys1", 300)),
        ]);
        let mut r: Vec<(String, Outcome)> = out.results().into_iter().map(|(p, o, _, _)| (p.split('/').next_back().unwrap()[..3].to_string(), o)).collect();
        r.sort_by_key(|x| x.0.clone());
        assert_eq!(r, [("101".into(), Outcome::Ok), ("109".into(), Outcome::Skipped), ("205".into(), Outcome::Ok), ("300".into(), Outcome::Skipped)]);
        let posts: Vec<_> = server.requests().into_iter().filter(|q| q.method == "POST").collect();
        assert_eq!(posts.len(), 2);
        // Talker aliases off: none sent.
        assert!(posts[0].form_field("srcId_alias").is_none());
    }

    /// The menu: only a range, except one of it; all but some; and an entry that isn't a talkgroup stops it starting.
    #[test]
    fn talkgroups_chosen_in_the_menu() {
        let (dir, server) = (testing::temp_dir("bcfy"), broadcastify());
        let sent = |system: Value, tgs: &[u32]| {
            let mut msgs = vec![hello(&dir, server.url(), system)];
            msgs.extend(tgs.iter().map(|&tg| HostMessage::CallConcluded(testing::call(&dir, "sys1", tg))));
            let out = testing::run::<Broadcastify>(msgs);
            let mut ok: Vec<u32> = out
                .results()
                .iter()
                .filter(|r| r.1 == Outcome::Ok)
                .map(|r| r.0.split('/').next_back().unwrap()[..4].trim_end_matches('-').parse().unwrap())
                .collect();
            ok.sort();
            (ok, out)
        };
        let only = json!({ "apiKey": "good", "systemId": 42, "talkgroupMode": "only", "talkgroupAllow": ["1000 - 1099"], "talkgroupDeny": [1050] });
        let (ok, out) = sent(only, &[1000, 1050, 1099, 2000]);
        assert_eq!(ok, [1000, 1099]);
        assert!(out.logs().iter().any(|l| l.1.contains("talkgroups: only 1000-1099; except 1050")), "{:?}", out.logs());
        let but = json!({ "apiKey": "good", "systemId": 42, "talkgroupMode": "allBut", "talkgroupAllow": ["1000-1099"], "talkgroupDeny": ["20??"] });
        assert_eq!(sent(but, &[1000, 2001, 3000]).0, [1000, 3000], "the hidden allow list doesn't apply");
        let (_, out) = sent(json!({ "apiKey": "good", "systemId": 42, "talkgroupMode": "only", "talkgroupAllow": ["10OO"] }), &[]);
        assert_eq!(out.exit_code, EXIT_CONFIG);
        assert!(out.status().unwrap().1.starts_with("sys1's talkgroups: \"10OO\" isn't a talkgroup"), "{:?}", out.status());
    }

    #[test]
    fn encrypted_calls_are_skipped() {
        let (dir, server) = (testing::temp_dir("bcfy"), broadcastify());
        let mut call = testing::call(&dir, "sys1", 101);
        call.call.encrypted = true;
        let out = testing::run::<Broadcastify>([hello(&dir, server.url(), json!({ "apiKey": "good", "systemId": 42 })), HostMessage::CallConcluded(call)]);
        assert_eq!(out.results()[0].1, Outcome::Skipped);
        assert!(server.requests().is_empty());
    }

    #[test]
    fn a_server_that_is_down_keeps_the_call_for_next_time() {
        let dir = testing::temp_dir("bcfy");
        let out = testing::run::<Broadcastify>([
            hello(&dir, "http://127.0.0.1:9", json!({ "apiKey": "good", "systemId": 42 })),
            HostMessage::CallConcluded(testing::call(&dir, "sys1", 5)),
        ]);
        assert!(out.results().is_empty(), "{:?}", out.results());
        assert!(matches!(out.status(), Some((State::Warning, _))));
        assert_eq!(std::fs::read_to_string(dir.join("data/queue.jsonl")).unwrap().lines().count(), 1);
    }

    #[test]
    fn needs_a_system_id() {
        let dir = testing::temp_dir("bcfy");
        let out = testing::run::<Broadcastify>([hello(&dir, "https://x", json!({ "apiKey": "good" }))]);
        assert_eq!(out.exit_code, EXIT_CONFIG);
        assert!(out.status().unwrap().1.contains("system ID"));
    }

    #[test]
    fn needs_m4a() {
        let dir = testing::temp_dir("bcfy");
        let mut h = testing::hello(&dir, Value::Null);
        h.systems[0].config = json!({ "apiKey": "good", "systemId": 42 });
        h.audio_formats = vec!["wav".into()];
        let out = testing::run::<Broadcastify>([HostMessage::Hello(h)]);
        assert_eq!(out.exit_code, EXIT_CONFIG);
        assert!(out.status().unwrap().1.contains("ffmpeg"));
    }

    #[test]
    fn the_settings_form() {
        let m = trunk_recorder_plugin::describe::<Broadcastify>();
        let s = m.system_config.unwrap();
        assert_eq!(s["properties"]["talkgroupMode"]["enum"], json!(["all", "only", "allBut"]));
        assert_eq!(s["properties"]["talkgroupAllow"]["x-when"], json!({ "talkgroupMode": "only" }));
        assert_eq!(s["x-order"], json!(["apiKey", "systemId", "talkgroupMode", "talkgroupAllow", "talkgroupDeny", "sites"]));
        assert_eq!(s["properties"]["systemId"]["type"], "integer");
        assert_eq!(m.config.unwrap()["properties"]["talkerAliases"]["default"], true);
    }

    /// A site with a Broadcastify Calls system of its own: its calls go there, the system's other sites' to the system's.
    #[test]
    fn a_site_of_its_own() {
        let (dir, server) = (testing::temp_dir("bcfy"), broadcastify());
        let mut h = testing::hello(&dir, json!({ "server": format!("{}/call-upload", server.url()) }));
        h.systems[0].config = json!({ "apiKey": "good", "systemId": 42, "sites": [{ "site": "south", "systemId": 77 }] });
        let mut north = testing::call(&dir, "sys1", 101);
        north.call.site = "north".into();
        let mut south = testing::call(&dir, "sys1", 102);
        south.call.site = "south".into();
        testing::run::<Broadcastify>([HostMessage::Hello(h), HostMessage::CallConcluded(north), HostMessage::CallConcluded(south)]);
        // (Each call's metadata names its system; the order they go in isn't fixed.)
        let sent = |field: &str| {
            let mut v: Vec<String> = server.requests().iter().filter_map(|r| r.form_field(field)).map(|v| String::from_utf8(v).unwrap()).collect();
            v.sort();
            v
        };
        assert_eq!(sent("systemId"), ["42", "77"]);
        assert_eq!(sent("apiKey"), ["good", "good"], "the system's key, the site having none of its own");
        // A site of its own needs its name and system ID.
        let mut h = testing::hello(&dir, json!({}));
        h.systems[0].config = json!({ "apiKey": "good", "systemId": 42, "sites": [{ "site": "south" }] });
        let out = testing::run::<Broadcastify>([HostMessage::Hello(h)]);
        assert!(format!("{out:?}").contains("site south"), "{out:?}");
    }

    /// A system can override the server, and whether talker aliases go.
    #[test]
    fn a_system_overrides_the_server_and_aliases() {
        let (dir, main, other) = (testing::temp_dir("bcfy"), broadcastify(), broadcastify());
        let mut h = testing::hello(&dir, json!({ "server": format!("{}/call-upload", main.url()) }));
        h.systems[0].config = json!({ "apiKey": "good", "systemId": 42 });
        h.systems.push(SystemInfo {
            index: 1,
            short_name: "sys2".into(),
            kind: "p25".into(),
            config: json!({ "apiKey": "good", "systemId": 42 }),
            settings: json!({ "server": format!("{}/call-upload", other.url()), "talkerAliases": false }),
            ..Default::default()
        });
        let mut one = testing::call(&dir, "sys1", 101);
        one.call.src_list[0].tag_ota = "ENGINE 1".into();
        let mut two = testing::call(&dir, "sys2", 202);
        two.call.src_list[0].tag_ota = "ENGINE 2".into();
        let out = testing::run::<Broadcastify>([HostMessage::Hello(h), HostMessage::CallConcluded(one), HostMessage::CallConcluded(two)]);
        assert!(out.results().iter().all(|r| r.1 == Outcome::Ok), "{:?}", out.results());
        let alias = |s: &MockServer| s.requests()[0].form_field("srcId_alias").map(|a| String::from_utf8(a).unwrap());
        assert_eq!(alias(&main).as_deref(), Some("ENGINE 1"));
        assert_eq!(alias(&other), None, "sys2 doesn't send aliases");
        assert_eq!(other.requests().len(), 2, "metadata, then audio");
    }
}
