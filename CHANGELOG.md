# Changelog

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
