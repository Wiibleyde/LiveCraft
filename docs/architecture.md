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
| `canvas.rs` | The steps of composing, which the processor and the graphics card both follow |
| `frame.rs`, `fx.rs`, `transitions.rs` | Pixels on the processor: blending, scaling, colour, keys, effects |
| `gpu.rs`, `gpu.wgsl` | Pixels on the graphics card, with the same arithmetic |
| `sources.rs`, `playback.rs`, `browser.rs` | Where pictures and sound come from |
| `audio.rs`, `dsp.rs` | The sound loop, buses and per-input processing |
| `output.rs`, `outputs.rs` | One encoder, and starting and stopping them |
| `replay.rs`, `extras.rs` | Instant replay, scripts, data sources, PTZ |
| `functions.rs`, `api.rs` | Named functions, and HTTP and TCP remote control |
| `vmix_import.rs` | Reading `.vmix` projects |
| `secret.rs` | Sealing secrets in the session file |
| `ui.rs`, `ui/`, `widgets.rs` | The window |
| `selftest.rs`, `playtest.rs` | `--selftest`: real end-to-end checks |

## Processor or graphics card

The mixer composes through `canvas.rs`, written once for both. With a suitable graphics card (DirectX 12,
Vulkan or Metal, not a software emulation) the card does the work; otherwise, or if the card reports an
error, the processor does, and Settings, Format says which. Each new source frame is uploaded once.
Only pictures needed in memory come back: the output, for the encoders and the window, is sent back
while the preview is composed, and is collected before the outputs need it. Blurs and sharpening stay
on the processor. Both sides do the same arithmetic, and `mixer_gpu_tests.rs` checks that they agree.

## Doing less

- A picture is only redone when something in it changed, tracked by a fingerprint per input.
- Cards are composed at card size and never touch full-size compositing.
- Sources know whether anyone is watching. A web page off air sends about two pictures a second.
- Big buffers are reused, because fresh memory costs more than the work done on it.
- A LUT is reduced to fit the processor's fast memory and applied without branches.
- On the graphics card, pictures and the buffers bringing them back are reused the same way.

## Rules worth keeping

- Pixels are RGBA with premultiplied alpha.
- Sizes in saved data are fractions of the frame, so projects survive a change of format.
- The desk can be tested without a window, and the window with real clicks in `ui/tests.rs`.
