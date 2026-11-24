//! Broadcastify — a Trunk Recorder Pro plugin that uploads recorded calls to
//! [Broadcastify Calls](https://www.broadcastify.com/calls/), as Trunk
//! Recorder's Broadcastify uploader does.

mod health;
mod sites;
mod upload;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use trunk_recorder_plugin::health::Recorder;
use trunk_recorder_plugin::{
    Attempt, CallQueue, ConcludedCall, Host, Manifest, Plugin, QueueOptions, Setup, State, System, TalkgroupFilter, Talkgroups, Totals, format, topic,
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
    /// Send health reports
    ///
    /// Every 5 minutes, tell Broadcastify how well this recorder hears each system it uploads: whether it's decoding the control channel, the calls it missed, the voice it lost. Totals only: no talkgroups, radio IDs or audio. Leave it on: without reports, Broadcastify can't compare this node with others uploading the same system (it may get a lower priority or be left out), and can't tell you about outages, problems or trends on it.
    health_reports: bool,
    /// Health report server
    ///
    /// Leave it empty: beside the upload server.
    #[schemars(url)]
    health_server: String,
}

impl Default for Config {
    fn default() -> Self {
        Config { server: DEFAULT_SERVER.into(), talker_aliases: true, skip_certificate_check: false, health_reports: true, health_server: String::new() }
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
}

/// Where a system's calls go.
struct Target {
    /// The queue's lane for its server: its host ("" for Broadcastify's own).
    lane: String,
    uploader: Arc<Uploader>,
    /// Send talker aliases.
    aliases: bool,
    filter: TalkgroupFilter,
    /// The system, with its settings and its sites' ([`System::targets`]).
    system: System<Config, SystemConfig>,
}

/// The Broadcastify Calls system that settings send to: its ID and API key (None: they name none).
fn node(c: &SystemConfig) -> Option<(u32, &str)> {
    let key = c.api_key.trim();
    c.system_id.filter(|&id| id != 0 && !key.is_empty()).map(|id| (id, key))
}

struct Broadcastify {
    host: Host,
    queue: CallQueue,
    /// Health reports, one thread for each server they go to.
    reports: Vec<health::Reports>,
    /// System IDs uploading several sites: the plugin's status says so.
    notices: sites::Notices,
    /// Systems whose settings Broadcastify turned down (the status says so; that comes first).
    refused: Arc<Mutex<HashSet<String>>>,
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
            subscribe: vec![topic::CALL_CONCLUDED.into(), topic::TOTALS.into()],
            audio_formats: vec![format::M4A.into()],
            // Each read per system: a system can upload elsewhere, its own way. (Not the certificate
            // check: whether to trust a server is the recorder's decision, made once.)
            system_overrides: vec!["server".into(), "talkerAliases".into()],
            // A site that's a Broadcastify Calls system of its own.
            site_overrides: vec!["apiKey".into(), "systemId".into()],
            ..trunk_recorder_plugin::manifest!()
        }
    }

    fn start(host: Host, setup: Setup<Config, SystemConfig>) -> Result<Self, String> {
        // Broadcastify takes AAC audio, not WAV.
        if !setup.has_format(format::M4A) {
            return Err("Broadcastify needs calls as M4A, and there's no M4A encoder on this computer. Install ffmpeg, then start recording again.".into());
        }
        // One uploader for each server (a system can override it), and certificate check (the same for all).
        let skip = setup.config.skip_certificate_check;
        let mut uploaders: HashMap<(String, bool), Arc<Uploader>> = HashMap::new();
        // By short name, and the name of each system id: a call finds its system by id, the same
        // after a rename (one saved by a recorder before ids, by short name).
        let mut targets: HashMap<String, Target> = HashMap::new();
        let mut names: HashMap<u32, String> = HashMap::new();
        // Health reports' nodes, by where they go (and certificate check).
        let mut health_nodes: BTreeMap<(String, bool), Vec<health::Node>> = BTreeMap::new();
        // What each system ID uploads, by server: one ID uploading several sites is said ([`sites`]).
        let mut covers: BTreeMap<(String, u32), sites::Covers> = BTreeMap::new();
        let data_dir = setup.data_dir.clone();
        for s in setup.systems {
            let Some(c) = &s.config else { continue };
            if !c.api_key.trim().is_empty() && c.system_id.is_none_or(|id| id == 0) {
                return Err(format!("Add {}'s Broadcastify system ID (or clear its API key).", s.short_name));
            }
            for site in s.sites.iter().filter(|t| t.config.is_some()) {
                if site.config.as_ref().and_then(node).is_none() {
                    return Err(format!("Add the Broadcastify system ID and API key of {}'s site {}.", s.short_name, site.name));
                }
            }
            let sites: Vec<(&str, (u32, &str))> = s.sites.iter().filter_map(|t| Some((t.name.as_str(), t.config.as_ref().and_then(node)?))).collect();
            if node(c).is_none() && sites.is_empty() {
                continue;
            }
            let server = s.settings.server.trim().to_string();
            if !(server.starts_with("https://") || server.starts_with("http://")) {
                return Err(format!("The upload server has to be a web address (https://…), not \"{server}\""));
            }
            if skip {
                host.warn(format!("not checking the certificate of {server} for {}", s.short_name));
            }
            // A site with an ID of its own goes to it; the others, to the system's.
            for t in &s.sites {
                if let Some((id, _)) = t.config.as_ref().and_then(node).or(node(c)) {
                    covers.entry((server.clone(), id)).or_default().push((s.short_name.clone(), Some(t.name.clone())));
                }
            }
            if let (true, Some((id, _))) = (s.sites.is_empty(), node(c)) {
                covers.entry((server.clone(), id)).or_default().push((s.short_name.clone(), None));
            }
            let uploader = uploaders.entry((server.clone(), skip)).or_insert_with(|| Arc::new(Uploader::new(&server, skip))).clone();
            let filter = c.talkgroups.filter_for(&s.short_name)?;
            let filtered = if filter.is_empty() { String::new() } else { format!(", talkgroups: {}", filter.describe()) };
            let lane = lane_of(&server);
            let to = if lane.is_empty() { String::new() } else { format!(" to {lane}") };
            match node(c) {
                Some((id, key)) => host.info(format!("uploading {}{to} as system {id} (key …{}){filtered}", s.short_name, last2(key))),
                None => host.info(format!("uploading {}{to} only from its sites with a Broadcastify system of their own{filtered}", s.short_name)),
            }
            for (site, (id, key)) in &sites {
                host.info(format!("uploading {}'s site {site} as system {id} (key …{})", s.short_name, last2(key)));
            }
            if s.id != 0 {
                names.insert(s.id, s.short_name.clone());
            }
            if let Some(url) = health_url_of(&s.settings) {
                // The system's node is its sites but those that are a Broadcastify system of their own; each of
                // those IDs is a node too, with every site that names it.
                let (nodes, notes) = health::nodes_of(&s.short_name, node(c), &sites);
                for n in notes {
                    host.warn(n);
                }
                health_nodes.entry((url, skip)).or_default().extend(nodes);
            }
            targets.insert(s.short_name.clone(), Target { lane, uploader, aliases: s.settings.talker_aliases, filter, system: s });
        }
        if targets.is_empty() {
            return Err("Add your Broadcastify API key and system ID to the systems you want to upload.".into());
        }
        // Said in the log now, and as the plugin's status from the first totals on (the recorder
        // says "running" once the plugin has started).
        let notices = sites::Notices::default();
        for ((_, id), on) in covers.iter().filter(|(_, on)| on.len() > 1) {
            let what = sites::several(*id, on);
            host.warn(format!("{what} {}", sites::ADVICE));
            notices.add(*id, what);
        }
        // Each server is a lane of the queue: one that's down holds up no other.
        let targets = Arc::new(targets);
        let lanes = targets.clone();
        let opts = QueueOptions { threads: UPLOAD_THREADS, noun: "upload", endpoint: Some("Broadcastify Calls".into()), ..QueueOptions::saved_in(&data_dir) };
        let names = Arc::new(names);
        // A call's system: by its id, else (a call from before ids) its short name.
        let of = move |call: &ConcludedCall| names.get(&call.call.system_id).cloned().unwrap_or_else(|| call.call.short_name.clone());
        let lane_of_call = of.clone();
        let lane = move |call: &ConcludedCall| lanes.get(&lane_of_call(call)).map(|t| t.lane.clone()).unwrap_or_default();
        // Systems whose settings Broadcastify turned down, said once each.
        let refused = Arc::new(Mutex::new(HashSet::new()));
        let refused_by = refused.clone();
        let status = host.clone();
        let status_host = host.clone();
        let queue = CallQueue::start_by(host, opts, lane, move |call: &ConcludedCall| {
            let Some(t) = targets.get(&of(call)) else {
                return Attempt::Skip("no Broadcastify API key for this system".into());
            };
            if call.call.encrypted {
                return Attempt::Skip("encrypted".into());
            }
            // Kept with none by "Keep calls with no audio": nothing to play.
            if !call.call.has_audio() {
                return Attempt::Skip("no audio".into());
            }
            if !t.filter.passes(call.call.talkgroup) {
                return Attempt::Skip(format!("talkgroup {} isn't uploaded (talkgroup filter)", call.call.talkgroup));
            }
            let Some(m4a) = &call.files.m4a else {
                return Attempt::Fail("this call couldn't be encoded as M4A".into());
            };
            let alias = if t.aliases { call.call.src_list.first().map(|s| s.tag_ota.trim()).filter(|a| !a.is_empty()) } else { None };
            // Each Broadcastify system the sites that heard it send to, once.
            let mut nodes: Vec<(u32, &str, Option<&str>)> = Vec::new();
            for d in t.system.targets(&call.call) {
                if let Some((id, key)) = node(d.config).filter(|(id, key)| !nodes.iter().any(|n| (n.0, n.1) == (*id, *key))) {
                    nodes.push((id, key, d.site.map(|s| s.name.as_str())));
                }
            }
            if nodes.is_empty() {
                return Attempt::Skip("none of the sites that heard it has a Broadcastify system".into());
            }
            // (Tried again, a system that already has it says so, and it's skipped there.)
            let attempts =
                nodes.into_iter().map(|(system_id, api_key, site)| match t.uploader.upload(&Upload { system_id, api_key, alias, call, audio: m4a }) {
                    Ok(a) => a,
                    // Trying again won't help (Trunk Recorder would): the call fails now, and the plugin's status says what to fix.
                    Err(r) => {
                        // A site of its own is named so ("dcfd's site south").
                        let who = site.map_or_else(|| call.call.short_name.clone(), |s| format!("{}'s site {s}", call.call.short_name));
                        let m = r.message(&who, system_id);
                        if refused_by.lock().unwrap().insert(who) {
                            status.status(State::Error, &m);
                        }
                        Attempt::Fail(m)
                    }
                });
            Attempt::together(attempts)
        });
        let recorder = Recorder::of(&setup.host);
        let reports = health_nodes
            .into_iter()
            // One report per system ID: Broadcastify takes no more each interval.
            .map(|((url, skip), nodes)| health::Reports::start(status_host.clone(), url, skip, recorder.clone(), health::one_per_id(nodes), notices.clone()))
            .collect();
        Ok(Broadcastify { host: status_host, queue, reports, notices, refused })
    }

    fn call_concluded(&mut self, call: ConcludedCall) {
        self.queue.push(call);
    }

    fn totals(&mut self, t: Totals) {
        for r in &self.reports {
            r.push(t.clone());
        }
        // A system ID uploading several sites stays the status: said again (the recorder's "running",
        // or the queue's "ok" once retries clear, replace it), but not over calls waiting to retry
        // or settings Broadcastify turned down, which say more.
        if let Some(m) = self.notices.status().filter(|_| self.queue.is_empty() && self.refused.lock().unwrap().is_empty()) {
            self.host.status(State::Warning, m);
        }
    }

    fn shutdown(&mut self, grace: Duration) {
        self.queue.shutdown(grace);
        for r in &mut self.reports {
            r.stop();
        }
    }
}

