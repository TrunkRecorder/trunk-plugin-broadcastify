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
use trunk_recorder_plugin::Host;
use trunk_recorder_plugin::Totals;
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

/// Sends a report: the service's answer, or why it wasn't taken (and what to say about it).
fn send(agent: &ureq::Agent, url: &str, report: &Report) -> Result<Answer, (Failure, String)> {
    let mut r = match agent.post(url).header("Content-Type", "application/json").send(serde_json::to_vec(report).unwrap_or_default().as_slice()) {
        Ok(r) => r,
        Err(e) => return Err((Failure::Unreachable, e.to_string())),
    };
    let status = r.status().as_u16();
    let retry_after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
    let text = r.body_mut().read_to_string().unwrap_or_default();
    let said: String = text.trim().chars().take(200).collect();
    if let Some(f) = Failure::from_status(status, retry_after.as_deref()) {
        return Err((f, if said.is_empty() { format!("HTTP {status}") } else { format!("HTTP {status}: {said}") }));
    }
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) if v.is_object() => Ok(serde_json::from_value(v).unwrap_or_default()),
        // Not a report's answer: a page in the way, or a server that doesn't take reports.
        _ => Err((Failure::NotThere, format!("it answered \"{said}\""))),
    }
}

/// The reporting thread: give it the recorder's totals as they come.
pub struct Reports {
    tx: Option<Sender<Totals>>,
    thread: Option<JoinHandle<()>>,
}

impl Reports {
    pub fn start(host: Host, url: String, skip_certificate_check: bool, recorder: Recorder, nodes: Vec<Node>) -> Reports {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new().name("health".into()).spawn(move || run(host, url, skip_certificate_check, recorder, nodes, rx)).ok();
        Reports { tx: Some(tx), thread }
    }

    pub fn push(&self, h: Totals) {
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

fn run(host: Host, url: String, skip: bool, recorder: Recorder, nodes: Vec<Node>, rx: Receiver<Totals>) {
    let agent = crate::upload::agent(Duration::from_secs(20), skip);
    let mut nodes: Vec<(Node, Reporter, Option<String>)> = nodes
        .into_iter()
        .map(|n| {
            let r = Reporter::new(n.select.clone(), &n.system_id.to_string());
            (n, r, None)
        })
        .collect();
    let mut history = History::default();
    let (mut told_down, mut told_not_taken) = (false, false);
    while let Ok(h) = rx.recv() {
        history.push(h.clone());
        for (n, r, last_message) in nodes.iter_mut() {
            if !r.due(&h) {
                continue;
            }
            let node = json!({ "systemId": n.system_id, "apiKey": n.api_key });
            let Some(report) = r.report(&history, node, recorder.clone()) else { continue };
            match send(&agent, &url, &report) {
                Ok(answer) => {
                    (told_down, told_not_taken) = (false, false);
                    r.delivered(&report, answer.next_s);
                    // What Broadcastify makes of the node, when it says something new.
                    if let Some(m) = answer.message.filter(|m| !m.is_empty() && Some(m) != last_message.as_ref()) {
                        host.info(format!("Broadcastify on system {}: {m}", n.system_id));
                        *last_message = Some(m);
                    }
                }
                Err((f, why)) => {
                    match f {
                        Failure::Refused => host.warn(format!(
                            "Broadcastify refused the health report of system {} ({why}): not sending its reports until recording starts again",
                            n.system_id
                        )),
                        Failure::Rejected => host.warn(format!("Broadcastify turned down the health report of system {}: {why}", n.system_id)),
                        // (Said once; the Reporter tries again less and less often, up to hourly.)
                        Failure::NotThere if !told_not_taken => {
                            told_not_taken = true;
                            host.info(format!("{url} doesn't take node health reports ({why}); trying again less often"));
                        }
                        Failure::NotThere => {}
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
