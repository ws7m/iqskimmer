# IQ Skimmer

Multi-channel CW skimmer for FlexRadio DAX IQ streams. Written in Rust with an egui GUI. It runs natively on macOS, Windows and Linux.

- Vertical waterfall (frequency up, time scrolling right to left, newest at the right edge)
- Detects every CW signal in the I/Q span and runs a decoder on each one, in parallel (rayon)
- Decoder text is displayed to the right of each signal as decoded.  In parenthesis is estimated CW WPM.
- Spot labels (`CQ EA3AKY ▶`) sit next to each signal's frequency. Click one to follow its text.

## GUI Controls

- Top: Select I/Q source, Radio IP, Discover, DAX IQ Channel, rate, Start/stop
- Next line: slider detect dB, slider for floor, slider for range, swap I/Q, Full span, Save text, Open folder, Alerts, Slice displays
  - Detect dB is the detect threshold
  - Floor sets the background color and signal floor of the waterfall
  - Range sets the signal range on the waterfall
  - Save text - Toggles on saving decoded text to small text files
  - Alerts - Provides a window to enter comma separated alerts.  Example:  W1AW, QST, CQ, WS7M
- Waterfall:
  - Left click on a signal in the waterfall highlights that signal.  Decoded text is shown below in decoder line.
  - Above decoded text in decoder line is frequency highlighted, estimated CW WPM and signal dB
- Center area between waterfall and decoded text:
  - Click and drag up/down to slide radio pan-adapter
  - Mouse wheel expand/compress IQSkimmer view
- Decoder dots in center area:
  - Right click and select slice to move to this frequency
- Alerts
  - If alert text was entered, discovered text will be listed in the alerts area along with date/time
  - Double click on an alert to move Slice A to the frequency.
    
## Build

```bash
cargo build --release
```

### macOS app

```bash
./scripts/macos-app.sh                      # builds target/release/IQ Skimmer.app
open "target/release/IQ Skimmer.app"
cp -R "target/release/IQ Skimmer.app" /Applications/
```

Use the app on macOS. On first launch, macOS asks to let IQ Skimmer find devices on your local network: click **Allow**.
Without that permission, connecting to the radio fails with "No route to host". A bare binary run from a terminal can be blocked silently.
The app remembers the source settings. If it was connected to the radio when you quit, it reconnects on the next launch.
The app is ad-hoc signed, so macOS may ask again after a rebuild.

### Windows

Install [Rust](https://rustup.rs) and the Visual Studio Build Tools (the "Desktop development with C++" workload), then run `cargo build --release`.
That produces `target\release\iqskimmer.exe`, which runs on its own.
On first connection, Windows Firewall asks whether to allow incoming network traffic. The I/Q stream arrives as UDP, so allow it on private networks.

### Linux

Install Rust and the GUI development packages, then run `cargo build --release`. On Debian/Ubuntu:

```bash
sudo apt-get install libxkbcommon-dev libgl1-mesa-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev libwayland-dev
```

### Automated builds (GitHub Actions)

`.github/workflows/build.yml` tests and builds all three platforms on every push to `main` and on every pull request.
- **Artifacts:** each run uploads three downloads: the macOS app (Apple Silicon), the Windows `.exe` and the Linux binary. They are on the run's page under **Actions**.
- **Releases:** push a version tag to create a GitHub release with all three attached:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

The downloads aren't signed with paid certificates, so each OS warns the first time:
- **macOS:** right-click the app and choose **Open**, or run `xattr -dr com.apple.quarantine "IQ Skimmer.app"`.
- **Windows:** SmartScreen shows "Windows protected your PC"; click **More info**, then **Run anyway**.

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

## Saving decoded text

Click **Save text** in the toolbar, or start with `--save-text`. Each CW signal shown gets its own file in `Documents/IQSkimmer`, named `date_time_frequency.txt` (for example `20261028_193512_7024.77.txt`).
A file starts with everything decoded so far. After a pause of 10 s or more, the next text starts a new line stamped with the time.
**Open folder** shows the folder. The setting is remembered between launches.

## Callsign alerts

Click **Alerts…** in the toolbar and enter the callsigns to watch for, separated by commas. The list is remembered between launches.
When one is decoded, it is listed in the **Alerts** panel below the decoded text with date, time, callsign, frequency and mode.
`W1AW` also matches `W1AW/P`. The same call on the same frequency (within 200 Hz) is listed only once. Alerts are removed after 5 minutes.
Double-click an alert to move slice A there in CW.

## How it works

| Stage | Where |
|---|---|
| FlexRadio TCP control (4992) + VITA-49 UDP DAX IQ | `src/source/flex.rs`, `src/source/vita.rs` |
| Waterfall STFT (~12 Hz bins, 64 rows/s), noise floor, peak detection, signal tracking | `src/engine.rs` |
| Channelizer STFT: one FFT gives every decoder an ~80 Hz-wide envelope at 250 Hz | `src/engine.rs`, `src/dsp/stft.rs` |
| Per-signal CW decoder: two-level tracker, hysteresis, adaptive WPM | `src/dsp/cw.rs` |
| Callsign / CQ extraction for labels | `src/dsp/callsign.rs` |
| GUI | `src/gui.rs` |