/// Where a system's health reports go: none when they're off, or its upload server has no
/// `call-health` beside its `call-upload`.
fn health_url_of(settings: &Config) -> Option<String> {
    if !settings.health_reports {
        return None;
    }
    let own = settings.health_server.trim();
    if !own.is_empty() {
        return Some(own.to_string());
    }
    health::health_url(settings.server.trim())
}

/// Uploads worked on at once, for each server. Every system sending to the
/// same server shares them, and each upload is two round trips (the details,
/// then the audio): at 4800 calls an hour, the SDK's 2 would leave each
/// upload 1.5 s before calls start to wait.
const UPLOAD_THREADS: usize = 4;

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
    use trunk_recorder_plugin::{EXIT_CONFIG, HostMessage, Outcome, SiteInfo, SiteRef, SystemInfo};

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
        // And the form-field style's fields, for a node set up for another recorder.
        assert_eq!((field("tg"), field("freq"), field("src")), ("101".into(), "851.0125".into(), "1234".into()));
        assert!(field("ts").parse::<i64>().unwrap() > 1_700_000_000);
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

    /// A call kept with no audio ("Keep calls with no audio"); a 0.4 s one, whose `call_length` says 0, has some.
    #[test]
    fn calls_with_no_audio_are_skipped() {
        let (dir, server) = (testing::temp_dir("bcfy-silent"), broadcastify());
        let mut silent = testing::call(&dir, "sys1", 101);
        (silent.call.call_length, silent.call.call_length_ms) = (0.0, 0);
        let out = testing::run::<Broadcastify>([hello(&dir, server.url(), json!({ "apiKey": "good", "systemId": 42 })), HostMessage::CallConcluded(silent)]);
        assert_eq!(out.results()[0].1, Outcome::Skipped);
        assert!(server.requests().is_empty());
        let mut short = testing::call(&dir, "sys1", 102);
        (short.call.call_length, short.call.call_length_ms) = (0.0, 400);
        let out = testing::run::<Broadcastify>([hello(&dir, server.url(), json!({ "apiKey": "good", "systemId": 42 })), HostMessage::CallConcluded(short)]);
        assert_eq!(out.results()[0].1, Outcome::Ok, "{:?}", out.results());
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
        assert_eq!(s["x-order"], json!(["apiKey", "systemId", "talkgroupMode", "talkgroupAllow", "talkgroupDeny"]));
        assert_eq!(s["properties"]["systemId"]["type"], "integer");
        let c = m.config.unwrap();
        assert_eq!(c["properties"]["talkerAliases"]["default"], true);
        assert_eq!(c["properties"]["healthReports"]["default"], true);
        // What a system can set again is one of its settings.
        assert!(m.system_overrides.iter().all(|k| c["properties"].get(k).is_some()), "{:?}", m.system_overrides);
        assert!(!m.system_overrides.contains(&"skipCertificateCheck".to_string()), "the certificate check is one for the recorder");
    }

    /// A site with a Broadcastify Calls system of its own (its settings in Setup): the calls it heard go
    /// there, whichever site's copy was kept; those only the others heard, to the system's.
    #[test]
    fn a_site_of_its_own() {
        let dir = testing::temp_dir("bcfy");
        // (The recorder sends each site's settings as the system's with what it sets again.)
        let run = |system: Value, south: Value, heard: &[(u32, &str)]| {
            let server = broadcastify();
            let mut h = testing::hello(&dir, json!({ "server": format!("{}/call-upload", server.url()) }));
            h.systems[0].config = system;
            h.systems[0].sites = vec![SiteInfo { id: 7, name: "north".into(), config: Value::Null }, SiteInfo { id: 8, name: "south".into(), config: south }];
            let mut call = testing::call(&dir, "sys1", 103);
            call.call.site = heard[0].1.into();
            call.call.sites = heard.iter().map(|&(id, name)| SiteRef { id, name: name.into() }).collect();
            let out = testing::run::<Broadcastify>([HostMessage::Hello(h), HostMessage::CallConcluded(call)]);
            let mut ids: Vec<String> = server.requests().iter().filter_map(|r| r.form_field("systemId")).map(|v| String::from_utf8(v).unwrap()).collect();
            ids.sort();
            (ids, out.results().into_iter().map(|r| r.1).collect::<Vec<_>>(), format!("{out:?}"))
        };
        let system = json!({ "apiKey": "good", "systemId": 42 });
        let south = json!({ "apiKey": "good", "systemId": 77 });
        assert_eq!(run(system.clone(), south.clone(), &[(7, "north"), (8, "south")]).0, ["42", "77"], "north has none of its own: the system's; south's own");
        assert_eq!(run(system.clone(), south.clone(), &[(8, "south")]).0, ["77"]);
        assert_eq!(run(system.clone(), south.clone(), &[(7, "north")]).0, ["42"]);
        // No system of the whole system's: only what south heard goes.
        let only = json!({ "systemId": 42 });
        assert_eq!(run(only.clone(), south.clone(), &[(7, "north"), (8, "south")]).0, ["77"]);
        let (ids, results, _) = run(only, south, &[(7, "north")]);
        assert_eq!((ids, results), (Vec::<String>::new(), vec![Outcome::Skipped]));
        // A site of its own needs its system ID and key.
        assert!(run(system, json!({ "apiKey": "good" }), &[(8, "south")]).2.contains("site south"));
    }

    /// Skipping the certificate check is the recorder's setting alone: a system that sets it for
    /// itself (as an older recorder could save) is still checked.
    #[test]
    fn the_certificate_check_is_set_once() {
        let (dir, server) = (testing::temp_dir("bcfy"), broadcastify());
        let run = |config: Value, settings: Value| {
            let mut h = testing::hello(&dir, config);
            h.systems[0].config = json!({ "apiKey": "good", "systemId": 42 });
            h.systems[0].settings = settings;
            format!("{:?}", testing::run::<Broadcastify>([HostMessage::Hello(h)]))
        };
        let url = format!("{}/call-upload", server.url());
        let own = run(json!({ "server": url }), json!({ "server": url, "skipCertificateCheck": true }));
        assert!(!own.contains("not checking the certificate"), "{own}");
        let all = run(json!({ "server": url, "skipCertificateCheck": true }), json!({ "server": url, "skipCertificateCheck": true }));
        assert!(all.contains("not checking the certificate"), "{all}");
    }

    /// The recorder's health totals, as it sends them every 10 s: two sites of sys1, `north` and `south`.
    fn totals(time: f64) -> Totals {
        use trunk_recorder_plugin::{SiteScope, SourceScope, Total};
        let up = time - 1000.0;
        let site = |name: &str| SiteScope {
            short_name: "sys1".into(),
            site_name: name.into(),
            key: format!("sys/sys1/site/{name}"),
            protocol: "p25".into(),
            control_channel_hz: Some(851_012_500),
            source: Some(0),
            ..Default::default()
        };
        let mut series = BTreeMap::new();
        for name in ["north", "south"] {
            let key = format!("sys/sys1/site/{name}");
            series.insert(format!("{key}/cc/locked"), Total { sum: up, s: up, now: Some(1.0) });
            series.insert(format!("{key}/cc/good"), Total { sum: up * 30.0, s: up, now: None });
            series.insert(format!("{key}/cc/bad"), Total { sum: up, s: up, now: None });
        }
        series.insert("src/800/samples".into(), Total { sum: up * 8e6, s: up, now: None });
        Totals {
            time,
            since: 1000.0,
            series,
            sites: vec![site("north"), site("south")],
            sources: vec![SourceScope { key: "src/800".into(), index: 0, name: "800".into(), driver: "airspy".into(), auto_tune: false }],
            ..Default::default()
        }
    }

    /// Every system ID gets a report of its own part of the recorder: a site of its own goes to its ID alone.
    #[test]
    fn health_reports_go_to_each_node() {
        let (dir, server) = (
            testing::temp_dir("bcfy"),
            MockServer::start(|req: &Request| match req.path.as_str() {
                "/call-health" => (200, r#"{"nextS":300,"message":"leader for 3 talkgroups"}"#.into()),
                _ => (404, String::new()),
            }),
        );
        let mut h = testing::hello(&dir, json!({ "server": format!("{}/call-upload", server.url()) }));
        h.systems[0].config = json!({ "apiKey": "good", "systemId": 42 });
        // South is a Broadcastify system of its own; north, set again but to the system's own, isn't.
        h.systems[0].sites = vec![
            SiteInfo { id: 7, name: "north".into(), config: json!({ "apiKey": "good", "systemId": 42 }) },
            SiteInfo { id: 8, name: "south".into(), config: json!({ "apiKey": "other", "systemId": 77 }) },
        ];
        h.host.name = "trunk-pro".into();
        h.host.version = "0.3.9".into();
        let mut msgs = vec![HostMessage::Hello(h)];
        // Seven minutes of totals: each node's first report is due once in them.
        msgs.extend((0..=42).map(|i| HostMessage::Totals(totals(1000.0 + i as f64 * 10.0))));
        let out = testing::run::<Broadcastify>(msgs);
        let reports: Vec<Value> = server.requests().iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect();
        let of = |id: u64| reports.iter().filter(|r| r["node"]["systemId"] == id).cloned().collect::<Vec<_>>();
        let (sys, south) = (of(42), of(77));
        assert!(!sys.is_empty() && !south.is_empty(), "{reports:?}");
        assert_eq!(sys[0]["node"]["apiKey"], "good");
        assert_eq!(south[0]["node"]["apiKey"], "other");
        let sites = |r: &Value| r["sites"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(sites(&sys[0]), ["north"]);
        assert_eq!(sites(&south[0]), ["south"]);
        let r = &sys[0];
        assert_eq!((r["v"].as_u64(), r["window"]["start"].as_f64(), r["window"]["seq"].as_u64()), (Some(1), Some(1000.0), Some(0)));
        let secs = r["window"]["end"].as_f64().unwrap() - 1000.0;
        assert!(secs >= 60.0, "not before a minute in: {secs}");
        assert_eq!(r["sites"][0]["cc"]["lockedS"].as_f64(), Some(secs));
        assert_eq!(r["sites"][0]["cc"]["good"].as_f64(), Some(secs * 30.0));
        assert_eq!(r["radios"][0]["kind"], "airspy");
        assert_eq!((r["recorder"]["name"].as_str(), r["recorder"]["version"].as_str()), (Some("trunk-recorder-pro"), Some("0.3.9")));
        // A second report of a node carries on where the first ended.
        if sys.len() > 1 {
            assert_eq!(sys[1]["window"]["start"], sys[0]["window"]["end"]);
            assert_eq!(sys[1]["window"]["seq"], 1);
        }
        assert!(out.logs().iter().any(|l| l.1.contains("leader for 3 talkgroups")), "{:?}", out.logs());
        assert!(!out.logs().iter().any(|l| l.1.contains(" is uploading ")), "a site of each ID: nothing to say");
    }

    /// A server that doesn't take reports (an answer that isn't one) is said once and tried less often; uploads go on.
    #[test]
    fn a_server_without_health_reports_is_left_alone() {
        let (dir, server) = (testing::temp_dir("bcfy"), broadcastify());
        let call = testing::call(&dir, "sys1", 101);
        let mut msgs = vec![hello(&dir, server.url(), json!({ "apiKey": "good", "systemId": 42 }))];
        msgs.extend((0..=100).map(|i| HostMessage::Totals(totals(1000.0 + i as f64 * 10.0))));
        msgs.push(HostMessage::CallConcluded(call.clone()));
        let out = testing::run::<Broadcastify>(msgs);
        assert_eq!(out.results()[0].1, Outcome::Ok);
        let tried = server.requests().iter().filter(|r| r.path == "/call-health").count();
        assert!((1..=2).contains(&tried), "tried, then backed off: {tried}");
        assert_eq!(out.logs().iter().filter(|l| l.1.contains("doesn't take node health reports")).count(), 1, "{:?}", out.logs());
        // Off: never tried.
        let server = broadcastify();
        let mut h = testing::hello(&dir, json!({ "server": format!("{}/call-upload", server.url()), "healthReports": false }));
        h.systems[0].config = json!({ "apiKey": "good", "systemId": 42 });
        let mut msgs = vec![HostMessage::Hello(h)];
        msgs.extend((0..=100).map(|i| HostMessage::Totals(totals(1000.0 + i as f64 * 10.0))));
        testing::run::<Broadcastify>(msgs);
        assert!(server.requests().is_empty());
    }

    /// A run with Broadcastify taking health reports: what was sent, what the plugin said, and its status.
    struct Ran {
        reports: Vec<Value>,
        logs: Vec<String>,
        status: Option<(State, String)>,
    }

    impl Ran {
        /// What it said of system IDs uploading several sites.
        fn several(&self) -> Vec<&str> {
            self.logs.iter().filter(|l| l.contains(" is uploading ")).map(String::as_str).collect()
        }
    }

    /// Twelve minutes of totals (`make`, from `from`): two or three reports of each node.
    fn health_run_with(system: Value, sites: Vec<SiteInfo>, more: Vec<SystemInfo>, from: f64, make: impl Fn(f64) -> Totals) -> Ran {
        let (dir, server) = (
            testing::temp_dir("bcfy"),
            MockServer::start(|req: &Request| match req.path.as_str() {
                "/call-health" => (200, r#"{"nextS":300}"#.into()),
                _ => (404, String::new()),
            }),
        );
        let mut h = testing::hello(&dir, json!({ "server": format!("{}/call-upload", server.url()) }));
        h.systems[0].config = system;
        h.systems[0].sites = sites;
        h.systems.extend(more);
        let mut msgs = vec![HostMessage::Hello(h)];
        msgs.extend((0..=72).map(|i| HostMessage::Totals(make(from + i as f64 * 10.0))));
        let out = testing::run::<Broadcastify>(msgs);
        let reports = server.requests().iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect();
        Ran { reports, logs: out.logs().into_iter().map(|l| l.1).collect(), status: out.status() }
    }

    fn health_run(system: Value, sites: Vec<SiteInfo>, more: Vec<SystemInfo>) -> Ran {
        health_run_with(system, sites, more, 1000.0, totals)
    }

    fn sites_of(r: &Value) -> Vec<String> {
        r["sites"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap().to_string()).collect()
    }

    fn site(id: u32, name: &str, config: Value) -> SiteInfo {
        SiteInfo { id, name: name.into(), config }
    }

    fn system(index: u16, short_name: &str, config: Value) -> SystemInfo {
        SystemInfo { index, short_name: short_name.into(), kind: "p25".into(), config, ..Default::default() }
    }

    /// Broadcastify Calls has a system ID for each site: one uploading several is said, in the log
    /// and as the plugin's status (uploads go on). Sites each with an ID of their own, or a
    /// system of one site, are fine.
    #[test]
    fn a_system_id_uploading_several_sites_is_said() {
        let id = |n: u32| json!({ "apiKey": "good", "systemId": n });
        // A system's ID, and two sites without their own.
        let r = health_run(id(77), vec![site(7, "north", Value::Null), site(8, "south", Value::Null)], vec![]);
        assert_eq!(r.several(), [format!("Broadcastify system 77 is uploading 2 sites: north, south. {}", sites::ADVICE)]);
        assert_eq!(r.status, Some((State::Warning, format!("Broadcastify system 77 is uploading 2 sites: north, south. {}", sites::ADVICE))));
        // One ID on each site's row.
        let r = health_run(json!({}), vec![site(7, "north", id(77)), site(8, "south", id(77))], vec![]);
        assert_eq!(r.several().len(), 1);
        assert!(r.several()[0].starts_with("Broadcastify system 77 is uploading 2 sites: north, south."), "{:?}", r.logs);
        // On site rows of two systems.
        let r = health_run(json!({}), vec![site(7, "north", id(77))], vec![system(1, "sys2", id(77))]);
        assert!(r.several()[0].starts_with("Broadcastify system 77 is uploading 2 sites: sys1 (north), sys2."), "{:?}", r.logs);
        // Two systems.
        let r = health_run(id(42), vec![], vec![system(1, "sys2", id(42))]);
        assert!(r.several()[0].starts_with("Broadcastify system 42 is uploading 2 sites: sys1, sys2."), "{:?}", r.logs);
        assert_eq!(r.status.map(|s| s.0), Some(State::Warning));
        // A site of each ID, a system of one site, a system with none: nothing to say.
        for (sys, sites) in
            [(id(77), vec![site(7, "north", Value::Null), site(8, "south", id(78))]), (id(77), vec![site(7, "north", Value::Null)]), (id(77), vec![])]
        {
            let r = health_run(sys, sites, vec![]);
            assert!(r.several().is_empty(), "{:?}", r.logs);
            assert_ne!(r.status.map(|s| s.0), Some(State::Warning));
        }
    }

    /// Sites the settings don't show (ones that appear while recording) are said once a health
    /// report names more than one under an ID. Receivers of one site are one site.
    #[test]
    fn sites_that_appear_while_recording_are_said() {
        let with_ids = |south: u32| {
            move |time: f64| {
                let mut t = totals(time);
                t.sites[0].identity.site = Some(1);
                t.sites[1].identity.site = Some(south);
                t
            }
        };
        let r = health_run_with(json!({ "apiKey": "good", "systemId": 42 }), vec![], vec![], 1000.0, with_ids(2));
        assert_eq!(r.several(), [format!("Broadcastify system 42 is uploading 2 sites: north, south. {}", sites::ADVICE)]);
        assert_eq!(r.status.map(|s| s.0), Some(State::Warning));
        let r = health_run_with(json!({ "apiKey": "good", "systemId": 42 }), vec![], vec![], 1000.0, with_ids(1));
        assert!(r.several().is_empty(), "receivers of one site: {:?}", r.logs);
    }

    /// Sites that name the same system ID are one node: one report each interval with all of them,
    /// not one per site (Broadcastify takes one report per system ID each interval).
    #[test]
    fn sites_sharing_a_system_id_are_one_report() {
        let own = json!({ "apiKey": "good", "systemId": 77 });
        for (sys, sites) in [
            // No ID for the whole system; both sites name 77.
            (json!({}), vec![site(7, "north", own.clone()), site(8, "south", own.clone())]),
            // The system's ID, with sites of none of their own.
            (own.clone(), vec![site(7, "north", Value::Null), site(8, "south", Value::Null)]),
        ] {
            let r = health_run(sys, sites, vec![]);
            assert!((2..=3).contains(&r.reports.len()), "one node, every 5 minutes: {:?}", r.reports);
            assert!(r.reports.iter().all(|r| r["node"]["systemId"] == 77 && sites_of(r) == ["north", "south"]), "{:?}", r.reports);
            let seqs: Vec<u64> = r.reports.iter().map(|r| r["window"]["seq"].as_u64().unwrap()).collect();
            assert_eq!(seqs, (0..r.reports.len() as u64).collect::<Vec<_>>());
        }
        // A site for each ID: a report for each.
        let r = health_run(own, vec![site(7, "north", Value::Null), site(8, "south", json!({ "apiKey": "good", "systemId": 78 }))], vec![]);
        assert!(r.reports.iter().any(|r| r["node"]["systemId"] == 77) && r.reports.iter().any(|r| r["node"]["systemId"] == 78));
    }

    /// Two recorder systems naming one system ID: a report can't cover both, so it's the first's.
    #[test]
    fn two_systems_with_one_id_are_reported_once() {
        let id = json!({ "apiKey": "good", "systemId": 42 });
        let r = health_run(id.clone(), vec![], vec![system(1, "sys2", id)]);
        assert!((2..=3).contains(&r.reports.len()), "one node: {:?}", r.reports);
        let seqs: Vec<u64> = r.reports.iter().map(|r| r["window"]["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, (0..r.reports.len() as u64).collect::<Vec<_>>());
    }

    /// A plugin (re)started while recording is under way has one totals to go on: no report with no
    /// time in it (Broadcastify turns those down); the next totals give it a window.
    #[test]
    fn a_restarted_plugin_sends_no_empty_window() {
        // Recording since 1000, the plugin's first totals at 5000.
        let r = health_run_with(json!({ "apiKey": "good", "systemId": 42 }), vec![], vec![], 5000.0, totals);
        assert!(!r.reports.is_empty());
        for r in &r.reports {
            assert!(r["window"]["end"].as_f64() > r["window"]["start"].as_f64(), "{r}");
        }
        assert_eq!(r.reports[0]["window"]["gap"], true, "the time before the plugin started isn't in it");
        assert_eq!(r.reports[0]["window"]["start"], 5000.0);
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
