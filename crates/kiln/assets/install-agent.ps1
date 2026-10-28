# Install a kiln agent on this Windows machine, fetched from the kiln server that served this
# script. Never asks for a password: installs into %LOCALAPPDATA%\Programs\kiln, puts that on your
# PATH, and starts kiln at boot through a scheduled task that runs as you (`kiln setup`, which says
# what it will do and asks first in this window). Windows asks for administrator rights once for
# that (an administrator PowerShell has them); without them, or with nobody at the keyboard, the
# task starts kiln when you sign in instead.
#
#   & ([scriptblock]::Create((irm @@KILN_SERVER@@/install.ps1))) -Token <join-token> [-Labels windows] [-Name box1] [-Slots 4]
#   $env:KILN_JOIN_TOKEN = '<join-token>'; irm @@KILN_SERVER@@/install.ps1 | iex
#
# Without a token (or with -Join) it asks the server to take this machine instead (`kiln join`):
# it prints a verification code, an admin approves the machine in the UI (Runners -> Add
# machines) after comparing it, and the agent is installed the same way.
#
#   irm @@KILN_SERVER@@/install.ps1 | iex
#   & ([scriptblock]::Create((irm @@KILN_SERVER@@/install.ps1))) -Join [-Labels windows] [-Name box1]
#
# With -Outpost, nothing is installed: kiln, its certificate and its files go in
# %LOCALAPPDATA%\kiln-outpost, nothing starts at logon, and it runs in this window as an outpost
# until you stop it (Ctrl-C). Its terminals, AI sessions and agents show up in kiln; it takes no
# jobs. With a one-time invite (Machines -> Add an outpost) it joins at once; without one it asks
# to join, as -Join does.
#
#   & ([scriptblock]::Create((irm @@KILN_SERVER@@/install.ps1))) -Outpost [-Token <invite>] [-Name box1]
param(
  [string]$Token = $env:KILN_JOIN_TOKEN,
  [string]$Server = $(if ($env:KILN_SERVER) { $env:KILN_SERVER } else { '@@KILN_SERVER@@' }),
  [string]$Labels = $env:KILN_LABELS,
  [string]$Name = $env:KILN_AGENT_NAME,
  [string]$Slots = $env:KILN_SLOTS,
  [string]$Quic = $env:KILN_QUIC,
  [switch]$Join = ($env:KILN_JOIN -eq '1'),
  [switch]$Outpost = ($env:KILN_OUTPOST -eq '1')
)
$ErrorActionPreference = 'Stop'
# Invoke-WebRequest's progress bar makes downloads very slow in Windows PowerShell 5.1.
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

# The machine's CPU, not this PowerShell's (an x64 PowerShell can run emulated on ARM64).
$arch = $null
try { $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() } catch { $arch = $null }
if (-not $arch) { $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE } }
switch -Regex ($arch) {
  '^(X64|AMD64)$' { $cpu = 'x86_64' }
  '^(Arm64|ARM64)$' { $cpu = 'aarch64' }
  default { throw "kiln: unsupported CPU architecture '$arch'" }
}
$target = "$cpu-pc-windows-msvc"

# An outpost keeps kiln, its certificate and its files in a folder of its own.
$dir = if ($Outpost) { Join-Path $env:LOCALAPPDATA 'kiln-outpost' } else { Join-Path $env:LOCALAPPDATA 'Programs\kiln' }
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$exe = Join-Path $dir 'kiln.exe'
$tmp = Join-Path $dir (".kiln.{0}.exe" -f $PID)
try {
  # The server sends its own binary when it was built for this target, else redirects to the
  # matching release build.
  Invoke-WebRequest -UseBasicParsing -Uri "$Server/dl/kiln/$target" -OutFile $tmp
  Get-ChildItem -Path $dir -Filter 'kiln.exe.old-*' -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
  if (Test-Path $exe) {
    # A running kiln.exe cannot be overwritten, but it can be renamed out of the way.
    Move-Item -Force -Path $exe -Destination (Join-Path $dir ("kiln.exe.old-{0}" -f $PID))
  }
  Move-Item -Force -Path $tmp -Destination $exe
} finally {
  if (Test-Path $tmp) { Remove-Item -Force $tmp }
}
if ($Outpost) {
  Write-Host "kiln ($target) is in $dir, not installed: it runs here as an outpost until you stop it (Ctrl-C)"
  $outpostArgs = @('outpost', '--server', $Server, '--state-dir', $dir)
  if ($Token) { $outpostArgs += @("--token=$Token") }
  if ($Name) { $outpostArgs += @('--name', $Name) }
  & $exe @outpostArgs
  return
}
Write-Host "kiln ($target) installed to $exe"
# `kiln install` puts $dir on your PATH for new terminals; this one finds kiln now too.
if (-not (($env:Path -split ';') -contains $dir)) { $env:Path = "$dir;$env:Path" }

# `kiln setup` takes it from here: in this window it says what it will do and asks first; with
# nobody here it asks nothing. Without a token (or with -Join) it asks the server to take this
# machine. A kiln from before `kiln setup` goes on below as it always did. (Windows PowerShell stops
# on a native command's error output while ErrorActionPreference is Stop: not for this question.)
$hasSetup = $false
try {
  $ErrorActionPreference = 'Continue'
  & $exe setup --help *> $null
  $hasSetup = ($LASTEXITCODE -eq 0)
} catch {
  $hasSetup = $false
} finally {
  $ErrorActionPreference = 'Stop'
}
if ($hasSetup) {
  $setupArgs = @('setup', '--server', $Server)
  if ($Token -and -not $Join) { $setupArgs += @("--token=$Token") }
  if ($Labels) { $setupArgs += @('--labels', $Labels) }
  if ($Name) { $setupArgs += @('--name', $Name) }
  if ($Slots) { $setupArgs += @('--slots', $Slots) }
  if ($Quic) { $setupArgs += @('--quic', $Quic) }
  if (-not [Environment]::UserInteractive -or [Console]::IsInputRedirected) { $setupArgs += @('--unattended') }
  & $exe @setupArgs
  if ($LASTEXITCODE -ne 0) { throw "kiln setup failed (exit code $LASTEXITCODE)" }
  return
}

if ($Join -or -not $Token) {
  # No token: ask to join; it waits for an admin to approve the code it prints, then installs.
  $joinArgs = @('join', '--server', $Server)
  if ($Labels) { $joinArgs += @('--labels', $Labels) }
  if ($Name) { $joinArgs += @('--name', $Name) }
  if ($Slots) { $joinArgs += @('--slots', $Slots) }
  & $exe @joinArgs
  if ($LASTEXITCODE -ne 0) { throw "kiln join failed (exit code $LASTEXITCODE)" }
  return
}

# Not --unattended: at a terminal, kiln asks Windows for the rights to start at boot.
$installArgs = @('install', '--agent', '--server', $Server)
if ($Token) { $installArgs += @("--token=$Token") }
if ($Labels) { $installArgs += @('--labels', $Labels) }
if ($Name) { $installArgs += @('--name', $Name) }
if ($Slots) { $installArgs += @('--slots', $Slots) }
if ($Quic) { $installArgs += @('--quic', $Quic) }
& $exe @installArgs
if ($LASTEXITCODE -ne 0) { throw "kiln install failed (exit code $LASTEXITCODE)" }
