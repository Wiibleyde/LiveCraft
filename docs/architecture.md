# Architecture

```text
 sources ──► frames ──► mixer ──► program ──► window
 (ffmpeg,               (layers,       └─────► encoders (ffmpeg): files, streams
  browser,               effects,
  generators)            transitions)
                          ▲
 buttons, shortcuts, ─► functions ─► desk state
 remote control, scripts
```

Every source runs on its own thread and publishes its latest frame. The mixer thread reads those frames
at the output rate and composes what is needed. The window only reads shared state and sends commands,
so a slow window cannot hold up the show.

## Files

| File | Holds |
| --- | --- |
| `model.rs` | Everything that is saved: inputs, settings, the project |
| `desk.rs` | What the buttons mean: preview, program, transitions, overlays. No threads |
| `engine.rs` | Starts the threads, owns sources and outputs |
| `mixer.rs` | The video loop: what to compose, and reusing what has not changed |
| `frame.rs`, `fx.rs`, `transitions.rs` | Pixels: blending, scaling, colour, keys, effects |
| `sets.rs` | Virtual sets: built-in decors and camera moves; the mixer composes them |
| `sources.rs`, `playback.rs`, `browser.rs`, `dvd.rs` | Where pictures and sound come from |
| `decode.rs` | Which files and streams the graphics card decodes |
| `call.rs` | Video calls: the guest's link and page, and the call received in the windowless browser |
| `slides.rs` | Presentations, turned into one picture per slide by PowerPoint or LibreOffice |
| `audio.rs`, `dsp.rs` | The sound loop, buses and per-input processing |
| `plugins.rs`, `clap.rs` | CLAP plug-ins: finding, running and saving them. Their own thread starts, saves and shows them; the sound thread only runs blocks, without waiting on a lock. Tests build `fixtures/clap-test-gain` |
| `output.rs`, `outputs.rs` | One encoder, and starting and stopping them |
| `netvideo.rs`, `ndi.rs`, `omt.rs` | NDI and OMT in and out; runtimes loaded on first use, never shipped |
| `replay.rs`, `extras.rs` | Instant replay, scripts, data sources, PTZ |
| `functions.rs`, `api.rs` | Named functions, and HTTP and TCP remote control, also https for guests |
| `vmix_import.rs` | Reading `.vmix` projects |
| `secret.rs` | Sealing secrets in the session file |
| `platform.rs`, `platform/` | What differs between Windows, Linux and macOS: capture devices, ending child processes, screens, browser, folders, the key that seals secrets |
| `ui.rs`, `ui/`, `widgets.rs` | The window |
| `i18n.rs`, `locales/` | Interface text: `t!("key")` reads it from one catalog per language |
| `selftest.rs`, `playtest.rs`, `calltest.rs` | `--selftest`: real end-to-end checks |
| `soak.rs` | `--soak`: a whole show left running, with memory, timing and outputs watched |
| `livecheck.rs` | `--livecheck`: a virtual camera and microphone streamed to a local server and read back |

## Doing less

- A picture is only redone when something in it changed, tracked by a fingerprint per input.
- Cards are composed at card size and never touch full-size compositing.
- Sources know whether anyone is watching. A web page off air sends about two pictures a second.
- Big buffers are reused, because fresh memory costs more than the work done on it.
- A LUT is reduced to fit the processor's fast memory and applied without branches.
- Files heavier than 1080p60 and network streams are decoded by the graphics card (D3D11). Smaller
  files stay in software, where they start and seek half a second sooner. A card that fails to open,
  ends a file early or goes quiet on a stream hands over to software, which carries on from the same
  place.

## Rules worth keeping

- Pixels are RGBA with premultiplied alpha.
- Sizes in saved data are fractions of the frame, so projects survive a change of format.
- Replay marks are moments of the show, not positions in a file. Every encoder logs when each picture it
  wrote was taken, so a file that lost pictures still finds the right one.
- The desk can be tested without a window, and the window with real clicks in `ui/tests.rs`.
