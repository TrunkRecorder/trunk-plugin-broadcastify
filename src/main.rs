//! Broadcastify — a Trunk Recorder Pro plugin that uploads recorded calls to
//! [Broadcastify Calls](https://www.broadcastify.com/calls/), as Trunk
//! Recorder's Broadcastify uploader does.

mod upload;

use std::collections::HashMap;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use trunk_recorder_plugin::filter::patterns;
use trunk_recorder_plugin::{Attempt, CallQueue, ConcludedCall, Host, Manifest, Plugin, QueueOptions, Setup, TalkgroupFilter, format, topic};

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
    /// Only these talkgroups
    ///
    /// Upload these talkgroups and no others: numbers, or patterns like 507* (* stands for any digits, ? for one). Leave it empty to upload every talkgroup.
    #[serde(alias = "broadcastifyAllow", alias = "broadcastifyWhitelist", alias = "talkgroupWhitelist", deserialize_with = "patterns")]
    #[schemars(with = "Vec<String>")]
    talkgroup_allow: Vec<String>,
    /// Not these talkgroups
    ///
    /// Never upload these talkgroups: numbers or patterns, as above.
    #[serde(alias = "broadcastifyDeny", alias = "broadcastifyBlacklist", alias = "talkgroupBlacklist", deserialize_with = "patterns")]
    #[schemars(with = "Vec<String>")]
    talkgroup_deny: Vec<String>,
}

/// Where a system's calls go.
struct Target {
    system_id: u32,
    api_key: String,
    filter: TalkgroupFilter,
}

struct Broadcastify {
    queue: CallQueue,
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
        let server = setup.config.server.trim().to_string();
        if !(server.starts_with("https://") || server.starts_with("http://")) {
            return Err(format!("The upload server has to be a web address (https://…), not \"{server}\""));
        }
        // Broadcastify takes AAC audio, not WAV.
        if !setup.has_format(format::M4A) {
            return Err("Broadcastify needs calls as M4A, and there's no M4A encoder on this computer. Install ffmpeg, then start recording again.".into());
        }
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
            let filter = TalkgroupFilter::new(&c.talkgroup_allow, &c.talkgroup_deny);
            let filtered = if filter.is_empty() { String::new() } else { format!(", talkgroups: {}", filter.describe()) };
            host.info(format!("uploading {} as system {system_id} (key …{}){filtered}", s.short_name, last2(&api_key)));
            targets.insert(s.short_name.clone(), Target { system_id, api_key, filter });
        }
        if targets.is_empty() {
            return Err("Add your Broadcastify API key and system ID to the systems you want to upload.".into());
        }
        if setup.config.skip_certificate_check {
            host.warn("not checking Broadcastify's certificate");
        }
        let uploader = Uploader::new(&server, setup.config.skip_certificate_check);
        let aliases = setup.config.talker_aliases;
        let opts = QueueOptions { noun: "upload", endpoint: Some("Broadcastify Calls".into()), ..QueueOptions::saved_in(&setup.data_dir) };
        let queue = CallQueue::start(host, opts, move |call: &ConcludedCall| {
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
            let alias = if aliases { call.call.src_list.first().map(|s| s.tag_ota.trim()).filter(|a| !a.is_empty()) } else { None };
            uploader.upload(&Upload { system_id: t.system_id, api_key: &t.api_key, alias, call, audio: m4a })
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
    use trunk_recorder_plugin::{EXIT_CONFIG, HostMessage, Outcome, State};

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
    fn talkgroup_filters_and_trunk_recorders_setting_names() {
        let (dir, server) = (testing::temp_dir("bcfy"), broadcastify());
        let system = json!({ "broadcastifyApiKey": "good", "broadcastifySystemId": 42, "broadcastifyAllow": ["10*", 205], "broadcastifyDeny": ["109"] });
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
        assert_eq!(s["properties"]["talkgroupAllow"]["type"], "array");
        assert_eq!(s["properties"]["talkgroupAllow"]["items"]["type"], "string");
        assert_eq!(s["properties"]["systemId"]["type"], "integer");
        assert_eq!(m.config.unwrap()["properties"]["talkerAliases"]["default"], true);
    }
}
