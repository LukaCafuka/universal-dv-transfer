# Universal DV Transfer

A Rust desktop application and CLI for transferring DV, DVCAM, and HDV tape recordings over FireWire. DV25 uses indexed AVI; HDV1 720p and HDV2 1080 use original MPEG-2 transport streams (`.m2t`). DVCPRO and DVrescue-style analysis remain outside this implementation.

## Build and install

Clone the repository from GitHub or download and extract its source ZIP. Open a
terminal in the project folder containing `Cargo.toml` and `Cargo.lock`.

- Install a stable Rust toolchain with Cargo. The project requires Rust 1.88 or
  newer. [Rust toolchain installer](https://rustup.rs/)
- Install the platform build tools listed below.
- Put FFmpeg on `PATH` for live preview, AVI input, and Type 2 AVI output. Check
  that `ffmpeg -version` works in a new terminal. Raw DV input and Type 1 AVI
  capture can run without FFmpeg when preview is disabled.
  [FFmpeg downloads](https://ffmpeg.org/download.html)

### Windows (x64)

Install Visual Studio Build Tools with **Desktop development with C++**,
including the MSVC x64 tools and a Windows SDK. If Cargo cannot find the
compiler or linker, open **Developer PowerShell for Visual Studio**.
[Microsoft build tools instructions](https://learn.microsoft.com/en-us/cpp/overview/acquire-msvc)

Build the release executable and launch it from the checkout.

```powershell
rustup target add x86_64-pc-windows-msvc
cargo build --locked --release --target x86_64-pc-windows-msvc
.\target\x86_64-pc-windows-msvc\release\universal-dv-transfer.exe gui
```

To install the executable for your user account, run this from the checkout.

```powershell
cargo install --locked --path . --target x86_64-pc-windows-msvc
```

For tape transfers, connect the FireWire deck in playback/VTR mode. Windows
uses MSDV for DV and MSTape for HDV. Close other programs using the deck before
opening it here.

### Linux (Debian or Ubuntu)

Install the compiler, FireWire development libraries, desktop libraries, and
FFmpeg.

```sh
sudo apt update
sudo apt install build-essential pkg-config libraw1394-dev libiec61883-dev \
  libavc1394-dev ffmpeg libx11-dev libxi-dev libxkbcommon-dev \
  libwayland-dev libgl1-mesa-dev
cargo build --locked --release
./target/release/universal-dv-transfer gui
```

For tape transfers, your account needs read and write access to the appropriate
`/dev/fw*` device. Use your distribution's udev or logind device permissions so
the GUI can run under your regular account.

### macOS

Install the Xcode Command Line Tools with `xcode-select --install` if you do
not already have a working Xcode toolchain. Install FFmpeg with
`brew install ffmpeg` if you use Homebrew.

For tape transfers, use a Mac with working FireWire drivers and build against
an SDK that includes the FireWire headers, such as the macOS 15 SDK in
Xcode 16. The commands below select that Xcode for the current shell. Adjust
the path to match your installation. An SDK without those headers builds the
file workflows only. [Xcode SDK and system requirements](https://developer.apple.com/xcode/system-requirements)

```sh
# For FireWire builds, select an Xcode with the macOS 15 SDK.
export DEVELOPER_DIR=/Applications/Xcode_16.app/Contents/Developer
export SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"
xcrun --sdk macosx --show-sdk-version
```

Build and launch from the checkout. For file workflows only, the active
toolchain is sufficient and you can skip the SDK selection above.

```sh
cargo build --locked --release
./target/release/universal-dv-transfer gui
```

### Install and launch from your terminal

On Linux and macOS, install from the checkout with `cargo install --locked --path .`.
For a macOS FireWire build, use the same SDK environment as above. On Windows,
use the install command in the Windows section.

Cargo installs to your Cargo bin directory. The default is `~/.cargo/bin` on
Linux and macOS and `%USERPROFILE%\.cargo\bin` on Windows. With that directory on
`PATH`, use `universal-dv-transfer --help` to verify the installation and
`universal-dv-transfer gui` to open the app.
[Cargo install reference](https://doc.rust-lang.org/cargo/commands/cargo-install.html)

Once the deck is connected, `universal-dv-transfer doctor` checks FFmpeg and
lists any FireWire devices the native backend can see.

## Implemented

- Native FireWire device discovery, DV capture, and DV output adapters for Windows, Linux, and macOS with its older FireWire stack.
- A native Rust GUI (egui/eframe) and command-line interface.
- Auto capture follows mixed DV/DVCAM and HDV tapes, keeping separate original streams and a report of switches and interruptions.
- Bounded capture buffers, frame and dropped-frame counters, recording date, timecode, and always-on GUI video preview.
- AVI Type 1 (`iavs`, interleaved DV) and Type 2 (DV video plus PCM audio). Both support large OpenDML AVI files. The DV video payload is copied without encoding.
- Type 2 AVI scan flags follow the DV stream (progressive, bottom field first, or top field first). A change of scan mode starts a new file.
- HDV capture and tape output preserve MPEG-2 video, audio, private data, PCR/PTS/DTS, interlacing, pulldown, and the original cadence. There is no transcoding or frame-rate conversion.
- HDV frame rates are detected from sequence headers and extensions using exact rational values: 23.976 (24000/1001), 24, 25, 29.97 (30000/1001), 30, 50, 59.94 (60000/1001), and 60 fps. The parser also handles the MPEG-2 frame-rate extension multiplier/divisor.
- HDV transport normalization handles 188-byte TS packets and 192-byte packets with source headers, fragmented callbacks, continuity errors, and PCR wrap. Scene/manual/frame-limit splits inspect MPEG headers and retain indexed GOP preroll and complete audio PES across cuts.
- Scene splitting on recording-date discontinuities, maximum frames per file, manual splitting, and every Nth frame capture.
- Date-based names, configurable number widths, collision-safe file creation, and timed capture.
- Capture pause/resume and preview while capture is paused.
- DV/AVI playlists recorded back to tape, with optional leader and appended clips, wildcard expansion, pause/resume, and optional automatic tape control.
- File-source capture and playlist export to a new `.dv` file, for hardware-free testing.
- Saved GUI settings, source validation, error reporting, and AVI finalization on normal stop and source failure.

Build and media tests have run on Windows and Linux. Physical device compatibility depends on the deck, driver, and adapter. Physical HDV recording to tape and macOS hardware support still need verification.

## Using the GUI

Select the deck and output folder, choose AVI Type 1 or Type 2, and click **Start capture**. The GUI automatically previews the selected deck whenever it is connected and sending video, including before capture and after stopping. It creates no recording files until capture starts. With automatic deck control disabled, start tape playback manually. You can also drag DV/AVI files into the window. **New file** starts a fresh capture segment. **Pause** finalizes the current file and keeps the monitor available.

For HDV, select **HDV** and set the camera's FireWire/i.LINK output to HDV with DV downconversion disabled. Captures are `.m2t`, with no AVI settings or fps selection required. File sources are also detected by their transport-packet layout. HDV preview retains the 16:9 aspect ratio. HDV1 720p includes the 24/25/30/50/60p families; HDV2 includes 50i/59.94i carriers and native 24/25/30p families. An interlaced carrier's 50 or 59.94 fields/s corresponds to 25 or 29.97 coded frames/s. Progressive-over-interlace and pulldown remain as recorded.

Record HDV `.m2t`, `.ts`, or `.m2ts` files using the playlist. Every packet is checked before tape output opens. A playlist must keep the same resolution, scan mode, and frame rate and cannot mix DV with HDV. The deck must support the selected HDV family/rate and have HDV input enabled. Tape output generates FireWire source headers and clip-boundary discontinuity signals; the MPEG video/audio packets are unchanged. Raw file export simply concatenates normalized TS packets and preserves the source clip timestamps. Arbitrary MP4/MKV videos are not automatically encoded for HDV tape.

See [tape preparation instructions](docs/ffmpeg-tape-encoding.md) for PAL DV and the free `scripts/prepare-hdv1080i50.py` tool. The HDV tool combines FFmpeg with HDV2 AUX/clock authoring; its generated 1080i50 output was confirmed to display on the Sony HVR-M25AC monitor. The user confirmed PAL DV recording; physical HDV recording and playback still require validation.

On Windows, match the deck's **HDV/DV SEL** setting to the playlist format and
reconnect FireWire after changing it. HDV output needs the MSTape input;
DV output needs MSDV. Keep HDV-to-DV conversion off for HDV. The app checks the
recording input and reports a format/driver mismatch before sending a Record
command. HDV uses the input's complete advertised format block and a direct
connection without intermediate conversion filters.

For DV tape output, select **HDV/DV (i.LINK)** as the deck's recording input and
match its recording system to the file (50i/PAL or 60i/NTSC). On the Sony
HVR-M25AC, another input/system setting can let the Windows pin connect but
prevent the output stream from starting (`0x800705AA`). **Start recording on
the deck automatically** is enabled by default on the Record to tape tab. It
has its own saved setting, separate from capture playback control. Disabling
it changes the action to **Send playlist to deck**; start recording manually
on the deck in that mode.

HDV scene splitting uses the same **Scene gap** setting as DV. A recording-date jump strictly larger than the threshold, or a recording-start marker, requests a new file; zero disables both automatic triggers. Supported Sony-compatible AUX metadata also supplies filenames and the displayed recording date/timecode. Other metadata layouts remain in the recording; when their dates cannot be read, manual and frame-limit splitting still work. Frame-rate detection and splitting work with both 720p and 1080 formats.

HDV files deliberately overlap by about one GOP to retain backward picture references and audio PES headers. Audio packets crossing a video cut are also completed in the preceding file. Encoded video/audio, timestamps and private streams are copied without re-encoding or remuxing. A split waits for validated sequence/GOP/I-picture headers and sufficient audio preroll; unusually long or unrecognized GOPs defer the split while capture continues. The 8 MiB preroll bound never causes capture packets to be discarded. Keep the `.hdv.jsonl` sidecar: it records each file's added prefix and audio tail. Removing those indexed duplicate packets and concatenating the unique ranges reconstructs the received 188-byte stream exactly. Simply joining whole overlapping files repeats footage.

Names use recording dates when available in the initial 512 KiB, otherwise capture time; later scene files use the available recording date. The frame limit counts unique received pictures and is approximate at GOP boundaries. Every-Nth sampling is disabled for HDV. The splitter preserves received data; it cannot recover packets a deck or driver never delivered. Native hardware compatibility still requires testing on each OS.

On **Record to tape**, add DV or DV AVI files in playback order (or use Up/Down to reorder them) and optionally choose leader and append clips. **Record playlist to tape** sends a Record command to the selected deck and transfers the playlist, replacing video at the tape's current position. The deck must support DV input. The entire playlist is checked before any Record command is sent. With automatic recording disabled, **Send playlist to deck** transfers video for monitoring or manual recording. **Export original stream** uses the same playlist reader without touching hardware and refuses to overwrite an existing file. The CLI still requires `--control-deck` to engage recording automatically.

Preview runs independently of source callbacks and displays its measured fps. Interlaced input is deinterlaced for viewing at 50 or 59.94 images/s; progressive input follows its recorded cadence. These display operations do not alter captured video. Preview is best-effort and may discard DV display frames or restart an overloaded HDV decoder to protect capture. The preview has no audio monitor. Tape audio stays embedded in the captured DV, and Type 2 also exposes PCM tracks. Capture file limits apply to written frames; every-Nth sampling restarts for each segment, matching WinDV. A recording-date gap must be strictly greater than the configured threshold to split. Zero disables date splitting, the frame-count limit, or the duration limit respectively.

Long DV captures use OpenDML to continue in the same AVI past 3.9 GB. With scene splitting off and no frame cap, a continuous capture of the same video standard stays in one AVI. Pausing capture, manually starting a new file, or a format change that needs a new header can still start another file. The destination drive must support files over 4 GB (for example, NTFS or exFAT rather than FAT32).

## Mixed tapes

Choose **Auto (mixed tape)** for capture, or use `capture --format auto`. Auto is the default for new settings and CLI device capture; existing saved DV/HDV selections remain available. Configure the deck for automatic HDV/DV output and turn HDV-to-DV conversion off. The app can follow only formats the deck actually sends over FireWire.

Auto writes `.dv` and `.m2t` segments without transcoding, drains the old stream before closing each boundary, and keeps native polling separate from disk writing. After reception stops it prepares the selected AVI type from the DV segments. Original `.dv` files remain available if packaging fails and are retained after success; allow space for both DV and AVI copies. Fixed **DV / DVCAM** mode continues to write AVI during capture and supports every-Nth sampling.

A `.capture.jsonl` report records format switches, files, incomplete data, receive losses, and driver recovery. Incomplete DIF/TS samples are saved as recovery files. Auto never waits for a new HDV GOP at a format change: it retains the received boundary packets even if decoding must wait for later program tables or an I-picture. Scene, manual and frame-limit splits within HDV use the same GOP overlap and `.hdv.jsonl` sidecars as fixed HDV capture. The mixed-capture report links those sidecars.

**Automatic switching cannot guarantee every tape frame.** A deck may interrupt or corrupt output while locking to a new recording format; Windows may switch between MSDV and MSTape, requiring a new capture graph. Auto matches the selected physical FireWire device, records interruptions, and retries rather than selecting another deck. Missing-frame counts at an unknown boundary remain unknown. Review and recapture questionable boundaries with overlap; the app does not automatically rewind or merge retries. See [Sony's HVR-1500A playback restrictions, page 39](https://pro.sony/s3/cms-static-content/operation-manual/3879632121.pdf).

The receiver retries opening during driver changes, including when the first open fails. Windows also checks for a changed driver endpoint and for missing samples while the deck reports PLAY. Recovery applies to idle preview and paused Auto capture as well as active capture. Some deck/driver combinations retain the previous endpoint until FireWire is reconnected; the app cannot receive DV through an HDV-only endpoint or HDV through MSDV.

[Windows switching research](docs/windows-format-switching.md) documents successful software reconnects in both DV-to-HDV and HDV-to-DV directions on the HVR-M25AC through its AV/C device node, followed by live reception in each format. The diagnostic script is read-only by default; automatic device restart is not yet integrated into the app. A restart still introduces a receive gap.

Linux and eligible macOS builds receive both CIP formats in one connection. Windows detects the exposed DV/TS media type and rebuilds after a driver error or two seconds without samples. Reconnection is bounded to 30 seconds; capture stops after 120 seconds without media data. Actual switching and first/last-frame completeness require physical deck validation on each platform.

## CLI

Add `--no-default-features` to the build or install command for a CLI-only build. The following commands use the built executable; `cargo run -- <arguments>` also works (add the MSVC target on Windows).

```sh
universal-dv-transfer devices
universal-dv-transfer doctor
universal-dv-transfer inspect recording.dv
universal-dv-transfer capture --output captures --avi-type type2 --control-deck
universal-dv-transfer capture --format auto --output mixed-tape --control-deck
universal-dv-transfer capture --source recording.dv --output captures --avi-type type1 --max-frames 1000
universal-dv-transfer capture --format hdv --output hdv-captures --control-deck
universal-dv-transfer capture --source recording.m2t --output hdv-captures --max-frames 0
universal-dv-transfer inspect recording.m2t
universal-dv-transfer record recording.m2t --control-deck
universal-dv-transfer record recording.m2t --output exported.m2t
universal-dv-transfer record first.avi second.avi --leader leader.avi --trailer end.avi --control-deck
universal-dv-transfer record first.avi second.avi --output joined.dv
```

If more than one device is connected, select its ID from `devices` with `--device`. Quoted wildcard patterns are expanded in sorted order within each playlist item. `Ctrl+C` requests a stop and finalizes the capture. Settings use this application's own per-user configuration directory; WinDV's registry is not read or changed.

## Diagnostic logs

GUI sessions and CLI runs write `logs/udvt-*.log` inside the capture folder (normally `captures/logs`). **Tools & compatibility** shows the current log path. Logs include capture settings, device identity, native output-pin types, negotiated format, connection-stage errors, initial sample delivery, FFmpeg commands/stderr, and the first decoded preview frame. Logging uses a bounded background queue so callbacks do not wait for disk writes. Auto capture also keeps its `.capture.jsonl` report.

If both preview and capture show zero received data, check the native input first. A Sony deck can play HDV locally while Windows still exposes its DV interface through MSDV, or retain an HDV interface after switching to DV playback. Select the required i.LINK format with HDV-to-DV conversion disabled, then reconnect/refresh so Windows exposes the matching interface. For example, the HVR-M25AC appeared as `Microsoft DV Camera and VCR` in DV mode and `Sony HVR-M25AC AV/C Tape Recorder/Player` in HDV mode. The GUI's idle preview owns the device too; close the app before opening a separate native capture/probe. Inside the GUI, starting a transfer closes its idle receiver before opening the transfer receiver.

## Development

On Windows, add `--target x86_64-pc-windows-msvc` to the Cargo build, test, and Clippy commands.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test -- --ignored             # real DV/HDV fixtures, requires FFmpeg
```

The [architecture and feature map](docs/architecture.md) describes the Rust/native boundary and the DV/HDV paths. Platform CI is in `.github/workflows/ci.yml`. [Third-party notices](THIRD_PARTY.md) describe the reference projects and the Apple SDK license.
