# Preparing video for tape with FFmpeg

Existing compatible `.dv`, DV AVI and original HDV `.m2t` captures can be used
directly. Converting an H.264/HEVC/ProRes source to DV or HDV is lossy encoding;
changing a filename extension is insufficient.

The recipes below target a **50 Hz deck setting** and assume **progressive,
SDR, 16:9 input at 25 or 50 fps, with an audio track**. They are not universal
conversions for interlaced, HDR, 24p, 29.97/59.94 or non-widescreen sources.

The Sony HVR-M25A family records 1080-line HDV, including 1080/50i; its manual
lists HDV 720p as unavailable for i.LINK recording. The connected deck identifies
itself as HVR-M25AC. Use a 1080 HDV recording format for this deck, rather than
the app's generic HDV1 720p support.
[Sony manual, pages 32-33](https://pro.sony/s3/cms-static-content/operation-manual/3878928131.pdf).

## DV PAL, 16:9

Run in PowerShell with a recent FFmpeg (tested here with FFmpeg 7.1):

```powershell
ffmpeg -n -i "input.mp4" -map 0:v:0 -map 0:a:0 `
  -vf "fps=50,scale=720:576:flags=lanczos:out_color_matrix=bt601:out_chroma_loc=topleft,setsar=64/45,tinterlace=mode=interleave_bottom" `
  -target pal-dv -aspect 16:9 "output-pal.dv"
```

This creates DV25 at 720x576, 25 coded frames / 50 fields per second, bottom
field first, 4:2:0, with stereo 16-bit PCM at 48 kHz. The nonsquare pixels give
16:9 display. `pal-dv` supplies the format, encoder, size/rate/pixel format and
audio defaults. Use `pal-dv`, not `pal-dv50`: the latter is DVCPRO50, outside this
app's DV25 tape path. `-n` refuses to overwrite an existing output file.
[FFmpeg target documentation](https://ffmpeg.org/ffmpeg.html#Main-options).

`fps=50` supplies progressive frames to the interlacing filter. With 50p input,
the two fields represent different motion instants. With 25p input, frames are
duplicated before weaving, retaining 25p motion inside the interlaced carrier;
the filter does not invent extra motion. The PAL chroma siting is explicitly
set for the DV encoder.
[FFmpeg filters](https://ffmpeg.org/ffmpeg-filters.html#tinterlace).

DVCAM uses the same DV25 video encoding; the deck's recording-mode setting
determines DVCAM versus DV SP on tape.
[Sony recording settings, page 70](https://pro.sony/s3/cms-static-content/operation-manual/3878928131.pdf).

## HDV 1080i50 preparation — Sony monitor decoding verified

The free preparation tool combines FFmpeg encoding with HDV2 transport authoring:

```powershell
python "C:\dev\universal-dv-transfer\scripts\prepare-hdv1080i50.py" "input.mp4" "output-hdv.m2t" --normalize-audio
```

Requires Python 3.10+, FFmpeg and FFprobe on PATH. Tested with FFmpeg 7.1 on
Windows. Omit `--normalize-audio` to leave loudness unchanged; when selected it
uses `loudnorm=I=-16:TP=-1.5:LRA=11`. Inputs without audio receive silent stereo.
The source remains unchanged and existing output files are rejected.

A completed encode is checkpointed in `<output>.hdv-work/`. If transport
preparation fails, re-run the **same command**: the tool reuses that encode
when the input and normalization settings match. This avoids repeating a long
encode. The work files are removed after successful preparation unless
`--keep-intermediate` is specified. Interrupted encodes are restarted; only a
completed, matching encode can be reused.

This tool targets **SDR 16:9 input and HDV2 1080i50**: 1440x1080, 25 coded
frames / 50 fields per second, top field first, BT.709, MPEG-2 Main/High-1440,
25-Mbit/s video and 384-kbit/s 48-kHz stereo MP2. Other source cadences are
converted to this output cadence. HDR conversion, other aspect ratios, HDV1
720p and 1080i59.94 authoring are outside this tool's verified scope. The app's
existing capture support for those formats is separate.

The encoder uses the captured Sony's quantization matrices and matching
quantization/scanning settings. The adapter then writes TSHV registration,
PAT/PMT/SIT, dedicated PCR and timed AUX-V/A streams. It aligns PCR with the
picture DTS/VBV-delay model, removes transport padding and verifies SHA-256
identity of the retained compressed picture/audio payloads across adaptation.
It does not copy dates or camera settings from an unrelated recording.

FFmpeg inserts additional PCR markers near keyframes. The adapter coalesces
markers closer than 20 ms and removes their empty original transport packets.
This prevents the added HDV packets from creating brief delivery-rate spikes;
the original clock values, picture/audio payloads and AUX timestamps remain
unchanged. The 100-ms clock-spacing and 33-Mbit/s transport limits are still
enforced. The check is not bypassed by increasing its threshold.

The encode adds black/silent padding. Only padding is removed when the
encoder's initial GOP is discarded; approximately one second of black leader
and half a second of black tail remain. All source pictures after the deliberate
frame-rate conversion are retained. The MP2 timestamps receive a small unit
alignment adjustment (about 2 ms with FFmpeg 7.1).

On 2026-10-22 the user confirmed that both the diagnostic authored stream and
the complete tool's output displayed a moving colour pattern on the stopped
HVR-M25AC over FireWire. The end-to-end test decoded all 300 source pictures
from a 12-second test, plus 36 padding pictures; opening and ending colour
markers remained intact. **Physical HDV tape recording and playback have not
yet been verified.** Add the prepared `.m2t` to the app's Record to tape playlist
with the Sony configured for HDV input and 50i/PAL.

Run the optional preparation regression with:

```powershell
python tests/test_prepare_hdv1080i50.py
```

## Why matching the MediaInfo report was insufficient

The captured file also contains timed HDV AUX streams and transport information
that the report does not fully describe. A closer FFmpeg codec match and static
TSHV/table changes still produced a blank monitor. Removing AUX streams and
their PMT declarations from a known-good Sony capture also made it blank;
restoring the original capture restored video. Keeping its video-frame AUX
packs while removing extra camera-information packs still played.

In addition, the generic FFmpeg mux clock differed from its MPEG picture
DTS/VBV-delay arrival model by about 0.5 seconds, while the native capture
agreed within roughly 0.6 ms. Generated AUX plus the uncorrected clock remained
blank. The authored copy with corrected clock, no null padding and matching
BT.709 signalling displayed video. These tests establish a working combination,
not proof that every individual encoder option is mandatory.

There is still no verified **FFmpeg-only** command here: the working command
above invokes FFmpeg and then the HDV adapter. FFmpeg 7.1's MPEG-TS muxer does
not synthesize these HDV AUX frame packs.
[FFmpeg MPEG-TS muxer source](https://raw.githubusercontent.com/FFmpeg/FFmpeg/n7.1/libavformat/mpegtsenc.c),
[GStreamer HDV AUX field descriptions](https://coral.googlesource.com/mtk-gst-plugins-bad-debian/+/b41a1808f67bbb9111ba6dd8d64a841c73efc157/gst/hdvparse/gsthdvparse.c).

## Earlier generic 1080i50 test encode — does not play on this Sony

FFmpeg has no `-target hdv` preset. This is an explicit MPEG-2/MP2 transport
recipe for testing against the deck, not a certified Sony HDV authoring preset.

```powershell
ffmpeg -n -i "input.mp4" -map 0:v:0 -map 0:a:0 `
  -vf "fps=50,scale=1440:1080:flags=lanczos,setsar=4/3,tinterlace=mode=interleave_top,setfield=tff" `
  -c:v mpeg2video -pix_fmt yuv420p -profile:v main -level:v 6 `
  -b:v 25M -minrate:v 25M -maxrate:v 25M -bufsize:v 7340032 `
  -g 12 -bf 2 -sc_threshold 1000000000 -flags:v +ildct+ilme `
  -color_primaries bt709 -color_trc bt709 -colorspace bt709 `
  -c:a mp2 -b:a 384k -ar 48000 -ac 2 `
  -streamid 0:2064 -streamid 1:2068 -mpegts_service_id 100 -mpegts_pmt_start_pid 0x100 `
  -muxrate 26700000 -pcr_period 20 -f mpegts "output-hdv-1080i50.m2t"
```

The video is MPEG-2 Main Profile / High-1440 Level (numeric level 6), 1440x1080,
25 coded frames / 50 fields per second, top field first, 4:2:0 and 16:9 display.
Audio is MPEG-1 Layer II, 48 kHz stereo at 384 kbit/s. These video/profile/audio
properties agree with the user's existing Sony capture. The chosen video/audio
PIDs also match that capture; FFmpeg does not synthesize its Sony AUX streams.
The 26.7 Mbit/s mux rate includes audio, transport overhead and padding; the
video bitrate remains 25 Mbit/s. This command alone is retained as a diagnostic
example; use the preparation tool above for the tested HDV transport authoring.
[FFmpeg MPEG-2 encoder](https://ffmpeg.org/ffmpeg-codecs.html#mpeg2),
[MPEG-TS muxer options](https://ffmpeg.org/ffmpeg-formats.html#mpegts),
[Sony audio specifications, page 100](https://pro.sony/s3/cms-static-content/operation-manual/3878928131.pdf).

For DV or the authored HDV output, add the resulting file to the app's **Record to tape**
playlist. Keep DV and HDV in separate playlists and select the matching input
mode on the deck. The user confirmed PAL DV recording on this Sony. The HDV
recipe above has **not** produced a working input picture on this Sony, even
with corrected output timing. Adding the captured Sony's TSHV registration
descriptor alone did not resolve it. It is an experimental MPEG encoding recipe,
not a usable, verified tape-authoring command for this deck. The app now rejects
missing HDV program signalling and missing HDV2 video AUX declarations before
opening a tape-output session. File capture/export and preview still accept
generic MPEG-TS with the supported video dimensions and rates.

An original Sony HDV capture **does** display on the stopped deck with the new
PCR-derived Windows output timing. Physical HDV recording and playback still
need verification on blank/expendable tape. After changing HDV/DV SEL, reconnect FireWire so Windows
loads the matching DV (MSDV) or HDV (MSTape) endpoint.

## Check the source and output

```powershell
ffprobe -v error -show_entries stream=codec_name,width,height,r_frame_rate,field_order,sample_aspect_ratio,display_aspect_ratio,pix_fmt,color_transfer,color_primaries,sample_rate,channels -of json "input.mp4"
```

Use the same command on the output. FFprobe may omit DV stream-level field
order; inspect its first decoded frame instead:

```powershell
ffprobe -v error -read_intervals "%+#1" -select_streams v:0 -show_entries frame=interlaced_frame,top_field_first -of json "output-pal.dv"
```

Expected DV flags: `interlaced_frame=1`, `top_field_first=0`. For the HDV stream,
expect `field_order=tt`, `r_frame_rate=25/1`, and `display_aspect_ratio=16:9`.
These metadata checks establish the generated stream's properties, not physical
recordability or full HDV conformance.

- **Already interlaced input:** do not apply the progressive-source interlacing
  recipe unchanged. Preserve or deliberately convert its existing fields.
- **Other aspect ratios:** use the appropriate DV aspect ratio or pad/crop the
  image; the examples assume 16:9 and would distort a different aspect ratio.
- **HDR input:** tone-map to SDR before this workflow; merely setting BT.709
  metadata is not HDR conversion.
- **Other frame rates:** match the deck's recording system. Native 24p and
  29.97/59.94 sources require a separate cadence/field-rate decision.
- **No audio track:** the explicit audio mapping will fail. Add a silent stereo
  source or another intended audio track rather than silently omitting audio.

## Local verification — 2026-10-21

A generated three-second progressive 1280x720/50 SDR test source with stereo
audio was encoded using these settings. FFprobe confirmed the expected PAL DV
and MPEG-2 Main / High-1440 interlaced HDV-style output, aspect ratios, rates and
audio. Both files passed the current app's recording preflight and were exported
to files with identical SHA-256 hashes. No FireWire device was opened and no tape
was written. The results are under ignored `test-output/ffmpeg-tape-recipes/`;
the encoding and preflight logs are under `target/ffmpeg-tape-*.log`.
