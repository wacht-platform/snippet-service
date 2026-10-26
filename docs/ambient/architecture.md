# Ambient voice

An always-on voice assistant. Devices only capture and play; the daemon transcribes, classifies, routes to Mission Control (MC) and speaks the replies.

## Pipeline

1. **Capture.** The phone or desktop app gates microphone audio with Silero VAD and streams only speech over the `/ambient/stream` websocket. Echo cancellation and a mic self-mute during playback keep the assistant from hearing itself.
2. **Listening.** `serve/listener.rs` holds one streaming STT session per device (Deepgram Flux or AssemblyAI, chosen by `[ambient] stt`) and masks words that match the assistant's own recent speech. Events are stored in `ambient_events`.
3. **Routing.** `serve/ambient_router.rs` keeps an ordered queue per device, merges fragments into turns (gap of 1.5 s or less), runs the voice classifier (`serve/ambient_watch.rs`) and decides whether MC sees the turn. A wake phrase is delivered immediately with a short classifier veto.
4. **Mission Control.** Delivered turns arrive as `<ambient_turn>` envelopes. MC answers with `speak`, switches modes with `ambient_mode`, hints the classifier with `ambient_expect` or `speak(expecting)`, and can search the stream with `ambient_search`.
5. **Speech.** `speech.rs` queues `speak` jobs and `tts_stream.rs` streams Deepgram Flux TTS to the phone over a duplex socket. When no duplex socket is available, replies fall back to per-clause PCM chunks served from `/ambient/turns/{id}/chunks/{seq}`.

## Modes

| Mode | Entered by | Behaviour |
|---|---|---|
| Off | User, or lease lost | Nothing is captured. |
| Passive | Default; `ambient_mode("passive")`; idle timeout | Silent notes. Addressed speech reaches MC as chat and MC cannot speak. |
| Active | `ambient_mode("active")`; an addressed line while Pinned | Spoken back and forth. Nearly every kept user turn reaches MC, which may stay silent. |
| Pinned | User only | Resting state of Active: addressed lines open a conversation without an MC decision. |

MC alone decides when a spoken conversation opens; the classifier only advises. The idle timer runs from the end of the last playback and pauses while MC is thinking or speaking.

## Delivery rules

- `media`, `background` and `noise` lines are never delivered, but stay searchable.
- Passive delivers a wake phrase, an addressed line, a line matching MC's stated expectation, or a continuation of a delivered turn.
- Active delivers every kept user turn except intent `ignore`, with an `addressed` score so MC can choose silence.
- Repeated out-of-context turns during a conversation raise an `<ambient_context_alert>` to MC, which can stay silent, schedule a check-in with `schedule_wake`, or return to Passive.

## Interruption

The microphone stays open during playback. The phone ducks playback when it hears speech and the server cancels the reply once a barge-in is confirmed; the words actually heard are stored with the turn.

## Devices

One device holds the listening lease at a time (`/ambient/lease`, 45 s TTL, heartbeat every 15 s). Ingest from another device is rejected and a displaced device stops capturing.

## Privacy

The stream stays out of agent context. MC searches it on demand and receives only routed turns plus redacted context.
