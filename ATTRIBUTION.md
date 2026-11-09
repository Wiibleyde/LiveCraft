# Attribution

LiveCraft stands on other people's work.

## Bundled in the program

| Component | Used for | License |
| --- | --- | --- |
| Ubuntu Light font, through `epaint_default_fonts` | Text in titles and the clock when no installed font is chosen | Ubuntu Font Licence 1.0 |
| Other egui default fonts, through `eframe` | The interface | OFL 1.1 and Ubuntu Font Licence 1.0 |

## In the Windows installer

| Component | Used for | License |
| --- | --- | --- |
| [Inno Setup](https://jrsoftware.org/isinfo.php) setup and uninstaller programs, by Jordan Russell and Martijn Laan | Installing and removing LiveCraft | Inno Setup License (permissive) |

## Rust crates

The direct dependencies are listed in `Cargo.toml`. All crates in `Cargo.lock`
are available under permissive terms (MIT, Apache-2.0, BSD, ISC, Zlib, BSL-1.0,
Unicode-3.0 or Unlicense). Where a crate offers a choice that includes a copyleft
license (`r-efi`, `self_cell`), LiveCraft uses it under its MIT or Apache-2.0 option.

To list them with their licenses:

```sh
cargo metadata --format-version 1
```

## Run alongside, not bundled

| Program | Used for | Note |
| --- | --- | --- |
| [FFmpeg](https://ffmpeg.org) | Decoding, capture, encoding, recording, streaming | Started as a separate process. Installed by the user, under its own license. |
| Microsoft Edge or Google Chrome | Web page inputs | Started as a separate process with no window. |

## Design references

- The public vMix documentation, for what the features do and for the names of
  the shortcut functions and the remote control API.
- [OBS Studio](https://github.com/obsproject/obs-studio), for ideas on doing less
  for what nobody is watching. OBS is GPL-2.0: ideas only, no code came from it,
  and none may. See [CONTRIBUTING.md](CONTRIBUTING.md).
