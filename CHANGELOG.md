# Changelog

## [0.1.5]

- A system's sites: Trunk Recorder Pro now has systems with several sites, and a
  call heard on more than one is saved once, saying which site it was kept
  from. A site that Broadcastify Calls knows as a system of its own can have
  its own system ID (and API key) under **Sites of their own**; its calls go
  there.
- Built with trunk-recorder-plugin 0.4.0.

## [0.1.4]

- A system can override the upload server, talker aliases and the certificate check; each server has its own upload queue, so one that's down doesn't hold up the others.
- Built with trunk-recorder-plugin 0.3.0.

## [0.1.3]

- A system whose API key or system ID Broadcastify turns down (`1 Invalid-API-Key`,
  `100 NO-API-KEY-SPECIFIED`, an unknown system, a key not allowed to upload it)
  fails its calls at once instead of retrying them for 20 minutes, and the
  plugin's status says which system's settings to check. Trunk Recorder retries
  these; a mistyped key looked like Broadcastify being down.

## [0.1.2]

- Knows systems by their short name (trunk-recorder-plugin 0.2.1).

## [0.1.1]

- The API key and System ID are marked as needed for each system, so the recorder shows which systems aren't set up for Broadcastify yet.
- Built with trunk-recorder-plugin 0.1.1.

## [0.1.0]

- Uploads recorded calls to Broadcastify Calls as Trunk Recorder's uploader
  does: the call JSON first, then the M4A to the address Broadcastify answers
  with.
- Sends call times that end when the call was saved, as Trunk Recorder does,
  so Broadcastify doesn't count the end of a call as lag.
- Talker aliases, talkgroup allow and deny lists (with `*` and `?`), and an
  option to skip certificate checks.
- Retries calls Broadcastify couldn't take after 10 s, 1 min, 5 min and
  15 min, and keeps calls still waiting when recording stops for the next start.
- Reads Trunk Recorder's setting names as well as its own.
