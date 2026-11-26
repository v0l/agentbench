# Voice

The supervisor chat can be spoken to. Voice mode (settings, conversation) reads each reply aloud a sentence at a time. Pressing the talk key while it reads cuts the reply off.

## Hands free

With hands free on, the microphone stays open while the supervisor chat is the open chat. You do not press anything:

- Speech is found with the same band and hang logic as a normal take (`Endpoint` in `crates/app/src/voice/mod.rs`). Room noise before speech is dropped, keeping one second of lead in.
- When speech starts, the reader stops and the queued sentences are dropped at once. That is barge in, the same as `App::barge_in`.
- When you go quiet for the hang time, the take is read and sent as a turn, and the microphone stays open for the next one.
- Leaving the chat closes the microphone. The talk key, or the `stop listening` key in the composer, pauses it until you press the talk key again or come back to the chat.
- The chat header shows `listening`, or `hearing you` while you speak.

### Headphones

Through speakers, the reply would feed back into the open microphone and barge in on itself. Hands free checks the default output first:

- On Linux it asks `pactl` (the active port of the default sink, then its form factor), and falls back to `wpctl inspect @DEFAULT_AUDIO_SINK@` (form factor, the bluetooth headset profile, HDMI in the name).
- Headphones and headsets go ahead. Speakers, HDMI and TVs are refused with a notice that names the device.
- When the type cannot be known (an audio interface such as a Focusrite, or macOS and Windows for now), it asks once whether you are on headphones and keeps the answer (`voice_headphones`). The answer can be changed in settings under `listening through`.
- With echo cancelling on, speakers are allowed.

## Echo cancelling

The setting is `echo cancelling`, off by default.

### Choice

| option | what it is | fit |
| --- | --- | --- |
| `webrtc-audio-processing` | bindings to the WebRTC audio processing module (AEC3) | Best canceller. Needs meson, ninja and a C++ build of the WebRTC code (the `bundled` feature) or the system library. The licence field is non standard (BSD style WebRTC licence). Android and Windows builds need the meson cross setup. |
| `aec-rs` | bindings to speexdsp's MDF canceller | MIT. Builds speexdsp C through cmake. Lists Android, macOS, Windows and wasm. |
| our own, `crates/app/src/voice/echo.rs` | a partitioned block frequency domain adaptive filter (the MDF family speexdsp uses), with two filters and a convergence gate | Pure Rust, no new dependency, builds everywhere the app does. |

We went with our own. The app builds for Linux, macOS, Windows, Android and wasm from one Cargo tree with no C++ toolchain beyond what cpal needs. Each binding adds a native build step to every one of those targets. MDF is a few hundred lines, and the test below holds it to the bar. If real rooms show it falls short (non linear speakers, long reverb), `webrtc-audio-processing` is the next step, behind a cargo feature for desktop.

### How it works

- The reader's output callback copies each frame it plays into a shared reference buffer (`echo::PLAYED`, two seconds at the output rate).
- Each tick, the open take hands the samples it has not cancelled yet, plus the same number of reference samples resampled to the microphone rate, to `Canceller::process`.
- The filter covers a 128 ms echo tail in 128 sample blocks. A background filter adapts with a per bin normalised step. Its taps are copied to the foreground filter when its error is clearly lower, and the copy is undone when the background filter diverges during double talk.
- Until the filter has removed 12 dB for 40 blocks in a row, the output is muted while a reply plays, so a cold filter cannot pass the reply off as speech. After that, a block whose residual is under a quarter of the estimated echo is cut by 20 dB.

### Measured

`voice::echo::tests::a_reply_played_into_the_room_is_cancelled_and_the_voice_is_kept` plays the recorded reply (`speech.wav`, 4.2 s) twice into a simulated room (25 ms delay, 30 ms reverb tail), and talks over the second half of the second play with the recorded question (`ask.wav`):

| measure | needed | got |
| --- | --- | --- |
| reply removed while it plays alone (second play) | 20 dB | 54.6 dB |
| voice kept during double talk (1 minus rms of the residual over rms of the voice) | 90 percent | 97.9 percent |

From a cold start the filter is muted for the first 1.5 s, then removes 27 to 48 dB within the first play.

`voice::native::tests::with_echo_cancelling_on_the_reply_heard_through_speakers_is_not_speech` runs the open take with a reply fed through the reference and its echo into the fake microphone. Without cancelling the echo passes for speech. With it, it does not.

Not yet measured: a real speaker and microphone in a real room, and the sound card clock drift between output and input over long sessions. Until that is done the setting stays off by default.

## Duplex model (Haru)

A full duplex model listens and speaks at once: one 133 ms frame in (3200 samples at 24 kHz), one frame of audio and text out. inference-bench serves it on `/v1/duplex` (`docs/specrt-haru.md` there, section 14).

### Settings

Under voice input, with voice mode and hands free on, the `duplex model` section has:

- `duplex server`: the inference-bench address, such as `http://gpu-box:8080`. Empty keeps today's path: speech to text, the supervisor, read aloud.
- `duplex model name` and `duplex key`: sent as `model` and as a bearer token.
- `replies in its voice`: on, the supervisor's replies are spoken by the model; off, they are read aloud as before and the model only listens and backchannels (the fallback of section 10 if constrained text proves hard to follow).
- `voice clip`: press record, speak, press stop. Up to 30 s is kept as `duplex-voice.wav` in the agentbench home and sent as the voice reference at the start of each session.

### What happens

- The microphone stays open as in hands free, through the same headphones check and echo canceller. Its samples, after cancelling, are resampled to 24 kHz and sent as 6400 byte frames as each one fills (`voice::duplex::Framer`, `voice::link`).
- The session starts with a fixed card (`voice::bridge::card`: name Bench, a calm persona that relays answers from an assistant) in `relay` mode, so the model only listens and backchannels until it is given words.
- Reply frames play through a jitter buffer of two frames (`voice::duplex::Jitter`). A frame that is late plays as silence and is dropped when it comes, so the delay never grows; a burst is cut to the buffer.
- A dropped socket reconnects with a backoff of 0.25 s doubling to 8 s, and starts the session again.
- Whisper still reads the same microphone: each utterance ended by the band and hang logic is a supervisor turn.
- Each finished sentence of the supervisor's reply (from `Converse`) is sent as `say`.
- Speaking for 400 ms while said words are still pending sends `interrupt`. The `said` answer is the part you heard, and the next turn starts with `(I cut your spoken reply off; I heard only: "...")` so the supervisor knows where you stopped it.
- The chat header shows `connecting`, `listening`, `hearing you`, `supervisor thinking`, `model speaking` or `reconnecting`. On a phone the long ones read `thinking` and `speaking`, in place of the supervisor chip.

### Tests

`crates/app/tests/duplex.rs` runs the app against a fake duplex server: frames of 6400 bytes about every 133 ms, replies in order, silence for a late frame, a reply as `say` messages, an utterance as a turn (Whisper base on CPU), a barge in as one `interrupt`, the voice clip in `start`, the fallback, and the indicator and settings at 1280 and 390 wide (`AGENTBENCH_SHOTS=dir` keeps the pictures).

`a_real_duplex_server_takes_the_start_frames_say_and_interrupt` (ignored, needs `AGENTBENCH_DUPLEX_URL` and `AGENTBENCH_DUPLEX_MODEL`) runs the same app against a real server. On 26 Nov against inference-bench serving Haru-4B on GPU1: ready, 14 frames sent for 2 s of room, all 30 frames pulled after that carried sound, and the three `say` messages of a reply were taken with no error.

Not covered here: a spoken conversation with the real model, judged by ear. That is the owner's check.
