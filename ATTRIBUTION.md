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
| Microsoft Edge or Google Chrome | Web page and video call inputs | Started as a separate process with no window. |
| NDI runtime (Vizrt NDI AB) | NDI inputs and output | Proprietary. Loaded at run time from the user's NDI Tools installation, never shipped. Bindings written from the public SDK documentation at docs.ndi.video. NDI® is a registered trademark of Vizrt NDI AB. |
| [libomt and libvmx](https://github.com/openmediatransport) | OMT inputs and output | MIT. Loaded at run time from beside the program or the `PATH`, not shipped. Bindings written from the public `libomt.h`. |
| [Softcam](https://github.com/tshino/softcam) | The virtual camera of `--livecheck` | MIT. Built from its sources by `scripts/livecheck-tools.ps1` and loaded at run time; not shipped. |
| [MediaMTX](https://github.com/bluenviron/mediamtx) | The local streaming server of `--livecheck` | MIT. Downloaded by `scripts/livecheck-tools.ps1`; not shipped. |
| [VB-CABLE](https://vb-audio.com/Cable/) | The virtual microphone of `--livecheck` | Donationware by VB-Audio, installed separately; not shipped. |

## Design references

- The public vMix documentation, for what the features do and for the names of
  the shortcut functions and the remote control API.
- [OBS Studio](https://github.com/obsproject/obs-studio), for ideas on doing less
  for what nobody is watching. OBS is GPL-2.0: ideas only, no code came from it,
  and none may. See [CONTRIBUTING.md](CONTRIBUTING.md).
