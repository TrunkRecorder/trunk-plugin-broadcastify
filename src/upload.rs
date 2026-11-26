//! One call to Broadcastify Calls, in two steps as Trunk Recorder's uploader
//! does it: POST the call's details (multipart) to the upload server, which
//! answers `0 <url>`; then PUT the audio to that URL.

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};
use trunk_recorder_plugin::{Attempt, ConcludedCall, Host, Multipart};

pub struct Uploader {
    agent: ureq::Agent,
    server: String,
    /// For what a call's reason leaves out (Broadcastify's whole answer).
    log: Host,
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

/// How the plugin talks to Broadcastify (uploads and health reports): every answer read, not an error.
pub fn agent(timeout: Duration, skip_certificate_check: bool) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().disable_verification(skip_certificate_check).build();
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .tls_config(tls)
        .user_agent(concat!("trunk-pro-broadcastify/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

impl Uploader {
    pub fn new(server: &str, skip_certificate_check: bool, log: Host) -> Uploader {
        Uploader { agent: agent(Duration::from_secs(60), skip_certificate_check), server: server.to_string(), log }
    }

    /// Sends the call; Err: Broadcastify turned down the node's settings.
    pub fn upload(&self, u: &Upload) -> Result<Attempt, Refusal> {
        let audio = match std::fs::read(u.audio) {
            Ok(b) => b,
            Err(e) => return Ok(Attempt::Fail(format!("can't read its M4A: {e}"))),
        };
        let concluded = std::fs::metadata(&u.call.files.json).and_then(|m| m.modified()).ok();
        let concluded = concluded.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
        let (meta, length_ms) = metadata(u.call, concluded);
        let mut form = Multipart::new()
            .file("metadata", "call_meta.json", "application/json", meta.to_string().as_bytes())
            .text("callDuration", format!("{:.6}", length_ms as f64 / 1000.0))
            .text("systemId", u.system_id.to_string())
            .text("apiKey", u.api_key);
        for (name, value) in fields(&meta) {
            form = form.text(name, value);
        }
        if let Some(a) = u.alias {
            form = form.text("srcId_alias", a);
        }
        let (body, content_type) = form.finish();
        let mut resp = match self.agent.post(&self.server).header("Content-Type", &content_type).send(&body) {
            Ok(r) => r,
            Err(e) => return Ok(Attempt::Retry(e.to_string())),
        };
        let status = resp.status().as_u16();
        let retry_after = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if let Some(what) = refused(&text) {
            return Err(Refusal { what, answer: text.trim().chars().take(200).collect() });
        }
        let audio_url = match answer(status, retry_after.as_deref(), &text) {
            Ok(url) => url,
            Err(a) => {
                // The reason is the same for every call; the log has what Broadcastify said of this one.
                if matches!(&a, Attempt::Fail(m) if m == SKEW) {
                    self.log.warn(format!("{}: Broadcastify: {}", u.call.path, text.trim()));
                }
                return Ok(a);
            }
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
/// Broadcastify's answers are `<code> <what>`, with any HTTP status (`1 No
/// API Key Specified` comes with 403); a wrong key is `1 Invalid-API-Key`.
fn refused(text: &str) -> Option<Refused> {
    let (code, message) = coded(text)?;
    if code == 0 || message.starts_with("SKIPPED") || message.starts_with("REJECTED") {
        return None;
    }
    let m = loud(message);
    if code == 100 || m.contains("API-KEY") || m.contains("APIKEY") {
        Some(Refused::ApiKey)
    } else if ["UPLOADS-NOT-ENABLED", "UNSUPPORTED-NODE-TYPE"].iter().any(|w| m.contains(w)) {
        // The node is turned off, or set up for a recorder it can't take calls from: support's to fix.
        Some(Refused::NotAllowed)
    } else if m.contains("SYSTEM") && ["INVALID", "UNKNOWN", "NO-", "BAD"].iter().any(|w| m.contains(w)) {
        Some(Refused::SystemId)
    } else if ["NOT-AUTHORIZED", "UNAUTHORIZED", "NOT-ALLOWED", "DENIED", "FORBIDDEN"].iter().any(|w| m.contains(w)) {
        Some(Refused::NotAllowed)
    } else {
        None
    }
}

/// An answer's code and what it says, when it's in Broadcastify's `<code> <what>` form.
fn coded(text: &str) -> Option<(u32, &str)> {
    let text = text.trim();
    let (code, message) = text.split_once(' ').unwrap_or((text, ""));
    Some((code.parse().ok()?, message.trim()))
}

/// Upper case, `-` between words, as most of Broadcastify's answers are written.
fn loud(message: &str) -> String {
    message.to_ascii_uppercase().replace(['_', ' '], "-")
}

// The reasons of Broadcastify's answers, as the card counts them (the same for every call).
/// `SKIPPED---ALREADY-RECEIVED-THIS-CALL`: another node hearing the system (or, on a retry, this
/// one's earlier try) sent it first. Which copy is kept is Broadcastify's choice; this node's are
/// the backup, taken if the other node stops or its audio is worse.
pub const ALREADY_RECEIVED: &str = "already received";
/// `Skip-Unit-To-Unit-CT3`/`CT4`: a private call (a talkgroup above 65535), which it doesn't take.
pub const UNIT_TO_UNIT: &str = "unit-to-unit";
/// `REJECTED-CALL-SKEW-TOO-LONG-<s>-See:…`: the call reached it too long after it was heard
/// (this computer's clock is off, or delay rules held it). Its seconds differ call by call.
pub const SKEW: &str = "Broadcastify: too long after the call (REJECTED-CALL-SKEW): check this computer's clock and delay rules";

/// What the answer to the details means: where to send the audio, or what became of the call.
fn answer(status: u16, retry_after: Option<&str>, text: &str) -> Result<String, Attempt> {
    let text = text.trim();
    if let Some((code, message)) = coded(text) {
        let m = loud(message);
        let said = || format!("Broadcastify: {message}");
        match code {
            0 if message.starts_with("http") => return Ok(message.to_string()),
            // Broadcastify answered and turned it away, as it means to: the call is skipped, and it
            // counts as answering.
            1 if m.starts_with("SKIPPED---ALREADY-RECEIVED") => return Err(Attempt::Declined(ALREADY_RECEIVED.into())),
            1 if m.starts_with("SKIP-UNIT-TO-UNIT") => return Err(Attempt::Declined(UNIT_TO_UNIT.into())),
            1 if m.starts_with("SKIPPED") => return Err(Attempt::Declined(said())),
            1 if m.starts_with("REJECTED-CALL-SKEW") => return Err(Attempt::Fail(SKEW.into())),
            1 if m.starts_with("REJECTED") => return Err(Attempt::Fail(said())),
            // Something the request lacks, or a slot the node doesn't have: sending it again won't help.
            1 if FIX_THE_REQUEST.iter().any(|w| m.contains(w)) => return Err(Attempt::Fail(said())),
            _ => {}
        }
    }
    if !(200..300).contains(&status) {
        let what: String = text.lines().next().unwrap_or("").chars().take(200).collect();
        let why = if what.is_empty() { format!("HTTP {status}") } else { format!("HTTP {status}: {what}") };
        return Err(Attempt::from_status(status, retry_after, why));
    }
    // (Trunk Recorder tries anything else again; so does this, but for
    // the refusals `refused` picks out first.)
    Err(Attempt::Retry(format!("Broadcastify: {}", if text.is_empty() { "an empty answer" } else { text })))
}

/// The answers that say the request was wrong ("Fix the request" in Broadcastify's spec), as [`loud`] writes them.
const FIX_THE_REQUEST: &[&str] = &[
    "NO-TG-SENT",
    "NO-FREQ-SENT",
    "NO-TIMESTAMP-SENT",
    "NO-CALL-DURATION-SENT",
    "SLOT-ENTRY-ERROR",
    "TABLE-ENTRY-FOR-UPLOAD",
    "METADATA",
    "CALL-DURATION-MUST-BE-SPECIFIED",
    "INVALID-ENCODE-TYPE",
    "NOTHING-SENT",
];

/// The call as the form-field style's fields: `tg`, `freq`, `ts` and `src`
/// (`callDuration` goes with every upload), from the call JSON that's sent.
///
/// Broadcastify reads a node's calls in the style it set the node up for,
/// from the recorder it was approved with: the call JSON (`metadata`) for
/// Trunk Recorder, these fields for others (SDRTrunk, Trunking Recorder).
/// A node set up for another recorder answers NO-TG-SENT, NO-FREQ-SENT, …
/// without them, so both are sent: a node reads its own style's.
/// (Broadcastify's spec: <https://github.com/blantonl/bcfy-calls-upload>.)
fn fields(meta: &Value) -> Vec<(&'static str, String)> {
    let num = |k: &str| meta[k].as_u64().unwrap_or(0);
    // A patch's talkgroups, its own first (Broadcastify files it under the first unless the system says otherwise).
    let talkgroup = num("talkgroup");
    let patched = meta["patched_talkgroups"].as_array().into_iter().flatten().filter_map(Value::as_u64).filter(|&t| t != talkgroup);
    let tg: Vec<String> = std::iter::once(talkgroup).chain(patched).map(|t| t.to_string()).collect();
    let mut f = vec![("tg", tg.join(",")), ("freq", format!("{}", num("freq") as f64 / 1e6)), ("ts", meta["start_time"].as_i64().unwrap_or(0).to_string())];
    let src = meta["srcList"].as_array().into_iter().flatten().filter_map(|s| s["src"].as_i64()).find(|&s| s > 0);
    if let Some(s) = src {
        f.push(("src", s.to_string()));
    }
    f
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
    let length_ms = c.call.audio_ms();
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
        assert_eq!(answer(200, None, "0 https://s3.example/x.m4a?sig=1\n").unwrap(), "https://s3.example/x.m4a?sig=1");
        assert!(matches!(answer(200, None, "1 SKIPPED: duplicate"), Err(Attempt::Declined(m)) if m == "Broadcastify: SKIPPED: duplicate"));
        assert!(matches!(answer(200, None, "1 REJECTED: no such system"), Err(Attempt::Fail(_))));
        assert!(matches!(answer(200, None, ""), Err(Attempt::Retry(_))));
        assert!(matches!(answer(200, None, "1 Database busy"), Err(Attempt::Retry(_))));
        assert!(matches!(answer(502, None, "Bad gateway"), Err(Attempt::Retry(_))));
        assert!(matches!(answer(403, None, "Forbidden"), Err(Attempt::Fail(_))));
        // Busy, and it said for how long: waited on.
        assert!(matches!(answer(503, Some("90"), "Service Unavailable"), Err(Attempt::Wait(_, w)) if w.as_secs() == 90));
        assert!(matches!(answer(429, None, ""), Err(Attempt::Retry(_))));
        // A 2xx that isn't Broadcastify's answer: tried again, not taken as done.
        assert!(matches!(answer(204, None, ""), Err(Attempt::Retry(_))));
        // The answers of Broadcastify's spec that aren't the node's settings.
        assert!(matches!(answer(200, None, "1 SKIPPED---ALREADY-RECEIVED-THIS-CALL"), Err(Attempt::Declined(m)) if m == ALREADY_RECEIVED));
        assert!(matches!(answer(200, None, "1 Skip-Unit-To-Unit-CT4"), Err(Attempt::Declined(m)) if m == UNIT_TO_UNIT));
        assert!(matches!(answer(200, None, "1 REJECTED-CALL-SKEW-TOO-LONG-22-See:https://www.broadcastify.com/skew"), Err(Attempt::Fail(m)) if m == SKEW));
        for text in [
            "1 NO-TG-SENT",
            "1 NO-FREQ-SENT",
            "1 NO-TIMESTAMP-SENT",
            "1 NO-CALL-DURATION-SENT",
            "1 SLOT_ENTRY_ERROR",
            "1 No-Tg-Table-Entry-For-Upload",
            "1 INVALID-ENCODE-TYPE",
            "1 Nothing-Sent",
        ] {
            assert!(matches!(answer(200, None, text), Err(Attempt::Fail(_))), "{text}");
        }
        assert!(matches!(answer(500, None, "1 TRUNK-RECORDER-METADATA-ERROR"), Err(Attempt::Fail(_))), "with any status");
        assert!(matches!(answer(500, None, "1 ERROR: Call Duration Must be Specified for Conventional Calls"), Err(Attempt::Fail(_))));
        assert!(matches!(answer(200, None, "1 Uploads-disabled-systemwide"), Err(Attempt::Retry(_))));
    }

    #[test]
    fn refusals_of_the_settings() {
        assert_eq!(refused("1 Invalid-API-Key\n"), Some(Refused::ApiKey));
        assert_eq!(refused("100 NO-API-KEY-SPECIFIED"), Some(Refused::ApiKey));
        assert_eq!(refused("1 Invalid-System-ID"), Some(Refused::SystemId));
        assert_eq!(refused("1 API Key not authorized for system"), Some(Refused::ApiKey));
        assert_eq!(refused("1 Not-Authorized"), Some(Refused::NotAllowed));
        // Not refusals: success, the answers Trunk Recorder knows, anything else, and HTTP errors.
        for text in ["0 https://s3.example/x", "1 SKIPPED---ALREADY-RECEIVED-THIS-CALL", "1 REJECTED: no such system", "1 Database busy", ""] {
            assert_eq!(refused(text), None, "{text}");
        }
        assert_eq!(refused("Invalid-API-Key"), None, "not in Broadcastify's form");
        // Whatever the HTTP status, and the node turned off or set up for another recorder.
        assert_eq!(refused("1 No API Key Specified"), Some(Refused::ApiKey));
        assert_eq!(refused("1 API-Key-Not-Authorized-For-System"), Some(Refused::ApiKey));
        assert_eq!(refused("1 NO-SYSTEM-ID-SPECIFIED"), Some(Refused::SystemId));
        assert_eq!(refused("1 Uploads-not-enabled-for-this-system"), Some(Refused::NotAllowed));
        assert_eq!(refused("1 Unsupported-Node-Type-Encoder-3-CallType-2"), Some(Refused::NotAllowed));
        for text in ["1 NO-FREQ-SENT", "1 Uploads-disabled-systemwide", "1 No-Freq-Table-Entry-For-Upload", "1 Skip-Unit-To-Unit-CT3"] {
            assert_eq!(refused(text), None, "{text}");
        }
        let r = Refusal { what: Refused::ApiKey, answer: "1 Invalid-API-Key".into() };
        assert_eq!(r.message("dcfd", 42), "Broadcastify refused the API key for dcfd — check the system's API key (Broadcastify said \"1 Invalid-API-Key\")");
    }

    #[test]
    fn form_fields() {
        let mut c = ConcludedCall::default();
        c.call.freq = 851_162_500;
        c.call.talkgroup = 1201;
        c.call.call_length_ms = 6120;
        c.call.src_list = vec![Default::default(), Default::default()];
        c.call.src_list[1].src = 4801123;
        let (meta, _) = metadata(&c, Some(1_794_590_106));
        let f = |meta: &Value| fields(meta).into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" ");
        // As in Broadcastify's examples: MHz, the start that's sent in the metadata, the first radio known.
        assert_eq!(f(&meta), "tg=1201 freq=851.1625 ts=1794590099 src=4801123");
        // A patch: its own talkgroup first.
        c.call.patched_talkgroups = vec![1305, 1201];
        c.call.src_list.clear();
        assert_eq!(f(&metadata(&c, Some(1_794_590_106)).0), "tg=1201,1305 freq=851.1625 ts=1794590099");
    }

    #[test]
    fn length_from_ms_or_seconds() {
        let mut c = ConcludedCall::default();
        c.call.call_length = 4.0;
        assert_eq!(metadata(&c, None).1, 4000);
        c.call.call_length_ms = 4320;
        let (v, ms) = metadata(&c, Some(1_700_000_010));
        assert_eq!(ms, 4320);
        assert_eq!(v["start_time_ms"], 1_700_000_005_680i64);
        assert_eq!(v["start_time"], 1_700_000_005);
    }
}
