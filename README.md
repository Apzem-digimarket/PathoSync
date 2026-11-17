# PathoSync

A Windows desktop tool that records **up to 4 microphones at the same time** with **push-to-talk**
for each one, and saves every recording as **one mixed file plus one multitrack file** (one channel
per microphone). It was built for histopathology work (grossing dictation, multi-head microscope
discussions), as an add-on: no login and no installation. Double-click `PathoSync.exe` and use it.

Staff instructions: [docs/QUICK-GUIDE.txt](docs/QUICK-GUIDE.txt) (it is also shipped next to the `.exe`).

## Features (Phase 1)

| Area | What it does |
|---|---|
| Microphones | Up to 4 at once: USB, wireless receivers, or several inputs of one audio interface. Wired and wireless can be mixed. |
| Sync ("virtual aggregate device") | Every mic is resampled onto one 48 kHz master clock, and each mic's clock drift is measured and corrected continuously. The **clap test** measures each mic's fixed delay so wireless and USB mics line up. |
| Push-to-talk | Per mic: a keyboard key (works even when PathoSync is in the background), the mic's own button if it sends a key, or the on-screen *Hold to talk* pad. **0.5 s pre-roll** (the first word is never cut) and a **0.3 s release tail**. Both are adjustable. |
| Files | `<case>_<date>_<time>/` containing `_mix.wav` (mono mix of all mics), `_multitrack.wav` (one channel per mic) and `_info.json` (mics, devices, and who spoke when). WAV files are 24-bit, 48 kHz. |
| Safety | Files are written on a separate thread and their headers are refreshed every second, so a crash or power cut keeps everything up to the last second. Closing the window during a recording saves it. Recordings longer than 1 hour continue in `_part2` files (WAV size limit). |
| Mic check | Live background-noise level, clock drift, dropouts and sync delay per mic, to compare microphones before buying. |
| Add-on use | Other software can open it with a case number: `PathoSync.exe --case S26-1234 --folder "D:\Recordings" --exit-after-save` |
| Portable | Settings are kept in `PathoSync.settings.json` next to the exe (or in `%APPDATA%\PathoSync` if that folder is read-only). |

**Phase 2 (planned):** noise reduction for the grossing hood fan, loudness levelling, a review/trim screen, and backup to a network folder.

## How it works

```
 Mic 1 ─┐  sound-card callback → lock-free queue ─┐
 Mic 2 ─┤                                          ├─► master clock (5 ms blocks):
 Mic 3 ─┤                                          │     resample each mic onto 48 kHz with drift correction
 Mic 4 ─┘                                          │     → clap-test delay → 0.5 s pre-roll delay
                                                   │     → push-to-talk gate (with 5 ms fades)
                                                   └─► writer thread → _mix.wav + _multitrack.wav
```

* **Drift correction** (`core/src/dsp.rs`, `core/src/pipeline.rs`): each mic's buffer is held at
  60 ms. Audio the device has captured but not yet delivered is counted too, using the callback
  time. A PI controller steers a cubic resampler. The simulated test runs three mics at −150 ppm,
  +220 ppm and 44.1 kHz/+80 ppm for 20 minutes. They line up within 0.2 ms after 30 s and within
  0.02 ms after that. Without correction they would be 0.44 s apart.
* **Push-to-talk** (`core/src/gate.rs`): presses are stamped on the master clock. The recorder runs
  0.5 s behind real time, so the pre-roll is real audio from before the press.
* **Clap test** (`core/src/align.rs`): cross-correlates the clap's envelope in every track.

## Project layout

```
core/      audio engine (Rust): devices, sync, push-to-talk, WAV writer, tests
app/       desktop shell (Tauri 2): settings, keys, commands, window
ui/        the screen (plain HTML/CSS/JS, no build step)
docs/      staff quick guide
.github/   Windows build (GitHub Actions)
```

## Building

**Windows .exe:** every push to `main` builds `PathoSync.exe` with GitHub Actions. Open
*Actions → Build Windows app → latest run → Artifacts → PathoSync-windows*. The build runs the
engine tests first.

**Locally** (Rust stable + Node 20):

```bash
npm ci
cargo test --release -p pathosync-core        # engine tests (~15 s)
npx tauri dev                                 # run with live reload
npx tauri build --no-bundle                   # → target/release/pathosync(.exe)
cargo run -p pathosync-core --example list_mics   # list the microphones the engine can see
```

On macOS 26 with Command Line Tools whose newest SDK is too new for the linker, set
`SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk`.

Windows needs the Microsoft Edge WebView2 runtime. It is built into Windows 11 and current Windows 10.
