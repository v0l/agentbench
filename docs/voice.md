# Voice

The supervisor chat can be spoken to. Voice mode (settings, conversation) reads each reply aloud a sentence at a time. Pressing the talk key while it reads cuts the reply off.

## Hands free

With hands free on, the microphone stays open while the supervisor chat is the open chat. You do not press anything:

- Speech is found with the same band and hang logic as a normal take (`Endpoint` in `crates/app/src/voice/mod.rs`). Room noise before speech is dropped, keeping one second of lead in.
- When speech starts, the reader stops and the queued sentences are dropped at once. That is barge in, the same as `App::barge_in`.
- When you go quiet for the hang time, the take is read and sent as a turn, and the microphone stays open for the next one.
- Leaving the chat closes the microphone. The talk key, or the `stop listening` key in the composer, pauses it until you press the talk key again or come back to the chat.
- The chat header shows `listening`, or `hearing you` while you speak.

### Noise is not sent

Whisper writes words for sounds that are not speech: a clap reads as "you", a breath or a cough as "Thank you.", and large v3 writes `*Clap*`. A hands free take is dropped, never sent, when any of these hold (`voice::sounds_like_noise`, `agent_voice::voice::noise`):

- It has less voice than `shortest speech` (`voice_min_speech_secs`, 0.25 s by default, the shortest speech Silero VAD counts). Voice is the span from the first to the last 20 ms block the endpoint called speech. Such a take is not read at all.
- Whisper's own scores say it is not speech: no speech probability over 0.6 and average log probability under -1.0, the defaults OpenAI's `transcribe` uses to skip a window. The local reader computes both; a speech server that answers `no_speech_prob` and `avg_logprob` beside `text` (agentbench's own does) is judged the same way.
- The text is only punctuation (`-`, `...`) or a sound tag in `*`, `[]`, `()` or `<>` (`*clap*`, `[music]`, `(applause)`).
- The take has less voice than `short take` (`voice_short_take_secs`, 2 s by default) and reads only as a phrase Whisper writes for silence: "you", "thank you", "thanks for watching", "bye", "um", a `www.` address and the like.

A dropped take shows `ignored: sounded like noise` (`ignored noise` on a phone) in the chat header for 3 s, and the microphone stays open. Large v3 turbo scores a clap as confident speech (no speech probability near 0), so for it the voice length and the phrase list do the work. The test fixtures `silence`, `clap`, `breath`, `cough`, `yes` and `stop` in `crates/app/tests/fixtures/voice` are made by `make/noise.py` (the two words from the agentbench text to speech server), and `hands_free_ignores_noise_and_still_sends_a_short_word` runs them through base.en and, when it is installed, large v3 turbo. `cargo run --release -p agent-voice --features speech-cuda --example hear -- take.wav ~/.local/share/agentbench/models whisper-large-v3-turbo` prints both scores for a recording.

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

- `voice model`: `Haru-4B (local)` loads the model on this machine's GPU while hands free is on; `none, read aloud` keeps today's path: speech to text, the supervisor, read aloud.
- `model folder`: where Haru-4B was downloaded. Empty uses the newest snapshot in the Hugging Face cache (`$HF_HOME/hub` or `~/.cache/huggingface/hub`), then `~/models/Haru-4B`. The line under it says which of `config.json` and `model.safetensors` is missing.
- `GPU`: auto, or a card from `nvidia-smi`. Auto picks the card with the most free memory; the line under it says which card and why, or why none can take the model (about 16 GB free is needed).
- `unload after`: minutes the model stays loaded while hands free is set up but not listening (another chat open, or paused), 10 by default. 0 keeps it loaded.
- `replies in its voice`: off by default. On, the supervisor's replies are spoken by the model; off, they are read aloud by text to speech and the model only listens and backchannels. It is off because Haru speaking queued text measured a word error of 3.07 (inference-bench `docs/specrt-haru.md`, section 13). It is kept under `voice_duplex_says`, so an `on` saved by an older build under `voice_duplex_replies` reads as off.
- `model speaks first`: off by default. Off, the session starts without `speak_first` and nothing the model sends is played until you speak or a reply is sent to it; on, it may greet you first.
- `voice clip`: press record, speak, press stop. Up to 30 s is kept as `duplex-voice.wav` in the agentbench home and sent as the voice reference at the start of each session. It is needed: with none the model speaks in an unconditioned voice that sounds like noise, so no session is opened (the local engine is not started either), hands free uses speech to text and read aloud, and the chat chip reads `record your voice clip first` (`record voice clip` on a phone).
- Advanced, `duplex server`: an inference-bench server that already runs a duplex model, here or elsewhere, such as `http://gpu-box:8080`. When set it wins and nothing is loaded here; `duplex model name` and `duplex key` are then sent as `model` and as a bearer token.
- Advanced, `engine program`: the `inference-bench-server` to run. Empty looks on PATH, then in `~/.cargo/bin`.

### The local model

Agentbench runs the model in a child process it owns, `inference-bench-server` from inference-bench, not inside the app: the engine is large and CUDA specific, and keeping it out of the app keeps it out of the phone and browser builds and keeps a CUDA context out of the window. You never see a server, an address or a key. Agentbench does not download or build it; when it is missing, too old to answer `--version`, or does not list `/v1/duplex` in `/v1/capabilities`, the notice says to run:

```
cargo install --locked --git https://github.com/v0l/inference-bench inference-bench-server
```

When hands free opens the microphone with the local model chosen (`voice::engine::Engine`):

- The model folder is checked, then the card: one with less free memory than needed is refused with its numbers, and nothing else is asked of a card another process holds.
- The child is started bound to `127.0.0.1` on a free port, with a random key passed in its environment (`INFERENCE_BENCH_API_KEYS`, never written to disk), `CUDA_VISIBLE_DEVICES` and `CUDA_DEVICE_ORDER=PCI_BUS_ID` set for the child only, and a generated config, `voice-engine/engine.toml` in the agentbench home: the list of models to load (one duplex model today), `sessions_per_gpu = 1`, `max_ctx = 4096`.
- The chat chip reads `loading the voice model, 4 s` until `/health` answers, `/v1/capabilities` lists `/v1/duplex` and `/v1/models` lists every model loaded with its type. Then the link connects to the child like to any duplex server.
- It stops at once when hands free or the local model is switched off in settings, after the idle time when hands free is set up but not listening (another chat is open, or it is paused with the talk key), and when the app exits. The child is its own process group and, on Linux, gets SIGKILL from the kernel when the app dies (`PR_SET_PDEATHSIG`), so a crash or `kill -9` of the app takes it down too.
- If it dies, the notice says so and it is started once more after 2 s. A second death leaves it off with the last lines of its log in the notice; hands free carries on with speech to text and read aloud. Turning hands free off and on again tries afresh.
- Its output goes to `voice-engine/engine.log` in the agentbench home.

The child loads a list of models on one port with one key, so speech to text and text to speech will move onto the same child later (inference-bench issue 1474); today they still run in the agentbench server through candle.

### What happens

- The microphone stays open as in hands free, through the same headphones check and echo canceller. Its samples, after cancelling, are resampled to 24 kHz and sent as 6400 byte frames as each one fills (`voice::duplex::Framer`, `voice::link`).
- The session starts with a fixed card (`voice::bridge::card`: name Bench, a calm persona that relays answers from an assistant) in `relay` mode, so the model only listens and backchannels until it is given words.
- Reply frames play through a jitter buffer of two frames (`voice::duplex::Jitter`). A frame that is late plays as silence and is dropped when it comes, so the delay never grows; a burst is cut to the buffer.
- Until you first speak (or a `say` is sent, or `model speaks first` is on), frames from the model are counted and dropped, not played (`voice::link`, `unmute`). On 26 Nov the first install sent no microphone frames (`frames_in=0` in `engine.log`) while Haru, started with no voice clip, sent 68 frames that the app played as noise.
- The microphone may run at another rate than 16 kHz (48 kHz is common); the framer follows the rate of each block it is handed. Before 26 Nov a microphone not at 16 kHz made a new framer on every frame until the first frame left, which threw away each partial frame, so at 48 kHz no frame ever left: that is the `frames_in=0` of the first install.
- A dropped socket reconnects with a backoff of 0.25 s doubling to 8 s, and starts the session again.
- Whisper still reads the same microphone: each utterance ended by the band and hang logic is a supervisor turn.
- Each finished sentence of the supervisor's reply (from `Converse`) is sent as `say` when `replies in its voice` is on; otherwise it is read aloud.
- Speaking for 400 ms while said words are still pending sends `interrupt`. The `said` answer is the part you heard, and the next turn starts with `(I cut your spoken reply off; I heard only: "...")` so the supervisor knows where you stopped it.
- The chat header shows `connecting`, `listening`, `hearing you`, `supervisor thinking`, `model speaking`, `reconnecting` or `record your voice clip first`. On a phone the long ones read `thinking`, `speaking` and `record voice clip`, in place of the supervisor chip.

### Settings changed on disk

The desktop app reads the voice settings (`voice_*` in `app.ron` in the eframe data folder) again every 2 s and before each save. A setting changed on disk while the app runs is taken, unless the same setting was also changed in the app since the last save, in which case the app's value wins. The settings window, when open, takes the disk change too for settings not touched in it, so saving it does not undo an edit made on disk.

### Tests

`crates/app/tests/duplex.rs` runs the app against a fake duplex server: frames of 6400 bytes about every 133 ms, replies in order, silence for a late frame, nothing played before the user speaks and `speak_first` playing at once, no session without a voice clip and the chip saying so, replies read by text to speech by default and as `say` messages when on, an utterance as a turn (Whisper base on CPU), a barge in as one `interrupt`, the voice clip in `start`, the fallback, a voice setting edited on disk kept through the settings window, and the indicator and settings at 1280 and 390 wide (`AGENTBENCH_SHOTS=dir` keeps the pictures).

`a_real_duplex_server_takes_the_start_frames_say_and_interrupt` (ignored, needs `AGENTBENCH_DUPLEX_URL` and `AGENTBENCH_DUPLEX_MODEL`) runs the same app against a real server. On 26 Nov against inference-bench serving Haru-4B on GPU1: ready, 14 frames sent for 2 s of room, all 30 frames pulled after that carried sound, and the three `say` messages of a reply were taken with no error.

Not covered here: a spoken conversation with the real model, judged by ear. That is the owner's check.

`crates/app/tests/engine.rs` runs the app against a fake child (the test binary itself behind a small script) that serves `/health`, `/v1/capabilities`, `/v1/models` and a duplex socket: start on switch on with the progress line and the card in `CUDA_VISIBLE_DEVICES`, port and key never on screen, stop on switch off, stop when idle and 0 keeping it, a child that dies restarted once then left off with its log tail, no duplex support, a busy card refused with the numbers, a missing engine, a set server address launching nothing, a missing model file falling back to read aloud, the settings at 1280 and 390 wide, and the app killed with SIGKILL in a subprocess taking its child with it.

`the_real_model_loads_answers_with_sound_and_frees_its_gpu` (ignored) runs the same flow on the real engine:

```
~/.agents/bin/gpu-run 1 --min-free 20000 -- env VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
  AGENTBENCH_HOME=/tmp/scratch-home AGENTBENCH_REAL_PLAYED=/tmp/played.wav \
  AGENTBENCH_REAL_ENGINE=<inference-bench-server> AGENTBENCH_REAL_MODEL=~/models/Haru-4B AGENTBENCH_REAL_GPU=1 \
  cargo test -p agentbench --test engine -- --ignored the_real_model
```

It checks what was played against speech (`common::speech_like`: RMS 0.01 to 0.5, zero crossing rate 0.01 to 0.25 per sample, and a 20 ms level that swings by more than 0.3 of its mean, which white noise and a steady tone do not; `the_speech_check_passes_recorded_speech_and_fails_white_noise` holds it to the recorded fixtures). Then it turns `replies in its voice` on, sends a supervisor turn and checks that the 8 s after the `say` messages are silence or speech, never noise. `AGENTBENCH_REAL_PLAYED` keeps both as WAV files.

On 26 Nov, after the noise fix, on GPU1: loaded in 4.4 s, ready 4.7 s after switch on; 149 frames sent, 3 frames dropped before the first speech, the 20 s played RMS 0.052, zero crossings 0.157, swing 2.85, and Whisper read words in it; the said reply RMS 0.065, zero crossings 0.211, swing 2.47, words but not the queued ones (as section 13 measured); 10 MiB on GPU1 after switch off apart from the test's own Whisper context.

On 26 Nov with inference-bench-server 0.2.1 (master `77fff0b6`) and Haru-4B on GPU1: loaded in 4.3 s, session ready 4.5 s after hands free opened, 13.7 GB held on GPU1; 20 s of speech streamed through the app's link sent 149 frames and 150 of the 151 frames pulled back had sound; after switch off GPU1 was back to 5 MiB and no `inference-bench-server` process was left. The app killed with SIGKILL during a real load (10.5 GB on GPU1 by then) took the child with it within 3 s and freed the card.
