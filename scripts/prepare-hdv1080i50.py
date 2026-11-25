#!/usr/bin/env python3
"""Prepare SDR video as HDV 1080i50 using FFmpeg and an HDV2 transport adapter.

Requires Python 3.10+, ffmpeg and ffprobe. No deck is opened or controlled.
The resulting format has been decoded by a Sony HVR-M25AC over FireWire;
physical tape recording and other decks/formats still require validation.
"""
from __future__ import annotations

import argparse
from bisect import bisect_right
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

PACKET = 188
VIDEO, AUDIO, AUX_V, AUX_A, PCR = 0x810, 0x814, 0x811, 0x815, 0x134
PREROLL = 1.44  # 36 frames; trimming the encoder's first GOP only removes black.
MIN_PCR_INTERVAL = 20 * 27_000  # Drop clustered keyframe PCRs; keep the actual clock values.
ENCODER_PROFILE = "hdv1080i50-ffmpeg-v1"
INTRA = (
    8,16,19,22,26,27,29,34,16,16,22,24,27,29,34,37,
    19,22,26,27,29,34,34,38,22,22,26,27,29,34,37,40,
    22,26,27,29,32,35,40,48,26,27,29,32,35,40,48,58,
    26,27,29,34,38,46,56,69,27,29,35,38,46,56,69,83,
)
INTER = (
    16,17,18,19,20,21,22,23,17,18,19,20,21,22,23,24,
    18,19,20,21,22,23,24,25,19,20,21,22,23,24,26,27,
    20,21,22,23,25,26,27,28,21,22,23,24,26,27,28,30,
    22,23,24,26,27,28,30,31,23,24,25,27,28,30,31,33,
)
PAT = bytes.fromhex("00b01100ffc100000000e01f0064e0817184a0d0")
PMT = bytes.fromhex(
    "02b02e0064c10000e134f00d050b54534856ff1080ffffffff"
    "02e810f00003e814f000a0e815f000a1e811f0004c856be8"
)
SIT = bytes.fromhex("7fb019ffffc10000f00a6308c14244ffffffffff00648000154c9eb0")


def packets(path):
    with Path(path).open("rb") as stream:
        index = 0
        while block := stream.read(PACKET * 4096):
            if len(block) % PACKET:
                raise ValueError("Incomplete transport packet")
            for offset in range(0, len(block), PACKET):
                p = block[offset:offset + PACKET]
                if p[0] != 0x47 or p[1] & 0x80:
                    raise ValueError("Invalid transport packet")
                yield index, p
                index += 1


def pid(p):
    return ((p[1] & 31) << 8) | p[2]


def payload_offset(p):
    return 5 + p[4] if p[3] & 0x20 else 4


def timestamp(b):
    return ((b[0] >> 1) & 7) << 30 | b[1] << 22 | (b[2] >> 1) << 15 | b[3] << 7 | b[4] >> 1


def write_timestamp(p, at, value):
    v = value % (1 << 33)
    p[at:at + 5] = bytes((
        (p[at] & 0xf0) | (((v >> 30) & 7) << 1) | 1,
        (v >> 22) & 255, (((v >> 15) & 127) << 1) | 1,
        (v >> 7) & 255, ((v & 127) << 1) | 1,
    ))


@dataclass
class Frame:
    index: int
    pts: int
    dts: int
    kind: int = 0
    delay: int = 0
    header_size: int = 0
    picture_offset: int = 0

    @property
    def arrival(self):
        # VBV delay measures the first picture-start bit's arrival to decoding.
        return (self.dts - self.delay) * 300 - round(self.picture_offset * 8 * 27_000_000 / 25_000_000)


