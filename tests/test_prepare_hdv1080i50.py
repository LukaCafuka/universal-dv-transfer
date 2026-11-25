"""Optional end-to-end authoring regression. Run with Python, FFmpeg and FFprobe."""
from pathlib import Path
import hashlib
import importlib.util
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


def load_preparer(script):
    spec = importlib.util.spec_from_file_location("hdv_prepare_test", script)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


@unittest.skipUnless(shutil.which("ffmpeg") and shutil.which("ffprobe"), "FFmpeg/FFprobe required")
class HdvPreparationTest(unittest.TestCase):
    def test_source_edges_silent_audio_and_no_overwrite(self):
        script = Path(__file__).resolve().parents[1] / "scripts" / "prepare-hdv1080i50.py"
        base = script.parent.parent / "test-output"
        base.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="udvt-hdv-prepare-test-", dir=base) as folder:
            self.assertTrue(Path(folder).resolve().is_relative_to(base.resolve()))
            source = Path(folder) / "source.mp4"
            dest = Path(folder) / "prepared.m2t"
            subprocess.run([
                "ffmpeg", "-v", "error", "-n", "-f", "lavfi", "-i",
                "testsrc2=size=320x180:rate=25:duration=4",
                "-vf", "drawbox=x=0:y=0:w=iw:h=ih:color=green:t=fill:enable='lt(t,0.2)',"
                       "drawbox=x=0:y=0:w=iw:h=ih:color=blue:t=fill:enable='gte(t,3.8)'",
                "-c:v", "libx264", "-preset", "ultrafast", "-crf", "18", str(source),
            ], check=True, capture_output=True)
            original = hashlib.sha256(source.read_bytes()).hexdigest()
            prepared = subprocess.run([sys.executable, str(script), str(source), str(dest), "--keep-intermediate"], capture_output=True, text=True)
            self.assertEqual(prepared.returncode, 0, prepared.stderr)
            self.assertEqual(hashlib.sha256(source.read_bytes()).hexdigest(), original)
            probe = json.loads(subprocess.check_output([
                "ffprobe", "-v", "error", "-show_streams", "-show_programs", "-of", "json", str(dest),
            ]))
            v = next(s for s in probe["streams"] if s.get("codec_type") == "video")
            a = next(s for s in probe["streams"] if s.get("codec_type") == "audio")
            self.assertEqual((v["width"], v["height"], v["r_frame_rate"], v["field_order"]), (1440, 1080, "25/1", "tt"))
            self.assertEqual((v["color_primaries"], v["color_transfer"], v["color_space"]), ("bt709",) * 3)
            self.assertEqual((a["codec_name"], a["sample_rate"], a["channels"]), ("mp2", "48000", 2))
            self.assertEqual(probe["programs"][0]["pcr_pid"], 0x134)
            self.assertTrue({"0x811", "0x815"}.issubset({s["id"] for s in probe["streams"]}))

            decoded = subprocess.run([
                "ffmpeg", "-v", "error", "-i", str(dest), "-map", "0:v:0", "-vf", "scale=1:1,format=rgb24",
                "-fps_mode", "passthrough", "-f", "rawvideo", "-",
            ], capture_output=True, check=True)
            self.assertEqual(decoded.stderr, b"")
            frames = [tuple(decoded.stdout[i:i + 3]) for i in range(0, len(decoded.stdout), 3)]
            self.assertEqual(len(frames), 136)  # All 100 source frames + 24 leader + 12 tail.
            self.assertTrue(all(g > 90 and r < 40 and b < 40 for r, g, b in frames[24:29]))
            self.assertTrue(all(b > 200 and r < 40 and g < 40 for r, g, b in frames[119:124]))
            before = hashlib.sha256(dest.read_bytes()).hexdigest()

            # Force the real failure trigger: extra, valid adaptation-only PCR
            # packets immediately after a keyframe's own PCR. Media bytes and
            # continuity counters remain intact.
            module = load_preparer(script)
            work = dest.with_name(dest.name + ".hdv-work")
            encoded = work / "encoded.m2t"
            clustered, repaired = Path(folder) / "clustered.m2t", Path(folder) / "repaired.m2t"
            injected = False
            with clustered.open("xb") as output:
                for n, p in module.packets(encoded):
                    output.write(p)
                    if not injected and n > 8000 and module.pid(p) == module.VIDEO and p[1] & 0x40 and p[3] & 0x20 and p[4] >= 7 and p[5] & 0x10:
                        clock = ((p[6] << 25) | (p[7] << 17) | (p[8] << 9) | (p[9] << 1) | (p[10] >> 7)) * 300 + ((p[10] & 1) << 8) + p[11]
                        for i in range(1, 5):
                            extra = bytearray(b"\xff" * 188)
                            extra[:6] = bytes((0x47, module.VIDEO >> 8, module.VIDEO & 255, 0x20 | (p[3] & 15), 183, 0x10))
                            b, e = divmod(clock + i * 2000, 300)
                            extra[6:12] = bytes((b >> 25 & 255, b >> 17 & 255, b >> 9 & 255, b >> 1 & 255, ((b & 1) << 7) | 0x7e | (e >> 8), e & 255))
                            output.write(extra)
                        injected = True
            self.assertTrue(injected)
            report = module.adapt_transport(clustered, repaired)
            self.assertGreaterEqual(report["coalesced_pcr_markers"], 4)
            self.assertLessEqual(report["maximum_pcr_interval_bitrate_mbps"], 33)
            self.assertEqual(module.elementary_hashes(repaired), module.elementary_hashes(dest))
            clocks = []
            for n, p in module.packets(repaired):
                if module.pid(p) == module.PCR:
                    t = ((p[6] << 25) | (p[7] << 17) | (p[8] << 9) | (p[9] << 1) | (p[10] >> 7)) * 300 + ((p[10] & 1) << 8) + p[11]
                    clocks.append((n, t))
            for a, b in zip(clocks, clocks[1:]):
                self.assertGreaterEqual(b[1] - a[1], 20 * 27000)
                self.assertLess(b[1] - a[1], 100 * 27000)
                self.assertLessEqual((b[0] - a[0]) * 188 * 8 * 27_000_000 / (b[1] - a[1]), 33_000_000)

            # A failed authoring pass must preserve the expensive encode. A
            # matching retry must not call FFmpeg again and must produce the
            # same output. Different normalization settings must not reuse it.
            encoded_hash = hashlib.sha256(encoded.read_bytes()).hexdigest()
            dest.unlink()
            with patch.object(module, "encode", side_effect=AssertionError("Unexpected re-encode")):
                with patch.object(module, "adapt_transport", side_effect=ValueError("Simulated authoring failure")):
                    with self.assertRaisesRegex(ValueError, "Simulated authoring failure"):
                        module.prepare(source.resolve(), dest, "ffmpeg", "ffprobe", False)
                self.assertFalse(dest.exists())
                self.assertEqual(hashlib.sha256(encoded.read_bytes()).hexdigest(), encoded_hash)
                self.assertFalse((work / "busy.lock").exists())
                with self.assertRaisesRegex(ValueError, "different input or settings"):
                    module.prepare(source.resolve(), dest, "ffmpeg", "ffprobe", True)
                module.prepare(source.resolve(), dest, "ffmpeg", "ffprobe", False)
            self.assertEqual(hashlib.sha256(dest.read_bytes()).hexdigest(), before)
            self.assertFalse(work.exists())
            repeated = subprocess.run([sys.executable, str(script), str(source), str(dest)], capture_output=True, text=True)
            self.assertNotEqual(repeated.returncode, 0)
            self.assertIn("Output already exists", repeated.stderr)
            self.assertEqual(hashlib.sha256(dest.read_bytes()).hexdigest(), before)


if __name__ == "__main__":
    unittest.main()
