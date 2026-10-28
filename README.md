# IQ Skimmer

Multi-channel CW skimmer for FlexRadio DAX IQ streams. Written in Rust with an egui GUI. It runs natively on macOS, Windows and Linux.

- Vertical waterfall (frequency up, time scrolling right to left, newest at the right edge)
- Detects every CW signal in the I/Q span and runs a decoder on each one, in parallel (rayon)
- Spot labels (`CQ EA3AKY ▶`) sit next to each signal's frequency. Click one to follow its text.

## Build

```bash
cargo build --release
```

## Run

```bash
# FlexRadio: first assign DAX IQ channel 1 to a panadapter in SmartSDR
./target/release/iqskimmer --flex 192.168.1.50 --daxiq 1 --rate 96000
./target/release/iqskimmer --flex auto          # use first radio discovered on the LAN
./target/release/iqskimmer --discover           # list radios

# Other sources
./target/release/iqskimmer --wav band.wav --center 7025000   # stereo WAV, L=I R=Q
./target/release/iqskimmer --synth --stations 40 --rate 192000

# No GUI: print spots for 30 s
./target/release/iqskimmer --synth --headless 30
```

You can also choose the source and start or stop it from the toolbar. To load a WAV file, drag it onto the window.

GUI controls: scroll to zoom, drag to pan, and double-click for the full span. Click a signal or a label to select it.
If the spectrum looks mirrored, tick **Swap I/Q**.

## How it works

| Stage | Where |
|---|---|
| FlexRadio TCP control (4992) + VITA-49 UDP DAX IQ | `src/source/flex.rs`, `src/source/vita.rs` |
| Waterfall STFT (~12 Hz bins, 64 rows/s), noise floor, peak detection, signal tracking | `src/engine.rs` |
| Channelizer STFT: one FFT gives every decoder an ~80 Hz-wide envelope at 250 Hz | `src/engine.rs`, `src/dsp/stft.rs` |
| Per-signal CW decoder: two-level tracker, hysteresis, adaptive WPM | `src/dsp/cw.rs` |
| Callsign / CQ extraction for labels | `src/dsp/callsign.rs` |
| GUI | `src/gui.rs` |
