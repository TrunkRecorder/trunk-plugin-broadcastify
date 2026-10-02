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

For each system:

| Setting | |
|---|---|
| **API key** | Your node's upload key. Leave it empty to not upload that system. |
| **System ID** | The system's number on Broadcastify Calls. |
| **Only these talkgroups** | Upload these talkgroups and no others. Leave it empty to upload all of them. |
| **Not these talkgroups** | Never upload these talkgroups. |

Talkgroups can be numbers or patterns: `*` stands for any digits and `?` for
one, so `507*` is every talkgroup starting with 507 and `12???` is every
five-digit one starting with 12. When both lists are set, a talkgroup has to
be in the first and not in the second.

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

## Coming from Trunk Recorder

Trunk Recorder's settings can be pasted in as they are:
`broadcastifyCallsServer`, `broadcastifyOTA` and `broadcastifySslVerifyDisable`
are the plugin's settings, and each system's `broadcastifyApiKey`,
`broadcastifySystemId`, `broadcastifyAllow` and `broadcastifyDeny` (or the
older `…Whitelist` / `…Blacklist` names) are its settings for that system.

## Building

```sh
cargo build --release
trunk-pro plugin run ./target/release/broadcastify ~/TrunkRecorderPro --settings examples/settings.json
```

## License

GPL-3.0-or-later, like Trunk Recorder.
