<#
.SYNOPSIS
  Installs or removes the PeetConnect host as a Windows service (LocalSystem, auto-start).

.DESCRIPTION
  The service runs unattended and launches a worker into the active console session so it can
  capture the screen and inject input, including across the secure desktop. Installing a service
  needs administrator rights; this script relaunches itself elevated if necessary.

  The allow rules you pass here are baked into the service's command line, so the service always
  starts with them. Change them by reinstalling.

.EXAMPLE
  ./install-service.ps1 -AllowUser you@example.com

.EXAMPLE
  ./install-service.ps1 -AllowTag tag:admin -Pin 4821

.EXAMPLE
  ./install-service.ps1 -Uninstall
#>
[CmdletBinding()]
param(
  [string]   $AllowUser,
  [string[]] $AllowTag,
  [string[]] $AllowNode,
  [switch]   $AllowAnyTailnet,
  [string]   $Pin,
  [int]      $Port,
  [string]   $Capture,
  [string]   $ExePath,
  [switch]   $Uninstall,
  [switch]   $NoStart
)

$ErrorActionPreference = 'Stop'

# Locate peet-host.exe: explicit path, a release build, or a debug build next to the repo.
if (-not $ExePath) {
  $root = Split-Path -Parent $PSScriptRoot
  $candidates = @(
    (Join-Path $root 'target/release/peet-host.exe'),
    (Join-Path $root 'target/debug/peet-host.exe')
  )
  $ExePath = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $ExePath -or -not (Test-Path $ExePath)) {
  throw "peet-host.exe not found. Build it with 'cargo build --release -p peet-host' or pass -ExePath."
}
$ExePath = (Resolve-Path $ExePath).Path

# Build the argument list for the host.
$hostArgs = [System.Collections.Generic.List[string]]::new()
if ($Uninstall) {
  $hostArgs.Add('--uninstall')
} else {
  $hostArgs.Add('--install')
  if ($AllowUser)       { $hostArgs.Add('--allow-user'); $hostArgs.Add($AllowUser) }
  foreach ($t in $AllowTag)  { $hostArgs.Add('--allow-tag');  $hostArgs.Add($t) }
  foreach ($n in $AllowNode) { $hostArgs.Add('--allow-node'); $hostArgs.Add($n) }
  if ($AllowAnyTailnet) { $hostArgs.Add('--allow-any-tailnet') }
  if ($Pin)             { $hostArgs.Add('--pin');     $hostArgs.Add($Pin) }
  if ($Port)            { $hostArgs.Add('--port');    $hostArgs.Add("$Port") }
  if ($Capture)         { $hostArgs.Add('--capture'); $hostArgs.Add($Capture) }
}

function Test-Admin {
  $id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
  (New-Object System.Security.Principal.WindowsPrincipal($id)).IsInRole(
    [System.Security.Principal.WindowsBuiltInRole]::Administrator)
}

# Relaunch elevated if needed, re-invoking this script with the same parameters.
if (-not (Test-Admin)) {
  Write-Host 'Requesting administrator rights...'
  $self = $MyInvocation.MyCommand.Path
  $boundArgs = @("-ExecutionPolicy","Bypass","-File",$self) + $args + $PSBoundParameters.GetEnumerator().ForEach({
    if ($_.Value -is [switch]) { if ($_.Value.IsPresent) { "-$($_.Key)" } }
    elseif ($_.Value -is [array]) { foreach ($v in $_.Value) { "-$($_.Key)"; "$v" } }
    else { "-$($_.Key)"; "$($_.Value)" }
  })
  Start-Process -FilePath 'powershell.exe' -Verb RunAs -ArgumentList $boundArgs -Wait
  return
}

Write-Host "Running: `"$ExePath`" $($hostArgs -join ' ')"
& $ExePath @hostArgs
if ($LASTEXITCODE -ne 0) { throw "peet-host exited with code $LASTEXITCODE" }

if (-not $Uninstall) {
  Write-Host 'Service installed.'
  if (-not $NoStart) {
    Write-Host 'Starting service...'
    sc.exe start PeetConnect | Out-Host
  } else {
    Write-Host 'Start it with: sc start PeetConnect'
  }
} else {
  Write-Host 'Service removed.'
}
