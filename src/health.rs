//! Node health reports to Broadcastify Calls (`call-health`): every 5 minutes, at a random point
//! of each node's own, how well this recorder hears each system ID it uploads. Broadcastify uses
//! them to tell a node that's up but quiet from one that's down, and to prefer the node hearing a
//! system best when several upload it. The report's format is the recorder's
//! (`docs/api/node-health.md`); this adds where it goes and the node's ID and key.
//!
//! Reports go from a thread of their own, so an upload never waits on one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::json;
use trunk_recorder_plugin::Host;
use trunk_recorder_plugin::Totals;
use trunk_recorder_plugin::health::{Answer, Failure, History, Recorder, Report, Reporter, Select};

use crate::sites::{ADVICE, Covers, Notices, several};

/// One node: a system ID and its key, and the part of the recorder it is.
pub struct Node {
    pub system_id: u32,
    pub api_key: String,
    pub select: Select,
}

/// A recorder system's nodes: one for each Broadcastify system ID it uploads as, its report
/// covering every site that uploads as that ID (never one report per site: Broadcastify takes one
/// report per system ID each interval). `own`: the system's ID and key (its sites without settings
/// of their own go with it); `site_nodes`: the sites with an ID of their own, and that ID and key.
/// Also what to tell the user about them.
pub fn nodes_of(short_name: &str, own: Option<(u32, &str)>, site_nodes: &[(&str, (u32, &str))]) -> (Vec<Node>, Vec<String>) {
    let mut notes = Vec::new();
    // The other IDs: each one's key (the first given) and sites.
    let mut others: BTreeMap<u32, (&str, Vec<&str>)> = BTreeMap::new();
    let mut keys_differ = BTreeSet::new();
    for &(site, (id, key)) in site_nodes {
        if own.is_some_and(|(o, _)| o == id) {
            if own.is_some_and(|(_, k)| k != key) {
                keys_differ.insert(id);
            }
            continue;
        }
        let (first, of_id) = others.entry(id).or_insert((key, Vec::new()));
        if *first != key {
            keys_differ.insert(id);
        }
        of_id.push(site);
    }
    for id in keys_differ {
        notes.push(format!(
            "{short_name}'s settings give Broadcastify system {id} more than one API key: its health reports use the first. Give it the one key it has."
        ));
    }
    let mut nodes = Vec::new();
    if let Some((id, key)) = own {
        let mut select = Select::system(short_name);
        for s in others.values().flat_map(|(_, ss)| ss) {
            select = select.except(s);
        }
        nodes.push(Node { system_id: id, api_key: key.to_string(), select });
    }
    for (id, (key, ss)) in others {
        let mut select = Select::system(short_name);
        select.only = Some(ss.iter().map(|s| s.to_string()).collect());
        nodes.push(Node { system_id: id, api_key: key.to_string(), select });
    }
    (nodes, notes)
}

/// One node for each system ID going to a server. A report covers one recorder system, so an ID
/// that two of them upload as is reported for the first alone (the plugin has said, at start, that
/// it uploads several sites: [`crate::sites`]).
pub fn one_per_id(nodes: Vec<Node>) -> Vec<Node> {
    let mut kept: Vec<Node> = Vec::new();
    for n in nodes {
        if !kept.iter().any(|k| k.system_id == n.system_id) {
            kept.push(n);
        }
    }
    kept
}

/// The sites a report names, by name: one for each identity (receivers of one site are one site),
/// leaving out those whose identity isn't known yet.
fn distinct_sites(r: &Report) -> Vec<String> {
    let mut seen: Vec<&BTreeMap<String, u32>> = Vec::new();
    let mut names = Vec::new();
    for s in r.sites.iter().filter(|s| !s.id.is_empty()) {
        if !seen.contains(&&s.id) {
            seen.push(&s.id);
            names.push(s.name.clone());
        }
    }
    names
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
    /// `notices`: system IDs uploading several sites, to add to when a report shows one (sites can
    /// appear while recording).
    pub fn start(host: Host, url: String, skip_certificate_check: bool, recorder: Recorder, nodes: Vec<Node>, notices: Notices) -> Reports {
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new().name("health".into()).spawn(move || run(host, url, skip_certificate_check, recorder, nodes, notices, rx)).ok();
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

fn run(host: Host, url: String, skip: bool, recorder: Recorder, nodes: Vec<Node>, notices: Notices, rx: Receiver<Totals>) {
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
            // No time in it: the plugin was (re)started on a recording already under way, and has
            // only these totals. The next ones give it a window (it's still due).
            if report.window.end <= report.window.start {
                continue;
            }
            // Several sites under one ID that the settings didn't show (sites that appeared since).
            let named = distinct_sites(&report);
            if named.len() > 1 {
                let on: Covers = named.into_iter().map(|s| (n.select.short_name.clone(), Some(s))).collect();
                let what = several(n.system_id, &on);
                if notices.add(&host, n.system_id, what.clone()) {
                    host.warn(format!("{what} {ADVICE}"));
                }
            }
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