def scan_frames(path):
    video, audio = [], []
    prefix, start = bytearray(), None

    def finish():
        if start is None:
            return
        b = prefix
        if len(b) < 19 or b[:4] != b"\0\0\1\xe0":
            raise ValueError("Expected one MPEG-2 video PES per picture")
        header_end = 9 + b[8]
        at = b.find(b"\0\0\1\0", header_end)
        if at < 0:
            raise ValueError("Missing MPEG picture header")
        word = int.from_bytes(b[at + 4:at + 8], "big")
        slices = [i for i in range(at + 8, len(b) - 3)
                  if b[i:i + 3] == b"\0\0\1" and 1 <= b[i + 3] <= 0xaf]
        if not slices:
            raise ValueError("Missing first MPEG slice")
        pt = timestamp(b[9:14])
        dt = timestamp(b[14:19]) if b[7] >> 6 == 3 else pt
        delay = (word >> 3) & 65535
        if delay == 65535:
            raise ValueError("Encoder did not supply finite VBV delays")
        video.append(Frame(start, pt, dt, (word >> 19) & 7, delay,
                           (slices[0] - header_end + 31) // 32, at))

    for n, p in packets(path):
        stream = pid(p)
        if stream not in (VIDEO, AUDIO) or not p[3] & 0x10:
            continue
        at = payload_offset(p)
        if at >= PACKET:
            continue
        if stream == VIDEO:
            if p[1] & 0x40:
                finish()
                prefix, start = bytearray(), n
            if start is not None and len(prefix) < 512:
                prefix.extend(p[at:at + min(PACKET - at, 512 - len(prefix))])
        elif p[1] & 0x40:
            b = p[at:]
            if len(b) < 14 or b[:4] != b"\0\0\1\xc0":
                raise ValueError("Unexpected audio packet layout")
            if 6 + int.from_bytes(b[4:6], "big") != 1166:
                raise ValueError("Expected one 384-kbit/s MP2 frame per PES")
            t = timestamp(b[9:14])
            audio.append(Frame(n, t, t))
    finish()
    if len(video) < 24 or len(audio) < 25:
        raise ValueError("Encoded stream is unexpectedly short")
    return video, audio


def crc(data):
    value = 0xffffffff
    for b in data:
        value ^= b << 24
        for _ in range(8):
            value = ((value << 1) ^ (0x04c11db7 if value & 0x80000000 else 0)) & 0xffffffff
    return value


def table(stream, section, counter):
    if crc(section):
        raise ValueError("Bad transport-table CRC")
    return bytes((0x47, 0x40 | (stream >> 8), stream & 255, 0x10 | (counter & 15), 0)) + section + b"\xff" * (183 - len(section))


def auxiliary(stream, body, counter):
    b = b"\0\0\1\xbf" + len(body).to_bytes(2, "big") + body
    length = 183 - len(b)
    if length < 1:
        raise ValueError("AUX packet is too large")
    return bytes((0x47, 0x40 | (stream >> 8), stream & 255, 0x30 | (counter & 15), length, 0)) + b"\xff" * (length - 1) + b


def bcd(value):
    return ((value // 10) << 4) | (value % 10)


def video_aux(frames):
    for i in range(0, len(frames), 3):
        group = frames[i:i + 3]
        f = group[0]
        if f.kind not in (1, 2) or any(x.kind != 3 for x in group[1:]):
            raise ValueError("Unexpected GOP layout; AUX units need I/P followed by two B-pictures")
        v = bytearray(58)
        v[0] = 0x39
        v[1:4] = (f.dts // 300 % (1 << 24)).to_bytes(3, "little")
        v[4] = len(group)
        v[5] = 1 if f.kind == 1 else 3
        v[6:8] = f.delay.to_bytes(2, "little")
        v[8] = f.header_size
        v[9:14] = f.dts.to_bytes(5, "little")
        v[14:16] = b"\x43\x16"  # TFF, 25 fps; 1080 HDV search modes.
        v[16:20] = (1440).to_bytes(2, "little") + (1080).to_bytes(2, "little")
        v[20] = 0x33  # 16:9, 25 coded frames/s.
        v[21:24] = (62500).to_bytes(3, "little")  # 25 Mbit/s / 400.
        v[24:26] = (448).to_bytes(2, "little")  # VBV / 16384.
        v[26:29] = b"\x46\x04\x63"  # Main/High-1440, component 4:2:0, GOP 12/3.
        seconds, frame = divmod(i, 25)
        minutes, second = divmod(seconds, 60)
        hour, minute = divmod(minutes, 60)
        v[29] = 1  # Valid title timecode; no invented camera recording date.
        v[30:34] = bytes((bcd(frame), 0x80 | bcd(second), 0x80 | bcd(minute), 0xc0 | bcd(hour % 24)))
        v[34:42] = b"\xff" * 8
        v[42] = 0x10 if i == 0 else 0x30
        v[43:58] = b"\xff" * 15
        body = b"\x44" + v + b"\x3f\xff\xff\xff\xff"
        yield group[-1].index, auxiliary(AUX_V, body, i // 3)


def audio_aux(frames, shift):
    for i in range(0, len(frames), 5):
        group = frames[i:i + 5]
        time = group[0].pts + shift
        a = bytearray(16)
        a[0] = 15
        a[1:4] = ((time - 3600) // 300 % (1 << 24)).to_bytes(3, "little")
        a[4] = len(group)
        a[5:10] = time.to_bytes(5, "little")
        a[12:14] = b"\xe1\x20"  # MPEG-1 Layer II, stereo, 48 kHz, 384 kbit/s.
        a[15] = 0 if i == 0 else 0x20
        yield group[0].index, auxiliary(AUX_A, b"\x40" + a, i // 5)


def elementary_hashes(path, video_start=0, audio_start=0):
    hashes = {VIDEO: hashlib.sha256(), AUDIO: hashlib.sha256()}
    for n, p in packets(path):
        stream = pid(p)
        if stream not in hashes or not p[3] & 0x10 or n < (video_start if stream == VIDEO else audio_start):
            continue
        at = payload_offset(p)
        if at >= PACKET:
            continue
        if p[1] & 0x40:
            at += 9 + p[at + 8]
        hashes[stream].update(p[at:])
    return {hex(stream): h.hexdigest() for stream, h in hashes.items()}


def adapt_transport(source, dest):
    large_file = Path(source).stat().st_size >= 256 * 1024 * 1024
    if large_file:
        print("Reading picture and audio timing...", flush=True)
    all_video, all_audio = scan_frames(source)
    first_v = next(i for i in range(1, len(all_video)) if all_video[i].kind == 1)
    source_start = all_video[0].pts + round(PREROLL * 90000)
    if first_v != 10 or any(f.pts >= source_start for f in all_video[:first_v]):
        raise ValueError("Initial GOP does not fit entirely inside the added black padding")
    video = all_video[first_v:]
    first_a = next(i for i, f in enumerate(all_audio) if f.pts >= video[0].dts + 3000)
    audio = all_audio[first_a:]
    # Align the 5-AAU / 3-picture AUX unit clocks. This is a small timestamp
    # correction (182 ticks with FFmpeg 7.1), not resampling or audio-frame loss.
    audio_shift = video[0].dts + 3600 - audio[0].pts
    if abs(audio_shift) > 2160 or audio[0].pts >= source_start - 2160:
        raise ValueError("Audio trimming would reach beyond the added silence")
    indices = [f.index for f in video]
    anchors = [f.arrival for f in video]
    if not all(b > a for a, b in zip(anchors, anchors[1:])):
        raise ValueError("Nonmonotonic picture arrival clock")

    def clock(index):
        i = max(0, min(bisect_right(indices, index) - 1, len(indices) - 2))
        return anchors[i] + round((index - indices[i]) * (anchors[i + 1] - anchors[i]) / (indices[i + 1] - indices[i]))

    v_events, a_events = iter(video_aux(video)), iter(audio_aux(audio, audio_shift))
    v_next, a_next = next(v_events, None), next(a_events, None)
    count = sit_count = clock_count = coalesced_clocks = 0
    previous = None
    max_rate = 0.0
    peak_interval = None
    if large_file:
        print("Writing HDV transport packets...", flush=True)
    with Path(dest).open("xb") as output:
        def emit(p):
            nonlocal count
            output.write(p)
            count += 1

        for n, original in packets(source):
            p = bytearray(original)
            stream = pid(p)
            if v_next is not None and v_next[0] == n:
                emit(v_next[1])
                v_next = next(v_events, None)
            if a_next is not None and a_next[0] == n:
                emit(a_next[1])
                a_next = next(a_events, None)
            if stream == VIDEO and p[3] & 0x20 and p[4] >= 7 and p[5] & 0x10:
                time = clock(n)
                delta = time - previous[1] if previous else None
                if delta is not None and delta <= 0:
                    raise ValueError("PCR clock runs backwards")
                # FFmpeg forces an extra PCR on keyframes, sometimes less than
                # one packet-time from its periodic PCR. Turning both into new
                # dedicated packets creates a spurious delivery-rate burst.
                # Coalescing short intervals gives the FireWire sender a useful
                # pacing window without changing media, AUX or clock values.
                if delta is None or delta >= MIN_PCR_INTERVAL:
                    if delta is not None:
                        if delta >= 2_700_000:
                            raise ValueError("PCR interval exceeds 100 ms")
                        rate = (count - previous[0]) * 188 * 8 * 27_000_000 / delta
                        if rate > max_rate:
                            max_rate = rate
                            peak_interval = (time, delta, count - previous[0])
                    q = bytearray(b"\xff" * PACKET)
                    q[:6] = bytes((0x47, PCR >> 8, PCR & 255, 0x20, 183, 0x10))
                    base, ext = divmod(time % ((1 << 33) * 300), 300)
                    q[6:12] = bytes((base >> 25 & 255, base >> 17 & 255, base >> 9 & 255,
                                    base >> 1 & 255, ((base & 1) << 7) | 0x7e | (ext >> 8), ext & 255))
                    previous = count, time
                    emit(q)
                    clock_count += 1
                else:
                    coalesced_clocks += 1
                p[5] &= ~0x10
                p[6:12] = b"\xff" * 6
                if not p[3] & 0x10:
                    # The original adaptation-only PCR packet has no media.
                    continue
            if stream == 0:
                emit(table(0, PAT, p[3] & 15))
                emit(table(0x1f, SIT, sit_count))
                sit_count += 1
                continue
            if stream == 0x81:
                emit(table(0x81, PMT, p[3] & 15))
                continue
            if stream in (0x11, 0x1fff):
                continue
            if (stream == VIDEO and n < video[0].index) or (stream == AUDIO and n < audio[0].index):
                continue
            if stream not in (VIDEO, AUDIO):
                raise ValueError(f"Unexpected encoder stream {stream:#x}")
            if p[1] & 0x40:
                at = payload_offset(p)
                p[at + 6] = 0x89 if stream == VIDEO else 0x81
                if stream == AUDIO:
                    write_timestamp(p, at + 9, timestamp(p[at + 9:at + 14]) + audio_shift)
            emit(p)
    if v_next is not None or a_next is not None or clock_count < 2 or max_rate > 33_000_000:
        details = (f" over {peak_interval[1] / 27000:.3f} ms / {peak_interval[2]} packets"
                   f" at stream clock {peak_interval[0] / 27_000_000:.3f} s") if peak_interval else ""
        raise ValueError(f"Incomplete or excessive-rate transport stream ({max_rate / 1e6:.3f} Mbit/s peak{details})")
    if large_file:
        print("Checking that compressed picture/audio bytes were preserved...", flush=True)
    expected = elementary_hashes(source, video[0].index, audio[0].index)
    if elementary_hashes(dest) != expected:
        raise ValueError("Compressed picture/audio payload changed during HDV adaptation")
    return {"coded_pictures": len(video), "audio_frames": len(audio),
            "removed_black_pictures": first_v, "removed_silent_audio_frames": first_a,
            "audio_timestamp_adjustment_ms": audio_shift / 90,
            "coalesced_pcr_markers": coalesced_clocks,
            "maximum_pcr_interval_bitrate_mbps": max_rate / 1e6,
            "elementary_sha256": expected}


def encode(source, dest, ffmpeg, ffprobe, normalize):
    probe = json.loads(subprocess.check_output([
        ffprobe, "-v", "error", "-select_streams", "a", "-show_entries", "stream=index", "-of", "json", str(source)
    ]))
    args = [ffmpeg, "-hide_banner", "-n", "-i", str(source)]
    audio_input = "0:a:0"
    if not probe["streams"]:
        args += ["-f", "lavfi", "-i", "anullsrc=channel_layout=stereo:sample_rate=48000"]
        audio_input = "1:a:0"
    vf = (
        "bwdif=mode=send_field:parity=auto:deint=interlaced,fps=50,"
        "scale=1440:1080:flags=lanczos:out_color_matrix=bt709,setsar=4/3,"
        "tinterlace=mode=interleave_top,setfield=tff,"
        f"tpad=start_duration={PREROLL}:stop_duration=0.48:color=black,"
        "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709"
    )
    af = "aresample=48000,aformat=channel_layouts=stereo,"
    if normalize:
        af += "loudnorm=I=-16:TP=-1.5:LRA=11,aresample=48000,"
    af += f"adelay={round(PREROLL * 1000)}:all=1,apad"
    args += [
        "-map", "0:v:0", "-map", audio_input, "-vf", vf, "-af", af, "-shortest",
        "-c:v", "mpeg2video", "-threads:v", "4", "-pix_fmt", "yuv420p",
        "-profile:v", "main", "-level:v", "6", "-b:v", "25M", "-minrate:v", "25M", "-maxrate:v", "25M",
        "-bufsize:v", "7340032", "-g", "12", "-bf", "2", "-sc_threshold", "1000000000",
        "-flags:v", "+ildct+ilme", "-top", "1", "-dc", "10", "-intra_vlc", "1",
        "-non_linear_quant", "1", "-qmax", "28", "-alternate_scan", "1",
        "-intra_matrix", ",".join(map(str, INTRA)), "-inter_matrix", ",".join(map(str, INTER)),
        "-seq_disp_ext", "1", "-video_format", "component",
        "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709",
        "-c:a", "mp2", "-b:a", "384k", "-ar", "48000", "-ac", "2",
        "-streamid", "0:2064", "-streamid", "1:2068", "-mpegts_service_id", "100",
        "-mpegts_transport_stream_id", "255", "-mpegts_pmt_start_pid", "0x81",
        "-pes_payload_size", "0", "-muxrate", "26700000", "-pcr_period", "40", "-f", "mpegts", str(dest),
    ]
    subprocess.run(args, check=True)


def encode_job(source, ffmpeg, normalize):
    stat = source.stat()
    return {"profile": ENCODER_PROFILE, "source": str(source),
            "source_bytes": stat.st_size, "source_mtime_ns": stat.st_mtime_ns,
            "ffmpeg": ffmpeg, "normalize_audio": normalize}


def save_checkpoint(path, data):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(data, indent=2), encoding="utf-8")
    temporary.replace(path)


def prepare(source, dest, ffmpeg, ffprobe, normalize, keep_intermediate=False):
    """Keep the expensive encode on authoring failure; resume only matching jobs."""
    if dest.exists():
        raise FileExistsError(f"Output already exists: {dest}")
    if not dest.parent.is_dir():
        raise FileNotFoundError(f"Output folder does not exist: {dest.parent}")
    work = dest.with_name(dest.name + ".hdv-work")
    if work.is_symlink() or getattr(work, "is_junction", lambda: False)():
        raise ValueError(f"Work folder must not be a link: {work}")
    work.mkdir(exist_ok=True)
    encoded, prepared = work / "encoded.m2t", work / "prepared.m2t"
    manifest, lock_path = work / "encode.json", work / "busy.lock"
    job = encode_job(source, ffmpeg, normalize)
    try:
        lock = lock_path.open("x", encoding="utf-8")
    except FileExistsError as error:
        raise ValueError(f"Work folder is already in use: {work}") from error
    completed_encode = False
    try:
        if manifest.exists():
            checkpoint = json.loads(manifest.read_text(encoding="utf-8"))
            if checkpoint.get("job") != job:
                raise ValueError(f"Saved encode belongs to a different input or settings; choose another output name: {work}")
            if checkpoint.get("state") not in ("encoding", "encoded"):
                raise ValueError(f"Unrecognized encoding checkpoint: {manifest}")
        else:
            if any(p.name != "busy.lock" for p in work.iterdir()):
                raise ValueError(f"Work folder contains unrecognized files: {work}")
            checkpoint = {"job": job, "state": "encoding"}
            save_checkpoint(manifest, checkpoint)
        if checkpoint["state"] == "encoded":
            stat = encoded.stat()
            if stat.st_size != checkpoint.get("encoded_bytes") or stat.st_mtime_ns != checkpoint.get("encoded_mtime_ns"):
                raise ValueError(f"Saved encode has changed; choose another output name: {encoded}")
            print(f"Reusing completed encode: {encoded}", flush=True)
        else:
            if encoded.exists():
                encoded.unlink()  # Only this job's incomplete encoder output.
            encode(source, encoded, ffmpeg, ffprobe, normalize)
            stat = encoded.stat()
            checkpoint.update(state="encoded", encoded_bytes=stat.st_size, encoded_mtime_ns=stat.st_mtime_ns)
            save_checkpoint(manifest, checkpoint)
        completed_encode = True
        if prepared.exists():
            prepared.unlink()  # A previous authoring attempt; rebuild from the saved encode.
        print("Authoring HDV transport and verifying compressed picture/audio payloads...", flush=True)
        report = adapt_transport(encoded, prepared)
        # Exclusive creation protects against a file appearing during encoding.
        created = False
        try:
            with dest.open("xb") as output:
                created = True
                with prepared.open("rb") as stream:
                    shutil.copyfileobj(stream, output, 1024 * 1024)
        except BaseException:
            if created:
                dest.unlink()
            raise
    except BaseException:
        if completed_encode:
            print(f"Completed encode kept in: {work}\nRe-run the same command to retry authoring without re-encoding.", flush=True)
        raise
    finally:
        lock.close()
        lock_path.unlink()
    if not keep_intermediate:
        # Remove only files this job owns; never recursively delete a work folder.
        for path in (prepared, encoded, manifest):
            try:
                path.unlink()
            except OSError:
                print(f"Output completed; could not remove intermediate: {path}", flush=True)
        try:
            work.rmdir()
        except OSError:
            print(f"Additional files remain in work folder: {work}", flush=True)
    else:
        print(f"Intermediate encode kept in: {work}", flush=True)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path, help="SDR 16:9 video; converted to 1080i50")
    parser.add_argument("output", type=Path, help="New .m2t file; existing files are never overwritten")
    parser.add_argument("--normalize-audio", action="store_true", help="Apply FFmpeg loudnorm at -16 LUFS / -1.5 dBTP")
    parser.add_argument("--ffmpeg", default="ffmpeg")
    parser.add_argument("--ffprobe", default="ffprobe")
    parser.add_argument("--keep-intermediate", action="store_true", help="Keep the completed encode after success, for further diagnostics")
    args = parser.parse_args()
    source, dest = args.input.resolve(strict=True), args.output.absolute()
    report = prepare(source, dest, args.ffmpeg, args.ffprobe, args.normalize_audio, args.keep_intermediate)
    print(f"\nPrepared HDV 1080i50: {dest}")
    print(json.dumps(report, indent=2))
    print("Added black/silent padding protects the original opening and ending. No deck was opened.")
    print("Sony HVR-M25AC monitor decoding verified; physical tape recording remains to be checked.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError, StopIteration) as error:
        raise SystemExit(f"HDV preparation failed: {error}") from error
