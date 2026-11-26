# Broadcastify Calls for Trunk Recorder Pro

Uploads the calls [Trunk Recorder Pro](https://github.com/TrunkRecorder/trunk-recorder-pro)
records to [Broadcastify Calls](https://www.broadcastify.com/calls/). It does
what Trunk Recorder's built-in Broadcastify uploader does.

## What it needs

- **A Broadcastify Calls node**, with an API key and a system ID for each
  system you upload. Broadcastify gives you these when it approves your node.
- **An M4A encoder.** Broadcastify takes AAC audio, not WAV. On macOS one is
  built in. On Linux and Windows, install [ffmpeg](https://ffmpeg.org) (on
  Debian and Raspberry Pi OS: `sudo apt install ffmpeg`). Without one, the
  plugin won't start, and says so.

## Settings

| Setting | |
|---|---|
| **Upload server** | Leave it as it is, unless Broadcastify tells you to change it. |
| **Send talker aliases** | When the first radio on a call sent its name over the air, send that too. On by default. |
| **Skip certificate checks** | Upload even when Broadcastify's certificate has expired. Leave it off unless uploads fail with a certificate error. |
| **Send health reports** | Every 5 minutes, tell Broadcastify how well this recorder hears each system it uploads (see [Health reports](#health-reports)). On by default. Leave it on: without them Broadcastify can't compare your node with others, so it may get a lower priority or be left out, and can't tell you about problems with it (see [Turning them off](#turning-them-off)). |
| **Health report server** | Leave it empty: reports go beside the upload server (`…/call-health`). |

A system can set each of these again for itself (its *shared settings* on its card in Setup).

For each system:

| Setting | |
|---|---|
| **API key** | Your node's upload key. Leave it empty to not upload that system. |
| **System ID** | The system's number on Broadcastify Calls. |
| **Talkgroups** | **Every talkgroup**, **Only some** or **All but some**. |
| **Only these** | With **Only some**: upload these talkgroups and no others. |
| **Except these** | With **Only some** or **All but some**: never upload these. |

A system with several sites has a row for each of them on its card in Setup. Each site needs its own
Broadcastify system ID, given in its row as its **System ID** (and **API key**, if not the system's):
every call it heard goes to it, whichever site's copy was kept, and a call two such sites heard goes
to both. A call only the system's other sites heard goes to the system's. With no API key for the
whole system, only the calls a site of its own heard are uploaded. **Sites** on the card turns the
plugin off for a site: a call only it heard isn't uploaded.

**If one system ID covers several sites, the plugin warns you**, on its card and in its log:
Broadcastify Calls records every call a system ID uploads as coming from the site it's registered
to, so the other sites' calls would be labeled as the wrong site. That's one system ID for a system
with several sites and no IDs on their rows, one ID typed into several site rows, or one ID on
several systems. Uploads go on as they are. A site that appears while recording is caught from the
health reports. If you need more system IDs, apply at
[broadcastify.com/apply/calls](https://www.broadcastify.com/apply/calls) (you can use your existing
API key). The warning on the card needs Trunk Recorder Pro after 0.5.0; earlier
versions show it in the log only.

On a conventional node, Broadcastify knows each channel by its slot, listed on
the node's page at [broadcastify.com/calls/manage](https://www.broadcastify.com/calls/manage/):
set each channel's **TG Number** in the recorder to its slot.

Talkgroups can be numbers, ranges or patterns: `1000-1099` is every talkgroup
from 1000 to 1099, `*` stands for any digits and `?` for one, so `507*` is
every talkgroup starting with 507 and `12???` every five-digit one starting
with 12. Separate them with commas or spaces. The plugin won't start with an
entry that's none of these, and says which. Lists saved before the menu still
apply (the log says so); choose the menu once to see them on the card.

## What it does with calls

For each recorded call of a system with an API key, it sends Broadcastify the
call's details (its call JSON: talkgroup, frequency, times, the radios heard)
and then its M4A audio. Encrypted calls are never sent. The details also go
as separate fields (`tg`, `freq`, `ts`, `src`), so a node Broadcastify set up
for another recorder, such as one shared with SDRTrunk or Trunking Recorder,
takes the calls too.

Broadcastify judges how far behind a node is from the end time of each call.
A call ends a few seconds after its last transmission, so (as Trunk Recorder
does) the times sent to Broadcastify are those of the call's audio played back
to back, ending when the call was saved. The files on disk keep the real
times.

If Broadcastify can't be reached, the plugin tries the call again after 10
seconds, 1 minute, 5 minutes and 15 minutes, and shows how many calls are
waiting. Calls still waiting when recording stops are kept, and sent when it
starts again. A call Broadcastify answers `SKIPPED` is marked skipped; one it
answers `REJECTED` is marked failed and not tried again.

**All skipped as already received?** Another node uploads the same system or
site, and Broadcastify keeps one copy of each call. Nothing is wrong: this node
is a backup, and Broadcastify takes its copies if the other node stops. See
[When an upload fails](https://github.com/TrunkRecorder/trunk-recorder-pro/blob/main/docs/plugins/broadcastify.md#when-an-upload-fails).

When Broadcastify turns down a system's settings (`1 Invalid-API-Key`, no API
key, an unknown system ID, a key not allowed to upload that system), its calls
fail at once instead of waiting on retries, and the plugin's status says what
to fix, e.g. "Broadcastify refused the API key for dcfd — check the system's
API key". (Trunk Recorder tries these calls again like any other failure, so a
mistyped key looks like Broadcastify being down.)

## Health reports

Every 5 minutes, at a random point of its own, each node (a system ID) sends
Broadcastify a [node health report](https://github.com/TrunkRecorder/trunk-recorder-pro/blob/main/docs/api/node-health.md):
how many seconds the recorder was decoding the system's control channel, the
control messages it decoded and lost, the calls the system announced and why
any weren't recorded (no radio covering the frequency, every recorder busy),
and the voice frames it lost, along with its radios' dropped samples and
clipping. When several nodes upload one system, Broadcastify can take its
calls from the one hearing it best, and can tell a node that's up but has no
calls from one that's down.
A site with a system ID of its own is reported to that ID alone. Sites left on
one system ID are one report (Broadcastify takes one report per system ID each
interval); two recorder systems on one ID are reported for the first.

A system or site the plugin is turned off for in Trunk Recorder Pro is neither
uploaded nor reported, nor is one with no API key and system ID. (Pro 0.4.1 and
earlier keep a plugin taking `totals` on for every system and site.)

Reports need Trunk Recorder Pro 0.3.9 or later (an older one doesn't send the
plugin the `totals` they're made of) and go on a thread of their own, so uploads never
wait on them. A report that fails is carried by the next. When the server
doesn't take them (not found, or an answer that isn't a report's), the plugin
says so once and tries again less and less often, up to once an hour.

### What a report contains

Counts of what the recorder heard, not scores:

- **For each system or site:** the identity its control channel broadcasts (WACN, System ID, RFSS,
  site, NAC) and the control channel's frequency; the seconds spent decoding it, the messages
  decoded and lost, its signal-to-noise ratio and phase error; the calls announced, how many were
  recorded and why any weren't; the voice frames received and lost.
- **For each conventional channel:** its frequency and the name you gave it, the time it was heard
  and open, its levels, and its calls.
- **For each radio (SDR) those use:** the name you gave it, its kind (`airspy`, `hackrf`, ...),
  seconds of samples dropped, clipping and frequency error.
- **About the recorder:** its name and version, how long it has been recording, and how far the
  computer's clock is from the radio system's.
- **Who sends it:** the node's system ID and API key, as every upload does. Broadcastify doesn't
  keep the key with the report.

Nothing about people: no talkgroups, radio IDs, talker aliases or audio.

### Turning them off

Untick **Send health reports** (`healthReports: false`). Uploads carry on as before, but
Broadcastify relies on these reports to look after your node, and without them:

- **Your node can't be compared with others.** When several nodes upload the same system,
  Broadcastify chooses whose calls to take by how well each one hears it. A node that doesn't
  report can't be compared, so it may be given a lower priority or left out.
- **Broadcastify can't tell you about problems.** It can't warn you of an outage, a lost control
  channel, rising voice loss or other trends on your node, and it can't tell a node that's quiet
  from one that's down.
- **Support can't see how your node has been doing** when you ask for help with it.

We recommend leaving them on.

## Sites

In Trunk Recorder Pro a system can have several sites, and a call heard on
more than one is saved once, from the site that heard it best. Each call says
which site that was. Broadcastify Calls has a system ID for each site (as
Trunk Recorder set them up, a system per site): give each site its own in its
row on the system's card, and a call goes to the system of every site that heard
it. Sites left on one system ID are uploaded as one, and the plugin warns you. A setup from before sites existed
became one system with a site per old system, keeping the first one's
Broadcastify settings; add the others' system IDs here.

## Coming from Trunk Recorder

Trunk Recorder's settings can be pasted in as they are:
`broadcastifyCallsServer`, `broadcastifyOTA` and `broadcastifySslVerifyDisable`
are the plugin's settings, and each system's `broadcastifyApiKey` and
`broadcastifySystemId` are its settings for that system. Its talkgroup lists
are set with the **Talkgroups** menu.
Trunk Recorder systems that are sites of one system (`multiSite`) are one
system here: give each site its system ID in its row on the system's card.

## Building

```sh
cargo build --release
trunk-pro plugin run ./target/release/broadcastify ~/TrunkRecorderPro --settings examples/settings.json
```

## License

GPL-3.0-or-later, like Trunk Recorder.
