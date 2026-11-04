# Roadmap

Nothing here has a date. Open an issue before starting something large.

## Most wanted

- [ ] **Real-world testing.** Cameras, capture cards, PTZ heads and live streaming platforms have not
      been tried yet. Reports are worth more than features at this stage.
- [ ] **Compositing on the GPU.** The picture is composed on the CPU today. Moving it to the graphics
      card is the path to comfortable 4K, and would remove most of the remaining load.
- [ ] **Interface translation.** Text is in French inside the code; it needs to move out first.
- [ ] **Shared clock for sound and picture** over very long recordings.

## Inputs and outputs

- [ ] NDI and OMT, in and out
- [ ] Animated title templates from `.vmix` projects
- [ ] Video call inputs
- [ ] PowerPoint and DVD inputs
- [ ] Virtual sets
- [ ] Hardware decoding where it pays off
- [ ] Audio plug-ins

## Platforms

- [ ] Linux
- [ ] macOS
- [ ] Installers and signed releases

## Quality

- [ ] Replay marks that stay exact when frames are dropped
- [ ] Long-run tests: a whole show, with memory and timing watched throughout
- [ ] Automated checks with a virtual camera and a local streaming server
