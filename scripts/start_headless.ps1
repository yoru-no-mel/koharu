# Launches Koharu in headless mode: shared runtime, HTTP API, no window.
# Usage: scripts/start_headless.ps1 [-Port 9170] [-ExtraArgs @("--store", "D:\koharu\store")]
param(
    [string]$BindHost,
    [int]$Port,
    [string]$Store,
    [string]$Config,
    [string]$Profile = "release",
    [string[]]$ExtraArgs = @()
)

$ErrorActionPreference = "Stop"
$exe = Join-Path $PSScriptRoot "..\target\$Profile\koharu.exe"
if (-not (Test-Path $exe)) {
    throw "koharu executable not found at $exe; build it first (bun run build)"
}

$launchArgs = @("--headless")
if ($BindHost) { $launchArgs += @("--host", $BindHost) }
if ($Port) { $launchArgs += @("--port", $Port) }
if ($Store) { $launchArgs += @("--store", $Store) }
if ($Config) { $launchArgs += @("--config", $Config) }

& $exe @launchArgs @ExtraArgs
