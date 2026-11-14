//! Node health reports to Broadcastify Calls (`call-health`): every 5 minutes, at a random point
//! of each node's own, how well this recorder hears each system ID it uploads. Broadcastify uses
//! them to tell a node that's up but quiet from one that's down, and to prefer the node hearing a
//! system best when several upload it. The report's format is the recorder's
//! (`docs/api/node-health.md`); this adds where it goes and the node's ID and key.
//!
//! Reports go from a thread of their own, so an upload never waits on one.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::json;
use trunk_recorder_plugin::Health;
use trunk_recorder_plugin::Host;
use trunk_recorder_plugin::health::{Answer, Failure, History, Recorder, Report, Reporter, Select};

/// One node: a system ID and its key, and the part of the recorder it is.
pub struct Node {
    pub system_id: u32,
    pub api_key: String,
    pub select: Select,
}

/// Where a server's reports go: its `call-upload` URL's sibling `call-health`.
pub fn health_url(upload_server: &str) -> Option<String> {
    let s = upload_server.trim_end_matches('/');
    s.strip_suffix("/call-upload").map(|base| format!("{base}/call-health"))
}

/// What became of one report.
#[derive(Debug, PartialEq)]
enum Sent {
    Ok(Answer),
    Failed(Failure, String),
    /// The server doesn't take reports (yet): not found, or an answer that isn't one.
    NotTaken(String),
}

fn retry_after(r: &ureq::http::Response<ureq::Body>) -> Option<f64> {
    r.headers().get("retry-after")?.to_str().ok()?.trim().parse::<f64>().ok()
}

fn send(agent: &ureq::Agent, url: &str, report: &Report) -> Sent {
    let mut r = match agent.post(url).header("Content-Type", "application/json").send(serde_json::to_vec(report).unwrap_or_default().as_slice()) {
        Ok(r) => r,
        Err(e) => return Sent::Failed(Failure::Unreachable, e.to_string()),
    };
    let status = r.status().as_u16();
    let after = retry_after(&r);
    let text = r.body_mut().read_to_string().unwrap_or_default();
    let said: String = text.trim().chars().take(200).collect();
    match status {
        200..=299 => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) if v.is_object() => Sent::Ok(serde_json::from_value(v).unwrap_or_default()),
            _ => Sent::NotTaken(format!("it answered \"{said}\"")),
        },
        404 | 405 => Sent::NotTaken(format!("HTTP {status}")),
        401 | 403 => Sent::Failed(Failure::Refused, format!("HTTP {status}: {said}")),
        429 | 503 => Sent::Failed(Failure::Busy(after), format!("HTTP {status}")),
        400..=499 => Sent::Failed(Failure::Rejected, format!("HTTP {status}: {said}")),
        _ => Sent::Failed(Failure::Unreachable, format!("HTTP {status}")),
    }
}

/// The reporting thread: give it the recorder's totals as they come.
pub struct Reports {
    tx: Option<Sender<Health>>,
    thread: Option<JoinHandle<()>>,
}

impl Reports {
    pub fn start(host: Host, url: String, skip_certificate_check: bool, recorder: Recorder, nodes: Vec<Node>) -> Reports {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new().name("health".into()).spawn(move || run(host, url, skip_certificate_check, recorder, nodes, rx)).ok();
        Reports { tx: Some(tx), thread }
    }

    pub fn push(&self, h: Health) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(h);
        }
    }

    pub fn stop(&mut self) {
        self.tx.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(host: Host, url: String, skip: bool, recorder: Recorder, nodes: Vec<Node>, rx: Receiver<Health>) {
    let tls = ureq::tls::TlsConfig::builder().disable_verification(skip).build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .tls_config(tls)
        .user_agent(concat!("trunk-pro-broadcastify/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut nodes: Vec<(Node, Reporter, Option<String>)> = nodes
        .into_iter()
        .map(|n| {
            let r = Reporter::new(n.select.clone(), &n.system_id.to_string());
            (n, r, None)
        })
        .collect();
    let mut history = History::default();
    let mut told_down = false;
    while let Ok(h) = rx.recv() {
        history.push(h.clone());
        for (n, r, last_message) in nodes.iter_mut() {
            if !r.due(&h) {
                continue;
            }
            let node = json!({ "systemId": n.system_id, "apiKey": n.api_key });
            let Some(report) = r.report(&history, node, recorder.clone()) else { continue };
            match send(&agent, &url, &report) {
                Sent::Ok(answer) => {
                    told_down = false;
                    r.delivered(&report, answer.next_s);
                    // What Broadcastify makes of the node, when it says something new.
                    if let Some(m) = answer.message.filter(|m| !m.is_empty() && Some(m) != last_message.as_ref()) {
                        host.info(format!("Broadcastify on system {}: {m}", n.system_id));
                        *last_message = Some(m);
                    }
                }
                Sent::NotTaken(why) => {
                    host.info(format!("{url} doesn't take node health reports ({why}); not sending them until recording starts again"));
                    return;
                }
                Sent::Failed(f, why) => {
                    match f {
                        Failure::Refused => host.warn(format!(
                            "Broadcastify refused the health report of system {} ({why}): not sending its reports until recording starts again",
                            n.system_id
                        )),
                        Failure::Rejected => host.warn(format!("Broadcastify turned down the health report of system {}: {why}", n.system_id)),
                        // Said once, not every 5 minutes.
                        _ if !told_down => {
                            told_down = true;
                            host.debug(format!("health report of system {} not sent: {why}", n.system_id));
                        }
                        _ => {}
                    }
                    r.failed(&h, f);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_health_url_is_the_upload_servers_sibling() {
        assert_eq!(health_url("https://api.broadcastify.com/call-upload").as_deref(), Some("https://api.broadcastify.com/call-health"));
        assert_eq!(health_url("http://10.0.0.5:8080/call-upload/").as_deref(), Some("http://10.0.0.5:8080/call-health"));
        assert_eq!(health_url("https://example.com/upload"), None);
    }
}
