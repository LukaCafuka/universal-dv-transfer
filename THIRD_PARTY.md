# Third-party notices

## Reference implementations

The initial design was informed by Petr Mourek's WinDV (2002–2003), Hunter Figgs' preservation of its source, and Makoto's WinDV2 (2026). Their MIT notices are retained in `licenses/WinDV-MIT.txt` and `licenses/WinDV2-MIT.txt`. Native interfaces and the behavior mapping follow those projects; the older project folders are not build dependencies.

DVrescue (MIPoPS / MediaArea) was reviewed for Linux FireWire and macOS transport behavior. Its source was not copied into this application.

## Apple AVCVideoServices

`native/apple/` contains Apple's sample-code-licensed AVCVideoServices sources from FireWire SDK 26, obtained from the preserved copy in [xingrz/tapecap](https://github.com/xingrz/tapecap/tree/826b9a2a1c1982222114de2e52fc0f6652f2a222/third_party/AVCVideoServices), revision `826b9a2a1c1982222114de2e52fc0f6652f2a222`, downloaded 2026-10-16. Every source retains its full Apple copyright, license grant, and disclaimer. `native/apple/NOTICE.md` records the SDK's archive provenance and upstream compatibility edits. These files remain under their original Apple license, separate from the application's MIT license. The new adapter is `native/macos.cpp`; the downloaded SDK sources were not changed.

## Other libraries

Rust dependencies and exact versions are recorded in Cargo.lock; each retains its upstream license. Linux FireWire libraries are dynamically linked and must be supplied by the operating system. FFmpeg is an external user-installed executable, not distributed in this repository. Its licensing depends on the user's build. Consult the respective upstream license terms when packaging a release.
