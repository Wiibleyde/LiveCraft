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
| `sources.rs`, `playback.rs`, `browser.rs` | Where pictures and sound come from |
| `audio.rs`, `dsp.rs` | The sound loop, buses and per-input processing |
| `output.rs`, `outputs.rs` | One encoder, and starting and stopping them |
| `replay.rs`, `extras.rs` | Instant replay, scripts, data sources, PTZ |
| `functions.rs`, `api.rs` | Named functions, and HTTP and TCP remote control |
| `vmix_import.rs` | Reading `.vmix` projects |
| `secret.rs` | Sealing secrets in the session file |
| `ui.rs`, `ui/`, `widgets.rs` | The window |
| `selftest.rs`, `playtest.rs` | `--selftest`: real end-to-end checks |

## Doing less

- A picture is only redone when something in it changed, tracked by a fingerprint per input.
- Cards are composed at card size and never touch full-size compositing.
- Sources know whether anyone is watching. A web page off air sends about two pictures a second.
- Big buffers are reused, because fresh memory costs more than the work done on it.
- A LUT is reduced to fit the processor's fast memory and applied without branches.

## Rules worth keeping

- Pixels are RGBA with premultiplied alpha.
- Sizes in saved data are fractions of the frame, so projects survive a change of format.
- The desk can be tested without a window, and the window with real clicks in `ui/tests.rs`.
