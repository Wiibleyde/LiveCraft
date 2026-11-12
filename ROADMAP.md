# Roadmap

Nothing here has a date. Open an issue before starting something large; the
[open issues](https://github.com/Wiibleyde/LiveCraft/issues) hold the details.

## Most wanted

- [ ] **Real-world testing.** Cameras, capture cards, PTZ heads and live streaming platforms have not
      been tried yet. Reports are worth more than features at this stage: try the
      [alpha](https://github.com/Wiibleyde/LiveCraft/releases) and say what broke.
- [x] **Compositing on the GPU**, with the CPU as fallback (#7)
- [x] **Interface translation**: French and English catalogs (#4)
- [x] **Shared clock for sound and picture** over very long recordings (#6)

## Inputs and outputs

- [x] NDI and OMT, in and out (#3)
- [ ] Animated title templates from `.vmix` projects
- [x] Video call inputs (#5)
- [x] PowerPoint and DVD inputs (#9, #15)
- [x] Virtual sets (#14)
- [x] Hardware decoding where it pays off (#8)
- [x] Audio plug-ins (CLAP, #11)
- [ ] Keep NDI/OMT inputs and video-call guests on the shared clock (#26)
- [ ] Video Delay input (#29)
- [ ] One encoder shared by stream destinations with identical settings (#28)
- [ ] Recording profiles for editing: ProRes, DNxHR, MagicYUV (#31)
- [ ] Encode straight from the graphics card, without a trip through memory

## Desk

- [ ] Video scopes: waveform, RGB parade, vectorscope and safe areas (#27)
- [ ] Production clock with pause, resume and reset (#30)
- [ ] Open a second project in the running instance (#25)
- [ ] Find FFmpeg more robustly and say what it can do (#32)

## Platforms

- [x] Linux and macOS from source, experimental (#1)
- [ ] Packaged builds for Linux and macOS
- [x] Windows installer and release workflow (#10)
- [ ] Signed Windows releases (the workflow signs once a certificate is set up)

## Quality

- [x] Replay marks that stay exact when frames are dropped (#13)
- [x] Long-run tests: a whole show, with memory and timing watched throughout (#12)
- [x] Automated checks with a virtual camera and a local streaming server (#2)
- [ ] Intermittent test failures (#22, #23, #24)
