# Notes for AI coding agents

Read [CONTRIBUTING.md](CONTRIBUTING.md) first. Its clean room rule applies to you without exception:
never open, run, decompile or query vMix or its files, and never copy GPL code.

## Commands

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                                 # includes interface tests with real clicks
cargo run -- --selftest target/selftest    # real media end to end, needs FFmpeg
cargo run -- --livecheck target/livecheck  # virtual camera and mic streamed and read back, see README
cargo run -- --screenshot target/shot.png  # look at what you changed
```

## Where things live

See [docs/architecture.md](docs/architecture.md). In short: desk logic in `src/desk.rs` holds no threads
and is easy to test, `src/mixer.rs` makes pictures, `src/sources.rs` and `src/playback.rs` feed them,
`src/outputs.rs` starts and stops outputs, and `src/ui/` only reads state and sends commands.

## Traps already fallen into

- ffmpeg opens its inputs in order and reads each before the next. Sound goes first in `output.rs`,
  or pictures and sound wait for each other forever.
- A page in a background tab draws nothing: each web input gets its own browser window.
- Saved data must keep loading: every saved struct has `#[serde(default)]`.
- An encoder's file starts when the encoder is ready, not when the button was pressed.
- Stopping a child process must end its whole tree, as ffmpeg may be started through a launcher.
- Do not measure speed by feel. Find what costs time on a real project before changing anything.
