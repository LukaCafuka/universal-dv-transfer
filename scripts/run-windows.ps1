param([switch]$Release)
$ErrorActionPreference='Stop'
Push-Location (Split-Path $PSScriptRoot -Parent)
try {
    $taskCargoArgs=@('run','--target','x86_64-pc-windows-msvc')
    if ($Release) {$taskCargoArgs+='--release'}
    & cargo @taskCargoArgs
    if ($LASTEXITCODE -ne 0) {throw "Cargo exited with $LASTEXITCODE"}
} finally {Pop-Location}
