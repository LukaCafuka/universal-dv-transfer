<#
.SYNOPSIS
Inspect a selected deck's AV/C driver and optionally test a software reconnect.
.DESCRIPTION
Read-only by default. -Apply restarts only the AV/C unit below the exact physical
1394 device ID. It does not restart the FireWire host controller. Close capture
applications first. This is an experimental diagnostic, not automatic recovery.
.EXAMPLE
.\scripts\probe-windows-reconnect.ps1 -DeviceId 'firewire:1394\Sony&HDV-VCR\0800460103F17EF1'
.EXAMPLE
.\scripts\probe-windows-reconnect.ps1 -DeviceId 'firewire:1394\Sony&HDV-VCR\0800460103F17EF1' -Apply -WhatIf
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('(?i)^firewire:1394\\[^\\*?]+\\[0-9a-f]{16}$')]
    [string] $DeviceId,
    [switch] $Apply
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$physicalId = $DeviceId.Substring('firewire:'.Length)

function Get-DeviceValue([string] $InstanceId, [string] $Key) {
    (Get-PnpDeviceProperty -InstanceId $InstanceId -KeyName $Key -ErrorAction Stop).Data
}

function Get-TapeEndpoints([string] $AvcId) {
    foreach ($device in @(Get-PnpDevice -PresentOnly -ErrorAction Stop)) {
        if (-not $device.InstanceId.StartsWith('AVC\', [StringComparison]::OrdinalIgnoreCase)) {
            continue
        }
        if ((Get-DeviceValue $device.InstanceId 'DEVPKEY_Device_Parent') -ne $AvcId) {
            continue
        }
        $service = Get-DeviceValue $device.InstanceId 'DEVPKEY_Device_Service'
        if ($service -in @('MSDV', 'MSTAPE')) {
            [pscustomobject]@{
                InstanceId = $device.InstanceId
                Name = $device.FriendlyName
                Service = $service
                Status = $device.Status
            }
        }
    }
}

$physical = Get-PnpDevice -PresentOnly -InstanceId $physicalId -ErrorAction Stop
if ((Get-DeviceValue $physical.InstanceId 'DEVPKEY_Device_Service') -ne '61883') {
    throw 'The selected physical node is not an IEC 61883 device. No restart performed.'
}
$units = @(
    foreach ($candidate in @(Get-PnpDevice -PresentOnly -Class AVC -ErrorAction Stop)) {
        if ((Get-DeviceValue $candidate.InstanceId 'DEVPKEY_Device_Parent') -eq $physical.InstanceId) {
            if ((Get-DeviceValue $candidate.InstanceId 'DEVPKEY_Device_Service') -eq 'AVC') {
                $candidate
            }
        }
    }
)
if ($units.Count -ne 1) {
    throw "Expected exactly one AV/C unit below the selected deck; found $($units.Count). No restart performed."
}
$avcId = $units[0].InstanceId
$before = @(Get-TapeEndpoints $avcId)
if ($before.Count -eq 0) {
    throw 'The selected AV/C unit has no present MSDV/MSTape endpoint. No restart performed.'
}

Write-Output "Physical deck: $($physical.InstanceId)"
Write-Output "AV/C restart target: $avcId"
Write-Output 'Current tape endpoint(s):'
$before | Format-List
Write-Output "Planned command: pnputil /restart-device `"$avcId`""
if (-not $Apply) {
    Write-Output 'Inspection only. No device was restarted. -Apply opts into the experimental restart.'
    return
}
if (-not $PSCmdlet.ShouldProcess($avcId, 'Restart the selected deck AV/C unit; interrupts its FireWire stream')) {
    return
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run the restart test from an Administrator PowerShell. No restart performed.'
}
# These are common clients, not a complete list. PnP/driver vetoes still apply.
$clients = @(Get-Process -ErrorAction Stop | Where-Object {
    $_.ProcessName -in @('universal-dv-transfer', 'monitor_probe', 'probe_auto_input', 'WinDV', 'WinDV.UI', 'WinDV2', 'HDVSplit', 'dvgrab')
})
if ($clients.Count -gt 0) {
    throw "Close tape applications first: $(($clients.ProcessName | Sort-Object -Unique) -join ', '). No restart performed."
}

$pnpUtil = Join-Path ([Environment]::GetFolderPath('System')) 'pnputil.exe'
& $pnpUtil /restart-device $avcId
$restartExit = $LASTEXITCODE
if ($restartExit -eq 3010) {
    throw 'Windows reported that a reboot is required. No automatic reboot or stronger reset will be attempted.'
}
if ($restartExit -ne 0) {
    throw "PnP restart failed with exit code $restartExit. No broader reset will be attempted."
}

$deadline = (Get-Date).AddSeconds(10)
$after = @()
do {
    Start-Sleep -Milliseconds 500
    try {
        $after = @(Get-TapeEndpoints $avcId)
    } catch {
        Write-Verbose "Waiting for PnP: $_"
    }
} while ($after.Count -eq 0 -and (Get-Date) -lt $deadline)
if ($after.Count -eq 0) {
    throw 'The restarted unit did not expose a tape endpoint within ten seconds. Physical reconnection may still be necessary.'
}
Write-Output 'Endpoint(s) after restart:'
$after | Format-List
Write-Output 'PnP restart completed. Confirm the expected driver AND incoming video; command success alone does not establish format switching.'
