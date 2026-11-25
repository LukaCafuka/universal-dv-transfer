# Windows DV/HDV switching research — 2026-10-18

A software restart of the Sony deck's AV/C unit successfully replaced unplugging
FireWire in both DV-to-HDV and HDV-to-DV tests on this machine. Windows changed
its driver binding and reception resumed in each format. Automatic app
integration and continuous, lossless capture across the transition are separate
requirements.

## Evidence

The HVR-M25A family manual documents automatic DV/HDV output with `HDV/DV SEL =
AUTO`. It also describes temporary video/audio interruption during a format
change and warns about changing that menu with the cable connected. Its computer
transfer notes identify mixed-format material as a possible interruption source.
This is family documentation; the connected unit identifies itself as M25AC.
[Sony operating instructions, pages 59 and 70](https://pro.sony/s3/cms-static-content/operation-manual/3878928131.pdf).

Microsoft's AV/C enumerator generates subunit identifiers used to choose the
driver. DV discovery adds `&DV` to the identifiers; the tape examples show how
MSDV/MSTape selection follows these IDs. Reopening a DirectShow graph does not
recreate its underlying PnP device.
[Peer subunit identifiers](https://learn.microsoft.com/en-us/windows-hardware/drivers/stream/peer-subunit-device-identifiers),
[AV/C enumeration](https://learn.microsoft.com/en-us/windows-hardware/drivers/stream/av-c-device-identifiers).

Read-only inspection on this machine found this exact hierarchy:

```text
1394\Sony&HDV-VCR\0800460103F17EF1              service 61883
  61883\Sony&HVR-M25AC&10001\F17EF1031460008   service Avc
    AVC\Sony&HDV-VCR&TYP_4&ID_0&DV\...        service MSDV, present
    AVC\Sony&HDV-VCR&TYP_4&ID_0\...           service MSTAPE, disconnected
```

This corroborates the earlier live tests: the same physical GUID can have
different DV/HDV child nodes, with only one present. The AV/C unit above those
children was the software-restart target. The successful test below confirms
that restarting it can change the format-dependent child. Whether restarting
only the MSDV/MSTape child would suffice remains untested.

## Practical implementation route

1. Check the deck's output-signal status alongside the offered capture-pin types.
   `IAMExtTransport::GetTransportBasicParameters(ED_TRANSBASIC_OUTPUT_SIGNAL)` is
   documented for DV/MPEG devices. Test whether this deck reports the current
   wire format or cached information before using it to trigger recovery.
   An unknown result must not cause repeated restarts.
   [Microsoft signal-status API](https://learn.microsoft.com/en-us/windows/win32/api/strmif/nf-strmif-iamexttransport-gettransportbasicparameters).
2. On a confirmed mismatch or persistent stalled input, drain received data,
   finalize the segment, close the receiver and log the uncertain boundary.
3. Through an elevated helper, request a restart of the selected AV/C unit.
   Windows supports `DIF_PROPERTYCHANGE` for device restart; `pnputil
   /restart-device <exact instance ID>` provides a simple prototype.
   [Device property changes](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/dif-propertychange),
   [PnPUtil syntax](https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/pnputil-command-syntax).
4. Wait for a matching child under the original physical GUID, rebuild the graph,
   reset the preview decoder and resume in a new segment. A successful restart
   request is insufficient: require actual incoming data in the expected format.

The existing Auto source worker already handles graph closure, the stable GUID,
stream draining, retry and preview reset. The new pieces would be reliable
mismatch detection and the privileged, device-specific PnP operation. Use a
bounded attempt count and cooldown. An application holding the device can veto
the operation; the helper must return that error rather than force removal.

Production integration should obtain Windows administrator approval for a helper
session scoped to the selected physical GUID, while keeping the GUI unelevated.
An authenticated local connection and fixed restart operation would avoid a UAC
prompt on every transition. The helper should terminate with that app session.
This is a proposed design, not an implemented service. Missing samples alone
must not be treated as proof of a format mismatch: blank tape and playback
pause also need to be distinguished from a stale endpoint.

This route is feasible to implement without installing a custom driver, and
recovery in both directions was verified on this deck. Other hardware and long
unattended mixed-tape captures still require validation. Even a successful software reconnect
introduces a receive gap. Capturing that missing tape interval
would require a separate, explicitly enabled pause/rewind/overlap workflow and
boundary verification; the current app does not do that.

## Diagnostic helper and live result

`scripts/probe-windows-reconnect.ps1` is read-only by default. It resolves the
exact physical deck, checks its 61883/AVC ancestry, prints current endpoint
services and shows the proposed command. It never targets the PCI FireWire host
controller. `-Apply` is an explicit experimental restart, requires Administrator
rights and refuses to run while known tape applications are open. It does not
force a vetoed operation, remove devices, change driver packages or reboot.

```powershell
# Inspect the proposed target; no restart:
.\scripts\probe-windows-reconnect.ps1 -DeviceId 'firewire:1394\Sony&HDV-VCR\0800460103F17EF1'
# Preview the opted-in operation; still no restart:
.\scripts\probe-windows-reconnect.ps1 -DeviceId 'firewire:1394\Sony&HDV-VCR\0800460103F17EF1' -Apply -WhatIf
```

For the actual experiment, close tape apps, establish a known stale binding by
switching the playing tape format without unplugging, then run the same command
with `-Apply` from Administrator PowerShell. Verify the endpoint changed and that
`monitor_probe` receives the new stream. Repeat both directions. Do not interpret
an unchanged binding while playing the same format as a failed test. Automatic
resets are not enabled in the app.
The script passed PowerShell syntax validation, real-device read-only inspection
and `-Apply -WhatIf` on 2026-10-18. The selected node and MSDV binding agreed with
an independent PnPUtil device-tree inspection. These checks establish target
selection and the dry-run path only.

The subsequent authorized live test on 2026-10-18 established:

- Before restart: the deck was playing HDV, while Windows exposed the `&DV`
  subunit under MSDV. Tape applications were closed.
- The ordinary helper correctly refused to mutate device state without an
  Administrator token. After local Windows UAC approval, the elevated helper
  restarted only `61883\Sony&HVR-M25AC&10001\F17EF1031460008` successfully.
- After restart: the `&DV` node was disconnected and the HDV node was present
  under MSTAPE. The physical cable remained connected and no driver was installed.
- A separate 15-second read-only monitor received 369 HDV pictures, produced
  approximately 49-50 preview images/s after startup and reported zero receive
  drops. It wrote no capture files and sent no tape transport commands.

Evidence is retained in `target/windows-reconnect-live.log`,
`target/windows-reconnect-live-result.json`, and
`target/windows-reconnect-hdv-preview.log`.

The reverse test was also completed on 2026-10-18. With DV playing, the cable
still connected and the stale MSTAPE endpoint present, the same AV/C-unit restart
changed the active child back to MSDV. The subsequent 15-second read-only probe
received 354 DV frames, with approximately 43-50 displayed images/s after startup
and zero reported receive drops. Evidence is retained in
`target/windows-reconnect-hdv-to-dv-live.log`,
`target/windows-reconnect-hdv-to-dv-result.json`, and
`target/windows-reconnect-dv-preview.log`.

These tests demonstrate software reconnect in both directions. The zero-drop
counters describe the subsequent receive intervals; they do not establish
continuity across either restart or a lossless tape boundary. Both helpers
completed successfully, both read-only probes exited successfully, and the deck
was left on the DV endpoint with no tape application open. The tests created no
recordings and did not send tape transport or record commands. Production
automatic integration remains unimplemented.

## Other approaches and limits

`IOCTL_AVC_BUS_RESET` is not a documented generic replacement for unplugging a
physical deck: it completes virtual-subunit changes and may do nothing when
those child relations have not changed. Its name alone is insufficient reason
to use it here.
[Microsoft IOCTL description](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/avc/ni-avc-ioctl_avc_bus_reset).

A universal raw FireWire receiver could avoid format-specific capture drivers,
as the Linux/macOS adapters are designed to do. The documented Windows
`IOCTL_AVCSTRM_CLASS` streaming interface is driver-facing, rather than a raw
user-mode capture API. A Windows bridge would be a substantial driver/backend
project unless a suitable existing bridge is found; this research has not
established such a bridge. It would still not recover data omitted by the deck.
[AV/C streaming interface](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/avcstrm/ni-avcstrm-ioctl_avcstrm_class).
