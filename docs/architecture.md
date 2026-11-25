# Architecture and WinDV feature map

The application is Rust except for the OS adapters and Apple's vendored streaming SDK. Native code exposes one C ABI in `native/bridge.h`; it does not own naming, scene splitting, AVI writing, the UI, settings, or playlists.

| WinDV behavior | New implementation |
| --- | --- |
| Buffered input, independent disk writing | `engine::Feed`, bounded channels, callback overflow counters |
| DV recording date and timecode | `dv`, subcode packs 0x62 / 0x63 / 0x13, validated BCD |
| Scene, size, and manual splitting | `engine::capture` |
| Every Nth frame, capture duration | `CaptureOptions`, capture loop |
| Timestamp names and numeric suffixes | `naming::reserve`, atomic create-new |
| Type 1 AVI | `avi::Type1Writer`, legacy idx1 for the first RIFF, plus OpenDML indexes and internal AVIX sections for large files |
| Type 2 AVI | `media::Writer`, FFmpeg stream copy plus PCM audio |
| Preview before, during and after transfers | Exclusive idle receiver, nonblocking preview queue and paced FFmpeg RGB decoder |
| Multiple AVI sources plus leader/append | `media::expand_playlist`, recording preflight and sequential readers |
| Pause/resume tape output | Native output plus repeated last frame while paused |
| Deck control | `backend::Transport`, C ABI commands |
| GUI and command line | `ui`, `main` |

## Ownership and threads

The UI only reads shared snapshots and sends commands. A controller owns one transfer worker. Capture has a separate source worker: it owns the native session and pumps its event loop while the transfer worker writes files. Callbacks copy DV bytes into bounded channels without blocking. Invalid or incomplete frames and queue overflow increment dropped counts. Native handles are explicitly not Send/Sync in Rust. Destruction closes the native stream before freeing callback context.

The source worker forwards device commands. On shutdown it processes the final pause command before closing its session. Source errors enter the same bounded queue as frames and cannot be silently lost behind a full buffer. Capture always attempts to finalize its current AVI after a source error. A disk or finalization failure is shown as an error; a partially written file is retained for recovery.

Recording first checks every frame of every input and rejects mixed PAL/NTSC or unreadable files before opening a tape output. Raw DV is copied directly. Type 1 AVI's interleaved stream is extracted without remuxing. Type 2's separate PCM tracks are rebuilt into the DV stream with FFmpeg; this may regenerate DV audio/metadata packs, as expected for a remux. Video is never re-encoded. The playlist is read again for output. This preflight trades additional disk I/O for detecting input failures before a deck starts recording; edits to source files during output are unsupported.

