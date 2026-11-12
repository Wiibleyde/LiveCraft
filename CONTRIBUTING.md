# Contributing to LiveCraft

Thank you for being here. LiveCraft is young, and what you bring will shape it. English and French are both
fine in issues and pull requests.

## Ways to help

- **Test with real gear.** Cameras, capture cards, PTZ heads, audio interfaces, multiple screens,
  streaming platforms. A report saying "this card works" is a real contribution.
- **Report bugs** with the [bug form](../../issues/new?template=bug_report.yml).
- **Share a difficult project.** If a `.vmix` file opens badly, describe what is missing. Do not post
  files holding stream keys or passphrases.
- **Pick something from the [roadmap](ROADMAP.md).** Open an issue first for anything large, so that
  two people do not build the same thing.

## The one rule you cannot break: clean room

LiveCraft exists because it was written without looking inside anyone else's software. Please keep it so.

- **Never** decompile, disassemble, trace or copy from vMix or any other closed program, and never
  paste its code, assets, logos, icons or title templates here.
- Public documentation, your own project files and behaviour you can see from the outside are fine.
- **No GPL code.** LiveCraft is MIT or Apache-2.0. Reading OBS Studio for ideas is welcome; copying or
  closely translating its code is not, in either direction.
- Say so in your pull request if you used an AI tool. You remain responsible for what you submit.

## Set up

```sh
git clone <this repository>
cd livecraft
cargo run
```

You need Rust and FFmpeg on your `PATH`. Web inputs also need Edge or Chrome.

## Before you open a pull request

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -- --selftest target/selftest
```

`--selftest` needs FFmpeg. It generates its own media, then really plays, records, streams and
replays, so run it for anything touching sources, outputs or timing.

`--soak <folder> <length>` (for example `45m`) runs a whole show for that long: looping videos, a list,
music, a web page, titles, transitions, recording, a local stream, replays and remote control calls.
Every five seconds it writes memory, handles, child processes, frame timing and what each encoder was
fed to `soak.csv`, and it fails with a report in `soak.json` on a memory or handle climb, sustained late
frames, dropped pictures or sound, or files that do not last as long as the show. It uses the same
fixed ports as `--selftest`. The [Long run](.github/workflows/soak.yml) workflow runs it weekly on
Windows and can be started by hand; the report is kept as an artifact.

If you change the interface, add a test in `src/ui/tests.rs`: they drive the real window and engine
with real clicks. If you fix a bug, add the test that would have caught it.

## How we write code

- **Measure before optimising.** A change for speed says what it gained, and on which project.
- **Small, single-purpose commits**, with a short subject in the imperative. Prefix with the area when
  it helps, for example `fix(mixer): keep the last frame on a failed decode` or `audio: reopen a lost
  device`.
- **Comments explain why**, not what. Code names say what things are.
- **A file saved by an older version must keep opening.** New fields get a default.
- **Nothing secret reaches a project file**, the screen or a log.
- **Interface text lives in [`locales/`](locales), never in code.** Write `t!("area.key")`, or
  `t!("area.key", name = value)` for a text holding `{name}`, and add the key to every
  `locales/<language>.txt` (`key = text`, one per line, `\n` for a line break). Keys are always
  literals: `cargo test` checks that each one used exists in every language with the same `{names}`.
  Give a window, combo box or collapsing section a fixed id, not one taken from its text, so it keeps
  its place when the language changes.

## Translating

Copy `locales/en.txt` to `locales/<code>.txt` (`de`, `es`…), translate the text after each ` = `
and leave the keys and `{names}` alone. Then add the language to `Lang` in `src/i18n.rs`. Keep
button labels about as short as the English ones: the cards are narrow. Run
`cargo run -- --screenshot target/shot.png --lang <code>` to see what overflows.

## Pull requests

Keep them focused, describe what you tested and on what hardware, and include a screenshot for visual
changes. By contributing, you agree that your work is licensed under MIT or Apache-2.0, as the project is.

## Be kind

Be patient and considerate. Harassment and personal attacks have no place here, and maintainers may
remove contributions or people who bring them.
