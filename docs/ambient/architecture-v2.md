# Ambient voice: target architecture (v2)

Status: design agreed in review, not yet built. Evidence was taken from the real event log (163 events, 25 ever reached Mission Control) and from reading both repos.

## Principles

1. Capture and assembly build whole, ordered turns. The voice classifier labels a turn. A server-owned router decides what Mission Control (MC) sees. These three jobs are separate.
2. MC decides when a spoken conversation opens. The classifier only advises. A wake phrase in Passive becomes a chat message, not a spoken conversation.
3. Nothing the user said is dropped for being a fragment. Everything is stored; routing works on assembled turns.

## Modes

| State | Entered by | Behaviour |
|---|---|---|
| Off | User, or lease lost | Nothing. |
| Passive (default) | User; MC `ambient_mode("passive")`; idle timeout | Silent note taking and delegation. Addressed speech reaches MC as chat; MC has no `speak`; one notification per MC reply. |
| Active | MC `ambient_mode("active")`; an addressed line while Pinned | Turn by turn speech. Nearly every user-voice turn reaches MC. MC may stay silent or return to Passive. |
| Pinned | User only (stage swipe) | Resting state of Active: addressed lines open a conversation without an MC decision. |

The idle timer runs from the end of the last playback, is paused while MC is thinking (cap 5 min) or speaking, and a `speak` answering an outstanding turn is honoured for up to 5 min after timeout. The state lives in the database, not in process statics, and changes are pushed as a device event.

## Routing

- `media`, `background` and `noise` lines are never delivered (still searchable). Lines from other people only are held back and ride along as context.
- Passive delivers: a wake phrase, an addressed line (>= 0.7), a line matching MC's stated expectation, or a continuation of a delivered turn.
- Active delivers every kept user-voice turn except intent `ignore`, with `addressed` so MC can choose silence.
- Envelope: `<ambient_turn mode ids device at parts speaker speakers judged addressed expects suggest wake late trust>` with only undelivered lines as `<context>`.

## Pause-split fragments

Server turn assembler per device, in sequence order: merge parts when the gap is 1.5 s or less and speakers agree, hold until the next part or an activity ping, classify the merged turn once, and send a late part as a continuation. Phone: 800 ms hangover, a speech-start ping, a sequence number per segment, and an overlapped cut at the 60 s cap.

## Voice classifier contract

Inputs: the assembled turn, audio length, speaker summary, mode and conversation state, the last 6 lines from the same device within 5 min ordered by start time (including dropped ones with their verdict), the last 2 spoken replies, MC's expectation (`answer`, `confirmation`, `dictation`, `choice`, `none` plus what about), wake phrases, and optionally the MC chat tail. Outputs: `kind`, `intent`, `addressed`, `matches_expectation`, `continues_previous`, `suggest_conversation`. MC sets the expectation with `speak(expecting, about)` or `ambient_expect`, and `ambient_mode` replaces `ambient_passive`.

## Interruptions and streaming voice

- The phone keeps the microphone open during playback, with platform echo cancellation, a reply-text echo filter on the server and an assistant-voice speaker. Barge-in ducks playback at 250 ms of speech and stops at 400 ms; the server records what was heard (`heard_text`).
- `speak` returns immediately and queues a speech job. Lines arriving while MC is mid-turn collapse into one pending envelope attached to the next tool result.
- Replies are synthesised per clause and delivered as numbered PCM chunks (control events on the existing socket, one GET per chunk, acknowledgements per chunk). The phone plays them through a gapless PCM sink.
- Provider streaming of partial `speak` text is a later stage: the ChatGPT Responses provider already sends the events but the parser ignores them; Anthropic needs `eager_input_streaming`; Gemini cannot on the API-key path.

## Staged plan

0. Stop the losses: staleness from segment end, deliver every kept line while a conversation is open, 0.5 s lead padding for word transcription, classifier history by device and start time including dropped lines. (S)
1. Rename to voice classifier everywhere user-visible, in prompts and docs, with a config alias; tell MC the classifier exists. (S)
2. Microphone stays open with echo protection; barge-in stops local playback. (M)
3. Non-blocking `speak`, mid-run delivery for every CLI kind, hold ambient lines while a question is pending. (M)
4. Ordered routing queue per device. (M)
5. Turn assembler and activity ping. (M/L)
6. Chunked streaming synthesis and delivery, `ambient_turns` status and `heard_text`. (L)
7. Gapless PCM playback and true barge-in. (M)
8. State machine and tools (`ambient_mode`, `speak(expecting)`, `ambient_expect`). (M)
9. Passive routing and per-reply notifications. (M)
10. Classifier contract v2. (M)
11. Safety (hold while a question is pending, soft speaker gating), retention, lease backlog. (S/M)
12. Provider level partial `speak` text. (M)
