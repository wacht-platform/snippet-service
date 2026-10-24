# Snippet Ambient: architecture draft

The goal is an assistant that is always listening (and later always looking). It should keep a record of your day, act when you address it, and work out follow-ups without being asked. The phone comes first and hardware later, all on the same backend.

## Principles

1. **Devices only capture, the daemon does the thinking.** A device's job is to capture, detect speech, queue and upload. The daemon handles transcription, memory and agents. Replacing the phone with a $15 board then changes only the front end.
2. **One ingest contract.** Phone, desk node and wearable all post the same kind of event. A device is just a source id.
3. **The ambient stream lives outside agent context.** A day of talk is 20k–60k tokens, and pushing it into a session repeats the Claude usage drain. Agents search the stream when they need it.
4. **Cheap by default.** Speech-only capture, a small model for triage, and the expensive model only once something needs acting on.

## Layers

```
 DEVICE                     DAEMON (snippet serve)                      AGENTS
 ------                     ----------------------                      ------
 mic ─▶ VAD ─▶ utterance ──▶ WS /ambient/stream ─▶ STT ─▶ ambient log ─┬▶ wake command ─▶ Mission Control
          └─▶ wake word ─▶ (priority flag)                    (sqlite+FTS)├▶ triage (cheap) ─▶ notes / tasks / memory
 camera ─▶ change detect ─▶ frame ─▶ same endpoint ─▶ caption ─┘          └▶ search tool for any agent
                                                                          │
 reply ◀── push notification / TTS / watch ◀──────────────────────────────┘
```

| Layer | Job | Phase 1 (phone) | Later |
|---|---|---|---|
| Capture | Mic always on, camera sometimes | Android foreground service | ESP32-S3 Sense over BLE, phone relays |
| Edge gate | Drop silence, mark wake | On-device VAD (Silero, ~2 MB) | Same model on a desk node; energy VAD on ESP32 |
| Transport | Get clips to the daemon, survive being offline | Opus clips over HTTPS, local queue | Same |
| Ingest | Store, dedupe, order | `WS /ambient/stream` | Same |
| Understanding | Audio to text, frames to captions | See decision D | Speaker labels |
| Memory | Durable, searchable record | `ambient_events` table + FTS | Embeddings |
| Agents | Act, digest, recall | Wake commands to MC, hourly triage | Proactive nudges |
| Output | Talk back | Push notification + TTS | Watch haptics, earbud audio |

## Decisions to make

**A. Where speech detection runs: on the device.**
It saves battery, data and transcription cost, since roughly 70–90% of the day is silence or noise. Silero VAD runs comfortably on any phone.