GUI tape output uses `Settings.record.control_deck`, enabled by default and
independent of capture playback control. Older settings acquire this separate
default instead of inheriting `capture.control_deck = false`. An explicit manual
output choice is persisted and labelled **Send playlist to deck**. Export never
enables deck control. The CLI retains its explicit `--control-deck` flag. Logs
include the output control choice and each transport command/result. Windows
only treats `S_OK` as command acknowledgement, normalizing the positive Win32
errors that [MSDV can return](https://learn.microsoft.com/en-us/windows/win32/directshow/msdv-driver).

Type 2 capture reads the IL and FS bits of DV25 VAUX source-control packs (0x61) and explicitly supplies FFmpeg's output `-field_order:v:0` option. Otherwise stream copy can leave field order unknown, causing FFmpeg's AVI `vprp` header to declare progressive video even when the DV stream is interlaced. Redundant packs are resolved by majority; missing or contradictory flags remain unspecified. A known scan-mode change starts a new Type 2 file so its header describes that segment; a frame with unreadable flags alone does not split the capture. Type 1 retains the flags inside each DV frame.

Native output is drained before automatic record-pause at completion. All tape writes require the explicit Record action; opening the app, listing devices, and previewing never put a deck into record mode.

## Native adapters

Auto capture uses `auto_capture::Feed` and `CaptureArchive`. The native source worker owns a universal session and continually polls into a bounded FIFO; the writer independently assembles DV DIF frames and normalizes MPEG TS. Callback envelopes explicitly distinguish complete DV, TS samples, and IEC 61883 CIP packets. CIP FMT selects DV25 or MPEG-2; packet continuity and transport continuity are checked. Partial frames and unrecognized samples are retained in recovery files instead of inserted into an AVI.

Format changes drain the prior decoder, flush/sync the old segment, and start an original `.dv` or `.m2t` file. No HDV packet is discarded to find an initial GOP. Type 1/2 AVI packaging occurs after reception stops, with the raw DV retained. Source shutdown stops callbacks and drains both FIFOs before finalization. A journal records switches, validation findings, native receive loss, recovery attempts, and unverified boundaries. Capture reports never assert zero loss from an unobserved tape boundary.

Windows Auto inspects DirectShow output pin media types and follows dynamic sample types. Device identity prefers the physical 1394 parent shared across MSDV/MSTape bindings, then ROM UniqueID properties, with an exact moniker fallback. Every 500 ms it checks the moniker without instantiating another device filter. A changed/missing endpoint, graph error, or four seconds without samples while the transport reports PLAY requests recovery, including in preview and paused capture. An idle STOP/PAUSE alone does not reopen the receiver. Both initial open and reconnect retry for up to 30 seconds with cancellation and rate-limited diagnostics. The driver may still need physical reconnection to expose the new format.

HDV input connects the device's offered transport-stream pin directly through Sample Grabber to Null Renderer, avoiding Intelligent Connect's stream selection. Input graphs use device pacing with no renderer reference clock so tape timestamp resets cannot stall reception. Teardown stops delivery, unregisters the callback, disconnects both ends of the input pins and removes the filters before reopening. Tape output keeps its paced graph.

Linux Auto uses raw1394 isochronous CIP input on one CMP connection, bypassing fixed-format libiec61883 decoders. macOS Auto uses Apple's UniversalReceiver and reports receive overruns. Reset recovery creates a fresh session, resolving the GUID again. A driver/deck outage can omit source data; recovery is logged without fabricating a frame count or silently starting a different deck.

The GUI owns a separate read-only idle monitor using the universal receiver and no capture archive. It closes and joins that receiver before a capture or tape output opens, and resumes monitoring after the hardware is released, including during Auto's AVI packaging. One lazy `LivePreview` decoder follows the received format and resets its references on reconnect. Its RGB reader publishes independently of input callbacks, at the source frame/field cadence; the GUI repaints frequently enough to show 50/59.94 fps. Scaling and field deinterlacing affect the display only. HDV preview coalesces small callbacks into fixed-size blocks and uses a bounded startup queue; overflow invalidates and restarts that decoder instead of silently losing predictive input. Capture uses its separate original-data queue.

- Windows: COM MTA, DirectShow/MSDV, property-bag discovery without throwaway MSDV filter instances, Sample Grabber capture, and a minimal push source feeding MSDV for output. Every native entry catches C++ exceptions. DirectShow is retained because it exposes the DV driver and external transport interfaces used by WinDV.
- Linux: libraw1394 plus libiec61883 DV frame reception/transmission, librom1394 discovery, libavc1394 deck commands, CMP allocation/release, and bus-reset failure handling. The transmission callback consumes 480-byte DIF packet groups; libiec61883 owns the raw handle's userdata.
- macOS: Apple AVCVideoServices managed device streams, direct DV reception/transmission, GUID discovery, AV/C tape commands, and locked frame-buffer queues between SDK callback threads and the Rust worker. SDKs without FireWire compile an explicit unavailable backend, preserving the file workflows.

## Deliberate first-milestone limits

HDV uses `hdv::Packetizer`, `Inspector`, and `Reader`, plus the separate `hdv_engine` transfer worker. It never passes through the DV DIF parser or AVI writer. Captures preserve every normalized 188-byte TS packet, including private metadata, audio variants, pulldown and timestamps. PAT/PMT section assembly checks CRCs; video PES assembly locates MPEG-2 sequence headers and sequence extensions, deriving exact rational rates. PCR handles the 33-bit base wrap; continuity and transport errors are counted during capture and rejected during recording preflight. Validated file inputs and file exports preserve normalized transport bytes. Frame sampling is rejected for HDV.

The shared `hdv_capture::Writer` serves fixed HDV and Auto capture. `Inspector` discovers AUX stream type 0xA1 from CRC-validated PMTs, reassembles its private PES across TS packets, and validates the Sony-compatible date/timecode BCD layout. Unknown layouts remain untouched. Date discontinuities and debounced recording-start markers request a split. `Scene gap = 0` disables both; max-frame/manual requests remain available.

The splitter verifies sequence, GOP and I-picture headers across video TS payloads instead of trusting RAI. A bounded lookahead keeps the candidate PES header in the correct file. A rolling 8 MiB cache supplies the preceding GOP and checks audio PES starts; missing prerequisites defer cuts without dropping input. Cached program tables are added, adjusting counters only in those added copies to match the older overlap. Audio PES crossing the cut are completed in the preceding file, with those duplicated tail packets indexed separately. Each segment otherwise contains a continuous, unmodified range of original TS packets. The JSONL sidecar indexes prefixes, tails and unique source packet offsets, allowing byte-exact reconstruction. Preroll is deliberately visible and may contain a partial first audio/video dependency already preserved in the preceding file. The requested scene boundary and frame limit are approximate at GOP boundaries. Receive errors invalidate candidate preroll and are still reported.

Recording dates supply filenames when found in the bounded initial buffer, and on subsequent cuts. Tape recordings without recognized AUX metadata use capture time and manual/frame-limit splitting. Format facts were checked against the upstream [dvgrab AUX parser](https://github.com/ddennedy/dvgrab/blob/master/iec13818-1.cc); the Rust parser and splitter are independently implemented. No upstream code was copied.

Tape playlists require matching video formats, with adaptation-only discontinuity packets at clip boundaries to signal timestamp/counter resets. File export omits these added packets. Record-pause drains/closes the transmitter and resume reopens it, without replaying compressed pictures.

Windows HDV uses the MSTape stream pins and MPEG2_TRANSPORT_STRIDE media type,
generating 192-byte source packets with timing interpolated between adjacent PCR
markers. A bounded queue retains packets until the next marker; short writes
and playlist boundaries do not insert padding. Only the final driver sample is
padded. Clock resets keep the outgoing timeline continuous. The original Sony
capture has been confirmed to display on the stopped HVR-M25AC with this clock;
physical HDV tape recording remains unverified. Device-output preflight requires
HDV program signalling and HDV2 video AUX declarations, while file workflows
continue to accept generic supported MPEG-TS. These checks do not certify a
stream's complete HDV conformance.

Linux receives with libiec61883 MPEG-2 and transmits IEC 61883-4 CIP/SPH packets
through raw1394 isochronous output at the measured transport bitrate; this
preserves initial program tables without libiec61883 transmitter PCR lookahead
discarding them. macOS uses the vendored SDK's MPEG receiver/transmitter and its
PCR scheduler. Those adapters still require physical output validation; macOS
compilation requires a suitable host and SDK.

DVCPRO and rescue/merge analysis remain outside this implementation.

There is no audio preview, tray integration, installer, or automatic software update mechanism. Native hardware compatibility has to be established with real decks. Recording must be checked on expendable tape before valuable material is used.

## Research references

- WinDV: local `DShow.cpp`, `DV.cpp`, `DVToolsDlg.cpp`, capture/record configuration, and MIT license. [Upstream](https://github.com/hfiggs/WinDV).
- WinDV2: engine, frame queue, DV metadata/naming core, thread-lifetime notes, and MIT license. [Upstream](https://github.com/mak0t0san/WinDV2).
- DVrescue: LinuxWrapper and AvfCtl show native capture, AV/C transport, and preservation concerns. No DVrescue source was copied. [Upstream](https://github.com/mipops/dvrescue).
- Microsoft: [DV capture](https://learn.microsoft.com/en-us/windows/win32/directshow/capture-dv-to-file), [MSDV](https://learn.microsoft.com/en-us/windows/win32/directshow/msdv-driver), [IMemInputPin](https://learn.microsoft.com/en-us/windows/win32/api/strmif/nn-strmif-imeminputpin).
- FFmpeg: [device documentation](https://ffmpeg.org/ffmpeg-devices.html), official DV muxer and AVI/DV demuxer source.
- Scan metadata: [FFmpeg DV decoder](https://github.com/FFmpeg/FFmpeg/blob/n7.1/libavcodec/dvdec.c), [AVI writer](https://github.com/FFmpeg/FFmpeg/blob/n7.1/libavformat/avienc.c), and [MediaInfo DV parser](https://github.com/MediaArea/MediaInfoLib/blob/master/Source/MediaInfo/Multiple/File_DvDif.cpp).
- Apple SDK and macOS compatibility: [AVCVideoServices source preserved by tapecap](https://github.com/xingrz/tapecap/tree/master/third_party/AVCVideoServices), [build requirements](https://github.com/xingrz/tapecap).

HDV references: [Microsoft MSTape media types](https://learn.microsoft.com/en-us/windows/win32/directshow/mstape-driver), [AV/C streaming formats](https://learn.microsoft.com/en-us/windows-hardware/drivers/stream/av-c-streaming-format-guids), [JVC GY-HD200 format tables](https://pro.jvc.com/pro/attributes/CAMERA/manual/GYHD200inst.pdf), and [Sony HDV format overview](https://pro.sony/s3/cms-static-content/uploadfile/85/1237494517385.pdf).
