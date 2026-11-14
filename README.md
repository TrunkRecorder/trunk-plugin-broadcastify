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
| **Send health reports** | Every 5 minutes, tell Broadcastify how well this recorder hears each system it uploads (see [Health reports](#health-reports)). On by default. |
| **Health report server** | Leave it empty: reports go beside the upload server (`…/call-health`). |

For each system:

| Setting | |
|---|---|
| **API key** | Your node's upload key. Leave it empty to not upload that system. |
| **System ID** | The system's number on Broadcastify Calls. |
| **Talkgroups** | **Every talkgroup**, **Only some** or **All but some**. |
| **Only these** | With **Only some**: upload these talkgroups and no others. |
| **Except these** | With **Only some** or **All but some**: never upload these. |
| **Sites of their own** | A site of the system that Broadcastify Calls knows as a system of its own: its name (as under Sites in Setup), its system ID, and its API key if not the system's. Its calls go there; the system's other sites' go to the system's. |

Talkgroups can be numbers, ranges or patterns: `1000-1099` is every talkgroup
from 1000 to 1099, `*` stands for any digits and `?` for one, so `507*` is
every talkgroup starting with 507 and `12???` every five-digit one starting
with 12. Separate them with commas or spaces. The plugin won't start with an
entry that's none of these, and says which. Lists saved before the menu still
apply (the log says so); choose the menu once to see them on the card.

## What it does with calls

For each recorded call of a system with an API key, it sends Broadcastify the
call's details (its call JSON: talkgroup, frequency, times, the radios heard)
and then its M4A audio. Encrypted calls are never sent.

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
clipping. Totals only: no talkgroups, radios or audio. When several nodes
upload one system, Broadcastify can take its calls from the one hearing it
best, and can tell a node that's up but has no calls from one that's down.
A site with a system ID of its own is reported to that ID alone.

Reports need Trunk Recorder Pro 0.3.8 or later (an older one doesn't send the
plugin what goes in them) and go on a thread of their own, so uploads never
wait on them. A report that fails is carried by the next. Until Broadcastify
takes them (an answer that isn't a report's), the plugin says so once and
stops sending them until recording starts again.

## Sites

In Trunk Recorder Pro a system can have several sites, and a call heard on
more than one is saved once, from the site that heard it best. Each call says
which site that was. When Broadcastify Calls has one system for all of them,
there is nothing to set. When it has a system for each site (as Trunk Recorder
set them up, a system per site), list those sites under **Sites of their
own**: a call goes to its site's system. A setup from before sites existed
became one system with a site per old system, keeping the first one's
Broadcastify settings; add the others' system IDs here.

## Coming from Trunk Recorder

Trunk Recorder's settings can be pasted in as they are:
`broadcastifyCallsServer`, `broadcastifyOTA` and `broadcastifySslVerifyDisable`
are the plugin's settings, and each system's `broadcastifyApiKey` and
`broadcastifySystemId` are its settings for that system. Its talkgroup lists
are set with the **Talkgroups** menu.
Trunk Recorder systems that are sites of one system (`multiSite`) are one
system here: give each site's system ID under **Sites of their own**.

## Building

```sh
cargo build --release
trunk-pro plugin run ./target/release/broadcastify ~/TrunkRecorderPro --settings examples/settings.json
```

## License

GPL-3.0-or-later, like Trunk Recorder.