**B. How it knows you're talking to it**
- *Wake word on the device* (openWakeWord, free; Porcupine, paid). Responds instantly and is easy to understand. The catch: "Snippet" needs a custom-trained model.
- *From the transcript*: look for "snippet, …" after transcription. It needs no extra model, but replies lag by the transcription time (2–10 s).
- Proposal: start from the transcript (it's free and already in the pipeline) and add an on-device wake word when replies need to feel live.

**C. Transport: clips vs a live stream**
- *Clips* (one upload per utterance, 1–60 s). Simple, resumable, and they work over a flaky connection. Fine for the record and for digests.
- *Live stream* (WebSocket audio). Needed only for back-and-forth conversation.
- Proposal: clips for everything at first. Only a wake turn opens a stream, later.

**D. Where transcription runs**

| Option | Cost | Quality | Notes |
|---|---|---|---|
| On the phone (whisper.cpp tiny/base) | Free | OK in English, weak on Hindi/Hinglish | Battery and heat; no server load |
| On this server (whisper.cpp base/small) | Free | Good with small | 4 vCPU, no GPU: small runs at about real time and competes with agents |
| AssemblyAI (already wired) | ~$0.15–0.40 per hour of speech | Best, handles mixed languages | Roughly $0.50–1.50/day at 2–4 h of speech |
| Groq Whisper large-v3-turbo | ~$0.04 per hour | Very good | Cheapest hosted option; new dependency |

The deciding factor is probably Hinglish: small local models handle it poorly.

**E. Memory model**
Ambient data gets its own store, not a session:
- `ambient_events(id, device, started_at, duration, kind[speech|frame|wake], text, speaker, confidence)` with full-text search
- Raw audio deleted after transcription, unless you choose to keep it for a while
- An `ambient_search(query, from, to)` tool for any agent ("what did I say about sponsors on Tuesday?")

**F. Which agent handles it**
- An **ambient triage agent** on a cheap model reads new events every N minutes. It writes notes and follow-ups, and passes anything that needs action to Mission Control.
- **Wake commands** go straight to Mission Control (or a chosen agent) as a message, with the last ~2 minutes of transcript as context.
- This keeps Mission Control's context clean and the cost low.

**G. Vision (desk node first)**
The camera captures a frame when the scene changes, plus one every few minutes at most. A vision model captions it, and the caption goes into the same event log. Raw video is never streamed, which is the only way the power budget works on anything with a battery.

**H. Privacy**
- A visible indicator and a one-tap mute on the phone (and a hardware switch later).
- Raw audio is never sent anywhere except the daemon and the chosen transcription service.
- People nearby: delete audio by default, and keep transcripts only on your own machine.

## Hardware path, same backend

| Device | Capture | Edge | Link | Cost |
|---|---|---|---|---|
| Phone (phase 1) | Mic | VAD in the app | HTTPS | $0 |
| Desk node: old phone or Pi + webcam | Mic + camera | VAD + scene change | HTTPS, wall power | $0–40 |
| Wearable: XIAO ESP32-S3 Sense | Mic (+ camera snapshots) | Energy VAD | BLE to the phone app, which relays | ~$15 |
| Glasses (much later) | Mic + POV camera | Sampled | BLE/Wi-Fi | n/a |


## Decisions so far

1. **Mission Control owns the policy.** It sets the interruption budget, cooldowns, channel ladder and which moments matter, and decides what to do with each one. It learns from accepted, dismissed and ignored nudges.
2. **The ambient worker only produces moments.** Capture, transcription, storage and the cheap voice classifier run outside Mission Control's context. Mission Control sees distilled moments, urgent ones wake it, and it can search the full log.
3. **Shadow mode first.** The voice classifier logs what it would have said and you rate it. Nudges go live from the quietest channel up once precision is good.
4. **One capture core in Dart.** The app already ships on Android, macOS, Windows and Linux, so recording, voice detection, the retry queue and upload are written once. Each platform only adds its own shell.
5. **The voice classifier (TypeSafe) is a candidate, not a decision.** Latency, price and the privacy of sending transcripts are unchecked.

## Platforms

| Platform | Shell around the shared core | Main limits |
|---|---|---|
| Android | Foreground service, microphone type, start from the app once | OEM battery killers; other apps silence it; no call audio |
| macOS | Menu-bar app, login item; optional system audio capture | Sleep stops it; Developer ID and notarization for a clean install |
| Windows | Tray app, start at login; WASAPI loopback for system audio | Sleep; SmartScreen warning unless signed |
| Linux | Tray or systemd user service; PipeWire monitor for system audio | Needs a real audio device; `record` support on Linux unverified |

## Build plan

| # | Step | Where | Status |
|---|---|---|---|
| 1 | Ingest endpoint `WS /ambient/stream` and `ambient_events` store with search | snippet-service | not started |
| 2 | Transcription behind the ingest, using the existing AssemblyAI path, pluggable later | snippet-service | not started |
| 3 | Capture core: record, voice detection, local queue, upload with retry | snippet-mobile | not started |
| 4 | Desktop shells: Mac, Linux, Windows (tray, login item, mute) | snippet-mobile | not started |
| 5 | Android foreground service, battery exemption prompt, mute | snippet-mobile | not started |
| 6 | One-day soak test per platform: gaps, battery, upload failures | all | not started |
| 7 | Voice classifier in shadow mode, moments table, review screen | both | not started |
| 8 | Mission Control integration: moments as wakes, policy in its memory, feedback loop | snippet-service | not started |
| 9 | Live nudges up the channel ladder | both | not started |

## Open questions

1. Language: English only, or Hindi/Hinglish too? Decides local versus hosted transcription.
2. Keep raw audio after transcription, or delete it?
3. May it listen through calls and meetings with other people?
4. Where do the first nudges land: phone, watch or earbuds?
