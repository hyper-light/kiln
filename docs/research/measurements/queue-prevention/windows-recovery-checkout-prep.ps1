param([switch]$Apply)
$ErrorActionPreference = 'Stop'
$src = 'C:\Users\corph\.kiln\reach\e30189ea29182644fe5a4b2e06c7dac2\kiln-offline-fix-20261110-8a5ae447ef'
$cache = 'C:\Users\corph\.cache\kiln-offline-release-20261111'
$expectedHead = 'f555736bd887a4196c8841564cb9b4d6c1ede95f'
$paths = @('deploy/helm/kiln/dashboards', 'deploy/helm/kiln/rules', 'docs/research/measurements/state-view/POLICY_READ.json', 'docs/research/measurements/state-view/ROOT.md')
$expectedStatus = @(' D deploy/helm/kiln/dashboards', ' D deploy/helm/kiln/rules', ' M docs/research/measurements/state-view/POLICY_READ.json', ' M docs/research/measurements/state-view/ROOT.md')
$prefix = @('--no-optional-locks', '-C', $src, '-c', 'core.fsmonitor=false')
$evidence = $null
$stage = 'preflight'
$stash = $null
$utf8 = [Text.UTF8Encoding]::new($false)
function Git-Lines([string[]]$arguments, [switch]$MissingOkay) {
  $quoted = @((@($prefix) + @($arguments)) | ForEach-Object { if ($_ -match '["\r\n]') { throw 'invalid fixed Git argument' }; '"' + $_ + '"' })
  $start = [Diagnostics.ProcessStartInfo]::new()
  $start.FileName = 'git.exe'
  $start.Arguments = $quoted -join ' '
  $start.UseShellExecute = $false
  $start.CreateNoWindow = $true
  $start.RedirectStandardOutput = $true
  $start.RedirectStandardError = $true
  $process = [Diagnostics.Process]::new()
  $process.StartInfo = $start
  try {
    if (-not $process.Start()) { throw 'native Git did not start' }
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    $process.WaitForExit()
    $output = $stdout.GetAwaiter().GetResult()
    $warnings = $stderr.GetAwaiter().GetResult()
    $code = $process.ExitCode
    if ($code -ne 0 -and -not ($MissingOkay -and $code -eq 1)) { throw ('native Git command refused with exit ' + $code) }
    $result = @([regex]::Split($output, '\r?\n') | Where-Object { $_.Length -ne 0 })
    return [pscustomobject]@{ lines = $result; exit_code = $code; stderr_present = $warnings.Length -ne 0 }
  } finally { $process.Dispose() }
}
function Head {
  $r = Git-Lines @('rev-parse', 'HEAD')
  if ($r.lines.Count -ne 1 -or $r.lines[0] -cne $expectedHead) { throw 'HEAD is not exact completed f555 source' }
  return $r.lines[0]
}
function Status([switch]$Normalized) {
  $a = @('status', '--porcelain=v1', '--untracked-files=all')
  if ($Normalized) { $a = @('-c', 'core.filemode=false') + $a }
  return (Git-Lines $a).lines
}
function Exact-Status {
  $s = @(Status -Normalized)
  if ($s.Count -ne $expectedStatus.Count) { throw 'unexpected content change count' }
  foreach ($line in $expectedStatus) { if ($s -cnotcontains $line) { throw 'unexpected content paths or staged changes' } }
  return $s
}
function File-Hash([string]$path) { return (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
function New-Bytes([string]$path, [byte[]]$bytes) {
  $f = [IO.File]::Open($path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
  try { $f.Write($bytes, 0, $bytes.Length); $f.Flush($true) } finally { $f.Dispose() }
}
function New-Json([string]$path, $value) { New-Bytes $path ($utf8.GetBytes(($value | ConvertTo-Json -Depth 8 -Compress))) }
function Copy-Exact([string]$source, [string]$destination) {
  $inputFile = [IO.File]::Open($source, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
  try {
    $outputFile = [IO.File]::Open($destination, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $inputFile.CopyTo($outputFile); $outputFile.Flush($true) } finally { $outputFile.Dispose() }
  } finally { $inputFile.Dispose() }
  if ((File-Hash $source) -cne (File-Hash $destination)) { throw 'document preservation hash mismatch' }
}
function Raw-Diff([string]$destination, [string]$right) {
  # Every argument is fixed here or a validated object ID; none contains a quote.
  $a = $prefix + @('-c', 'core.filemode=false', 'diff', '--binary', '--full-index', '--no-ext-diff', '--no-textconv', '--no-renames', $expectedHead)
  if ($right) { $a += $right }
  $a += @('--') + $paths
  $quoted = @($a | ForEach-Object { if ($_ -match '["\r\n]') { throw 'invalid fixed Git argument' }; '"' + $_ + '"' })
  $start = [Diagnostics.ProcessStartInfo]::new()
  $start.FileName = 'git.exe'
  $start.Arguments = $quoted -join ' '
  $start.UseShellExecute = $false
  $start.CreateNoWindow = $true
  $start.RedirectStandardOutput = $true
  $start.RedirectStandardError = $true
  $process = [Diagnostics.Process]::new()
  $process.StartInfo = $start
  $outFile = $null
  try {
    $outFile = [IO.File]::Open($destination, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    if (-not $process.Start()) { throw 'Git diff did not start' }
    $stderr = $process.StandardError.ReadToEndAsync()
    $process.StandardOutput.BaseStream.CopyTo($outFile)
    $process.WaitForExit()
    $ignored = $stderr.GetAwaiter().GetResult()
    $outFile.Flush($true)
    if ($process.ExitCode -ne 0) { throw 'Git diff preservation failed' }
  } finally { if ($outFile) { $outFile.Dispose() }; $process.Dispose() }
}
try {
  if (-not [IO.Directory]::Exists($src) -or -not [IO.Directory]::Exists((Join-Path $src '.git'))) { throw 'exact private clone unavailable' }
  $head = Head
  $flags = [ordered]@{}
  foreach ($key in @('core.filemode', 'core.autocrlf', 'core.symlinks')) {
    $r = Git-Lines @('config', '--get-all', $key) -MissingOkay
    $local = Git-Lines @('config', '--local', '--get-all', $key) -MissingOkay
    $flags[$key] = [ordered]@{ effective = @($r.lines); effective_exit = $r.exit_code; local = @($local.lines); local_exit = $local.exit_code }
  }
  [long]$rawCount = 0
  & git.exe @prefix status --porcelain=v1 --untracked-files=all 2>$null | ForEach-Object { $rawCount++ }
  if ($LASTEXITCODE -ne 0) { throw 'raw status failed' }
  $content = @(Exact-Status)
  foreach ($p in $paths[0..1]) {
    $full = Join-Path $src $p
    $existing = Get-Item -LiteralPath $full -Force -ErrorAction SilentlyContinue
    if ($null -ne $existing -or [IO.File]::Exists($full) -or [IO.Directory]::Exists($full)) { throw 'expected missing link now exists; preserve and inspect instead' }
    $tree = Git-Lines @('ls-tree', $expectedHead, '--', $p)
    if ($tree.lines.Count -ne 1 -or $tree.lines[0] -cnotmatch ('^120000 blob [0-9a-f]{40}\t' + [regex]::Escape($p) + '$')) { throw 'expected tracked symbolic link not confirmed' }
  }
  foreach ($p in $paths[2..3]) {
    $f = Get-Item -LiteralPath (Join-Path $src $p) -Force
    if ($f.PSIsContainer -or ($f.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'expected plain document unavailable' }
  }
  if (-not $Apply) {
    [ordered]@{ type = 'windows-recovery-checkout-prep-v1'; mode = 'preflight'; passed = $true; head = $head; baseline_flags = $flags; raw_status_count = $rawCount; exact_content_status = $content; mutations = $false } | ConvertTo-Json -Depth 6 -Compress
    exit 0
  }
  $evidence = Join-Path $cache ('checkout-preservation-' + [Guid]::NewGuid().ToString('N'))
  if (Test-Path -LiteralPath $evidence) { throw 'fresh preservation directory already exists' }
  [void][IO.Directory]::CreateDirectory($evidence)
  $stage = 'preserve'
  $docs = @()
  foreach ($p in $paths[2..3]) {
    $original = Join-Path $src $p
    $backup = Join-Path $evidence ([IO.Path]::GetFileName($p) + '.original')
    Copy-Exact $original $backup
    $docs += [ordered]@{ path = $p; backup = $backup; bytes = (Get-Item -LiteralPath $backup).Length; sha256 = File-Hash $backup }
  }
  $patch = Join-Path $evidence 'four-paths.before.patch'
  Raw-Diff $patch ''
  $patchHash = File-Hash $patch
  $beforeStash = Git-Lines @('rev-parse', '--verify', '--quiet', 'refs/stash') -MissingOkay
  $before = [ordered]@{ type = 'windows-recovery-checkout-preservation-v1'; source = $src; head = $head; baseline_flags = $flags; raw_status_count = $rawCount; exact_content_status = $content; missing_links = @($paths[0..1]); documents = $docs; binary_diff = $patch; binary_diff_sha256 = $patchHash; previous_stash = @($beforeStash.lines) }
  New-Json (Join-Path $evidence 'before.json') $before
  $null = Head
  $null = Exact-Status
  foreach ($doc in $docs) { if ((File-Hash (Join-Path $src $doc.path)) -cne $doc.sha256 -or (File-Hash $doc.backup) -cne $doc.sha256) { throw 'document changed during preservation' } }
  $stage = 'platform-config'
  $null = Git-Lines @('config', '--local', 'core.filemode', 'false')
  $null = Git-Lines @('config', '--local', 'core.symlinks', 'false')
  $null = Exact-Status
  $stage = 'named-stash'
  $name = 'kiln-windows-recovery-preserved-' + [IO.Path]::GetFileName($evidence)
  $null = Git-Lines (@('stash', 'push', '--message', $name, '--') + $paths)
  $r = Git-Lines @('rev-parse', '--verify', 'refs/stash')
  if ($r.lines.Count -ne 1 -or $r.lines[0] -cnotmatch '^[0-9a-f]{40}$' -or $beforeStash.lines -ccontains $r.lines[0]) { throw 'new named stash not confirmed' }
  $stash = $r.lines[0]
  $parent = Git-Lines @('rev-parse', ($stash + '^1'))
  $subject = Git-Lines @('show', '-s', '--format=%s', $stash)
  if ($parent.lines.Count -ne 1 -or $parent.lines[0] -cne $expectedHead -or $subject.lines.Count -ne 1 -or -not $subject.lines[0].EndsWith(': ' + $name, [StringComparison]::Ordinal)) { throw 'stash identity mismatch' }
  $saved = Git-Lines @('diff', '--name-only', '--no-renames', ($stash + '^1'), $stash)
  if ($saved.lines.Count -ne 4) { throw 'stash path count mismatch' }
  foreach ($p in $paths) { if ($saved.lines -cnotcontains $p) { throw 'stash path set mismatch' } }
  $afterPatch = Join-Path $evidence 'four-paths.stash.patch'
  Raw-Diff $afterPatch $stash
  if ((File-Hash $afterPatch) -cne $patchHash) { throw 'stash differs from preserved binary diff' }
  $stage = 'verify'
  $null = Head
  $clean = @(Status)
  if ($clean.Count -ne 0) { throw 'source remains dirty; stop without reset or clean' }
  foreach ($doc in $docs) { if ((File-Hash $doc.backup) -cne $doc.sha256) { throw 'preserved document changed' } }
  if ((File-Hash $patch) -cne $patchHash) { throw 'preserved patch changed' }
  $afterFlags = [ordered]@{}
  foreach ($key in @('core.filemode', 'core.autocrlf', 'core.symlinks')) {
    $r = Git-Lines @('config', '--get-all', $key) -MissingOkay
    $afterFlags[$key] = @($r.lines)
    if ($key -eq 'core.autocrlf') {
      if (($r.lines -join "`n") -cne ($flags[$key].effective -join "`n")) { throw 'autocrlf changed unexpectedly' }
    } elseif ($r.lines.Count -ne 1 -or $r.lines[0] -cne 'false') { throw 'platform flag not confirmed' }
  }
  $receipt = [ordered]@{ type = 'windows-recovery-checkout-prep-v1'; mode = 'apply'; passed = $true; source = $src; head = $expectedHead; clean = $true; preservation = $evidence; stash = $stash; stash_name = $name; changed_paths = $paths; documents = $docs; binary_diff_sha256 = $patchHash; final_flags = $afterFlags; no_checkout_build_or_service_change = $true }
  New-Json (Join-Path $evidence 'result.json') $receipt
  $receipt | ConvertTo-Json -Depth 8 -Compress
} catch {
  $failure = [ordered]@{ type = 'windows-recovery-checkout-prep-v1'; passed = $false; stage = $stage; preservation = $evidence; stash = $stash; error_type = $_.Exception.GetType().FullName; reason = $_.Exception.Message; stopped = $true; no_cleanup_reset_or_stash_drop = $true }
  if ($evidence) { try { New-Json (Join-Path $evidence 'failure.json') $failure } catch {} }
  $failure | ConvertTo-Json -Depth 5 -Compress
  exit 1
}
