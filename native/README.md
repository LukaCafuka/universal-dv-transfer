# Native adapter development

`bridge.h` is the ABI contract. Session creation, polling, transport, writing, draining, and destruction run on one Rust source/transfer worker. Capture callbacks may execute on native streaming threads and must finish without blocking. The Rust context stays alive until native destruction has stopped all callbacks.

Commands are stop, play, pause, rewind, fast-forward, record, and record-pause (0–6). Opening an output stream alone does not issue a record command. Finish must drain scheduled output before the caller record-pauses the deck. Bus resets and lost devices end fixed-format transfers with an error; do not reuse a stale IEEE 1394 node ID.

Windows uses only the Windows SDK and standard C++; neither MFC nor the DirectShow sample baseclasses are required. Linux uses system development headers and dynamic libraries. macOS includes the unchanged Apple sample sources in `apple/`, retaining all their notices. SDKs without FireWire headers compile `unavailable.c` to keep file-based workflows usable.

Windows DV output supplies a complete static `DVINFO` connection block using
[Microsoft's DVCR/dvsd defaults](https://learn.microsoft.com/en-us/windows/win32/directshow/dvinfo-field-settings-in-the-msdv-driver).
Its AAUX/VAUX system bits match the source's PAL/NTSC frame size. An empty or
inconsistent block can pass `QueryAccept` but fail `ReceiveConnection` with
`E_INVALIDARG`, reported by `RenderStream` as `VFW_E_CANNOT_CONNECT`. These pin
defaults do not rewrite the actual audio or metadata packs in transmitted frames.
Output logs identify the source, connection and graph-start stages separately,
and include the allocator/receiver stage if the pin connection fails.

For a Windows hardware connection regression check, close tape applications and
compile `examples/windows_output_probe.cpp` in an x64 Visual Studio developer
terminal, from the project root:

```bat
cl /nologo /EHsc /std:c++17 /D_CRT_SECURE_NO_WARNINGS /Fe:target\windows-output-probe.exe /Fo:target\windows-output-probe.obj examples\windows_output_probe.cpp strmiids.lib ole32.lib oleaut32.lib uuid.lib user32.lib setupapi.lib cfgmgr32.lib
```

Run `target\windows-output-probe.exe "<exact-device-id>" pal` (or `ntsc`, matching
the deck's current recording system). It tries the old incomplete format, then
the production source via `RenderStream`, checks the allocator and verifies that
the graph remains stopped. It never runs/pauses the graph, delivers samples, or
sends tape transport commands. This checks connection setup only, not recording.

The optional `--commit` check also allocates and retrieves one sample, then
releases it without delivery. `--start` additionally verifies that the deck
reports STOP, requests graph startup, and stops the graph again without sending
samples or tape commands. A successful asynchronous `Run` result (`S_FALSE`)
means startup was accepted; it does not prove data transmission or recording.
Do not change the deck's transport state during these checks.

The probe also accepts `hdv` instead of `pal`/`ntsc`. `--self-test` runs an
offline HDV-format ownership/layout regression without opening any device.
Windows output inspects the driver's input pins and rejects a DV/HDV mismatch
before startup or transport control. HDV directly connects the 192-byte stride
source using a complete owned copy of the input's advertised format block and
GUID. The tested Sony MSTape input requires format
`{97E218B1-1E5A-498E-A954-F962CFD98CDE}` with a 16-byte block; using `FORMAT_None`
was rejected even though the packet layout matched. Only the generated
4-byte-header + 188-byte-payload layout and 256-packet sample size are accepted.

The macOS adapter and physical transfer behavior still require real hardware validation. The normal CI and file fixtures cannot establish adapter compatibility, tape timing, or driver behavior.

The open mode is 0 for DV NTSC, 1 for DV PAL, 2 for HDV, and 3 for automatic input. HDV capture callbacks provide arbitrary chunks of 188-byte TS or 192-byte source packets. HDV output accepts whole 188-byte packets and the measured TS bitrate, independent of video frame rate. Native implementations generate source packet headers; Rust preserves the transport payload. MPEG buffers must not be treated as DV frames.

Windows output reconstructs source-packet timing between adjacent PCR markers
using `hdv_output.h`, rather than extrapolating an entire tape from one bitrate
estimate. It buffers at most 32,768 TS packets, preserves all payload bytes and
packet order, follows PCR wrap, and maintains a continuous output clock across
explicit playlist discontinuities (including PCR PID changes). Short writes and
clip boundaries are coalesced; null packets pad only the final 256-packet driver
sample. Missing or invalid timing fails explicitly. The bitrate ABI parameter
remains available to the other platform adapters.

`examples/hdv_clock_test.cpp` is a standalone C++17 offline regression for this
clock. It covers variable packet spacing, PCR wrap and resets, repeated boundary
markers, PID changes, exact payload preservation, final padding, and missing
timing. Its optional arguments are a raw TS fixture, an independently timed
192-byte reference fixture, and a packet count. With the captured Sony fixture,
generated timestamps differ from exact rational PCR interpolation by at most two
24.576-MHz ticks (82 ns); that rounding error does not accumulate.

`examples/windows_hdv_send_probe.cpp` is the stopped-deck monitor check used for
live verification. Compile it with the same SDK libraries as the connection
probe above. Arguments are the exact device ID, TS file, bitrate, and block
count (1–2000 blocks of 256 packets). It exercises preview handover, checks STOP
before/throughout/after transmission, and never sends a transport command.
Its optional `--stride` mode sends an independently prepared 192-byte timing
fixture; normal mode uses production `udvt_write` and its PCR scheduler. Status
queries are read-only. The probe's delivery count establishes host completion;
a person must confirm the deck's input picture separately.

Auto callbacks prepend a one-byte tag: 1 is a complete DV sample, 2 a 188/192-byte TS sample, 3 a CIP packet including its eight-byte header, and 5 an unknown payload to preserve. Windows detects pin/sample types and exposes a stable ROM GUID or physical 1394 identity when possible. Linux rawiso and Apple's UniversalReceiver pass CIP for both formats without replacing a decoder at the boundary. The Rust source worker can close/reopen a session after a driver/reset error, resolving the same stable device identity; fixed-format workflows still stop. Auto input must never issue Record. Hardware gaps remain unverified and must be logged.
