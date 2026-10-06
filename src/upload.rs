//! One call to Broadcastify Calls, in two steps as Trunk Recorder's uploader
//! does it: POST the call's details (multipart) to the upload server, which
//! answers `0 <url>`; then PUT the audio to that URL.

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};
use trunk_recorder_plugin::{Attempt, ConcludedCall, Multipart};

pub struct Uploader {
    agent: ureq::Agent,
    server: String,
}

/// What Broadcastify needs to know about a call, besides its audio.
pub struct Upload<'a> {
    pub system_id: u32,
    pub api_key: &'a str,
    /// The talker alias of the call's first radio.
    pub alias: Option<&'a str>,
    pub call: &'a ConcludedCall,
    pub audio: &'a Path,
}

impl Uploader {
    pub fn new(server: &str, skip_certificate_check: bool) -> Uploader {
        let tls = ureq::tls::TlsConfig::builder().disable_verification(skip_certificate_check).build();
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .tls_config(tls)
            .user_agent(concat!("trunk-pro-broadcastify/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Uploader { agent, server: server.to_string() }
    }

    /// Sends the call; Err: Broadcastify turned down the node's settings.
    pub fn upload(&self, u: &Upload) -> Result<Attempt, Refusal> {
        let audio = match std::fs::read(u.audio) {
            Ok(b) => b,
            Err(e) => return Ok(Attempt::Fail(format!("can't read {}: {e}", u.audio.display()))),
        };
        let concluded = std::fs::metadata(&u.call.files.json).and_then(|m| m.modified()).ok();
        let concluded = concluded.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
        let (meta, length_ms) = metadata(u.call, concluded);
        let mut form = Multipart::new()
            .file("metadata", "call_meta.json", "application/json", meta.to_string().as_bytes())
            .text("callDuration", format!("{:.6}", length_ms as f64 / 1000.0))
            .text("systemId", u.system_id.to_string())
            .text("apiKey", u.api_key);
        if let Some(a) = u.alias {
            form = form.text("srcId_alias", a);
        }
        let (body, content_type) = form.finish();
        let mut resp = match self.agent.post(&self.server).header("Content-Type", &content_type).send(&body) {
            Ok(r) => r,
            Err(e) => return Ok(Attempt::Retry(e.to_string())),
        };
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if let Some(what) = refused(status, &text) {
            return Err(Refusal { what, answer: text.trim().chars().take(200).collect() });
        }
        let audio_url = match answer(status, &text) {
            Ok(url) => url,
            Err(a) => return Ok(a),
        };
        let r = self.agent.put(&audio_url).header("Content-Type", "audio/aac").send(&audio[..]);
        Ok(match r {
            Ok(r) if r.status().is_success() => Attempt::Done { url: String::new() },
            Ok(r) => Attempt::Retry(format!("audio upload: HTTP {}", r.status().as_u16())),
            Err(e) => Attempt::Retry(format!("audio upload: {e}")),
        })
    }
}

/// Which of the node's settings Broadcastify turned down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Refused {
    ApiKey,
    SystemId,
    /// The key and system ID don't go together, or the node isn't allowed to upload.
    NotAllowed,
}

/// Broadcastify turned down the node's settings, and will turn down every
/// call until they're changed.
#[derive(Debug)]
pub struct Refusal {
    pub what: Refused,
    /// What Broadcastify said.
    pub answer: String,
}

impl Refusal {
    pub fn message(&self, system: &str, system_id: u32) -> String {
        let what = match self.what {
            Refused::ApiKey => format!("Broadcastify refused the API key for {system} — check the system's API key"),
            Refused::SystemId => format!("Broadcastify refused system ID {system_id} for {system} — check the system's Broadcastify system ID"),
            Refused::NotAllowed => format!("Broadcastify won't take {system}'s calls as system {system_id} — check the system's API key and system ID"),
        };
        format!("{what} (Broadcastify said \"{}\")", self.answer)
    }
}

/// Whether an answer turns down the node's settings. Trunk Recorder tries
/// such a call again (twice, like any answer but SKIPPED and REJECTED), so a
/// mistyped key looks like Broadcastify being down; here the call fails at
/// once, and the plugin says what to fix.
///
/// Broadcastify documents `100 NO-API-KEY-SPECIFIED` ("do not retry without
/// correcting"); it answers a wrong key with `1 Invalid-API-Key`. Other
/// refusals are told apart by what they name.
fn refused(status: u16, text: &str) -> Option<Refused> {
    let text = text.trim();
    let (code, message) = text.split_once(' ').unwrap_or((text, ""));
    if status != 200 || code == "0" || message.starts_with("SKIPPED") || message.starts_with("REJECTED") {
        return None;
    }
    let m = message.to_ascii_uppercase().replace(['_', ' '], "-");
    if code == "100" || m.contains("API-KEY") || m.contains("APIKEY") {
        Some(Refused::ApiKey)
    } else if m.contains("SYSTEM") && ["INVALID", "UNKNOWN", "NO-", "BAD"].iter().any(|w| m.contains(w)) {
        Some(Refused::SystemId)
    } else if ["NOT-AUTHORIZED", "UNAUTHORIZED", "NOT-ALLOWED", "DENIED", "FORBIDDEN"].iter().any(|w| m.contains(w)) {
        Some(Refused::NotAllowed)
    } else {
        None
    }
}

/// What the answer to the details means: where to send the audio, or what became of the call.
fn answer(status: u16, text: &str) -> Result<String, Attempt> {
    let text = text.trim();
    if status != 200 {
        let what: String = text.lines().next().unwrap_or("").chars().take(200).collect();
        let why = if what.is_empty() { format!("HTTP {status}") } else { format!("HTTP {status}: {what}") };
        return Err(match status {
            // Something about the request was wrong; it won't get better.
            400..=499 if status != 408 && status != 429 => Attempt::Fail(why),
            _ => Attempt::Retry(why),
        });
    }
    let (code, message) = text.split_once(' ').unwrap_or((text, ""));
    match code {
        "0" if message.starts_with("http") => Ok(message.trim().to_string()),
        "1" if message.starts_with("SKIPPED") => Err(Attempt::Skip(format!("Broadcastify: {message}"))),
        "1" if message.starts_with("REJECTED") => Err(Attempt::Fail(format!("Broadcastify: {message}"))),
        // (Trunk Recorder tries anything else again; so does this, but for
        // the refusals `refused` picks out first.)
        _ => Err(Attempt::Retry(format!("Broadcastify: {}", if text.is_empty() { "an empty answer" } else { text }))),
    }
}

/// The call JSON to send, and the call's length in ms.
///
/// Broadcastify measures how late a node is from the call's end time. A call
/// stays open for a few seconds after its last transmission, so (as Trunk
/// Recorder does) the times sent are those of the call's audio played back to
/// back, ending when the call was concluded — when its JSON was written,
/// which stays the same when an upload is tried again later. The call's
/// files keep their real times.
fn metadata(c: &ConcludedCall, concluded: Option<i64>) -> (Value, i64) {
    let mut v = serde_json::to_value(&c.call).unwrap_or_default();
    let length_ms = v["call_length_ms"].as_i64().filter(|&l| l > 0).unwrap_or((c.call.call_length * 1000.0).round() as i64);
    if let Some(end) = concluded.filter(|&t| t > 0 && length_ms > 0) {
        let stop_ms = end * 1000;
        let start_ms = stop_ms - length_ms;
        let start = start_ms as f64 / 1000.0;
        v["start_time_ms"] = json!(start_ms);
        v["start_time"] = json!(start_ms.div_euclid(1000));
        v["stop_time_ms"] = json!(stop_ms);
        v["stop_time"] = json!(end);
        for list in ["freqList", "srcList"] {
            for e in v.get_mut(list).and_then(Value::as_array_mut).into_iter().flatten() {
                if let Some(pos) = e["pos"].as_f64() {
                    e["time"] = json!(start + pos);
                }
            }
        }
    }
    (v, length_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(answer(200, "0 https://s3.example/x.m4a?sig=1\n").unwrap(), "https://s3.example/x.m4a?sig=1");
        assert!(matches!(answer(200, "1 SKIPPED: duplicate"), Err(Attempt::Skip(_))));
        assert!(matches!(answer(200, "1 REJECTED: no such system"), Err(Attempt::Fail(_))));
        assert!(matches!(answer(200, ""), Err(Attempt::Retry(_))));
        assert!(matches!(answer(200, "1 Database busy"), Err(Attempt::Retry(_))));
        assert!(matches!(answer(502, "Bad gateway"), Err(Attempt::Retry(_))));
        assert!(matches!(answer(403, "Forbidden"), Err(Attempt::Fail(_))));
    }

    #[test]
    fn refusals_of_the_settings() {
        assert_eq!(refused(200, "1 Invalid-API-Key\n"), Some(Refused::ApiKey));
        assert_eq!(refused(200, "100 NO-API-KEY-SPECIFIED"), Some(Refused::ApiKey));
        assert_eq!(refused(200, "1 Invalid-System-ID"), Some(Refused::SystemId));
        assert_eq!(refused(200, "1 API Key not authorized for system"), Some(Refused::ApiKey));
        assert_eq!(refused(200, "1 Not-Authorized"), Some(Refused::NotAllowed));
        // Not refusals: success, the answers Trunk Recorder knows, anything else, and HTTP errors.
        for text in ["0 https://s3.example/x", "1 SKIPPED---ALREADY-RECEIVED-THIS-CALL", "1 REJECTED: no such system", "1 Database busy", ""] {
            assert_eq!(refused(200, text), None, "{text}");
        }
        assert_eq!(refused(502, "Invalid-API-Key"), None);
        let r = Refusal { what: Refused::ApiKey, answer: "1 Invalid-API-Key".into() };
        assert_eq!(r.message("dcfd", 42), "Broadcastify refused the API key for dcfd — check the system's API key (Broadcastify said \"1 Invalid-API-Key\")");
    }

    #[test]
    fn length_from_ms_or_seconds() {
        let mut c = ConcludedCall::default();
        c.call.call_length = 4.0;
        assert_eq!(metadata(&c, None).1, 4000);
        c.call.extra.insert("call_length_ms".into(), json!(4320));
        let (v, ms) = metadata(&c, Some(1_700_000_010));
        assert_eq!(ms, 4320);
        assert_eq!(v["start_time_ms"], 1_700_000_005_680i64);
        assert_eq!(v["start_time"], 1_700_000_005);
    }
}
