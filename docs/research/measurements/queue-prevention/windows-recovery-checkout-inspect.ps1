$ErrorActionPreference = 'Stop'
$src = 'C:\Users\corph\.kiln\reach\e30189ea29182644fe5a4b2e06c7dac2\kiln-offline-fix-20261110-8a5ae447ef'
$evidence = 'C:\Users\corph\.cache\kiln-offline-release-20261111\checkout-preservation-7cd1f5ee3aa84f89b2494a21d1de7697'
$expectedHead = 'f555736bd887a4196c8841564cb9b4d6c1ede95f'
$paths = @('deploy/helm/kiln/dashboards', 'deploy/helm/kiln/rules', 'docs/research/measurements/state-view/POLICY_READ.json', 'docs/research/measurements/state-view/ROOT.md')
function Git-Read([string[]]$arguments, [switch]$MissingOkay) {
  $all = @('--no-optional-locks', '-C', $src, '-c', 'core.fsmonitor=false') + $arguments
  $quoted = @($all | ForEach-Object { if ($_ -match '["\r\n]') { throw 'invalid fixed Git argument' }; '"' + $_ + '"' })
  $start = [Diagnostics.ProcessStartInfo]::new()
  $start.FileName = 'git.exe'; $start.Arguments = $quoted -join ' '
  $start.UseShellExecute = $false; $start.CreateNoWindow = $true
  $start.RedirectStandardOutput = $true; $start.RedirectStandardError = $true
  $process = [Diagnostics.Process]::new(); $process.StartInfo = $start
  try {
    if (-not $process.Start()) { throw 'Git read did not start' }
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    $process.WaitForExit()
    $output = $stdout.GetAwaiter().GetResult(); $warnings = $stderr.GetAwaiter().GetResult()
    $code = $process.ExitCode
    if ($code -ne 0 -and -not ($MissingOkay -and $code -eq 1)) { throw ('Git read refused with exit ' + $code) }
    return [pscustomobject]@{ lines = @([regex]::Split($output, '\r?\n') | Where-Object { $_.Length -ne 0 }); exit_code = $code; stderr_present = $warnings.Length -ne 0 }
  } finally { $process.Dispose() }
}
function File-Hash([string]$path) { return (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
try {
  $head = Git-Read @('rev-parse', 'HEAD')
  $status = Git-Read @('status', '--porcelain=v1', '--untracked-files=all')
  $flags = [ordered]@{}
  foreach ($key in @('core.filemode', 'core.autocrlf', 'core.symlinks')) {
    $r = Git-Read @('config', '--get-all', $key) -MissingOkay
    $local = Git-Read @('config', '--local', '--get-all', $key) -MissingOkay
    $flags[$key] = [ordered]@{ effective = @($r.lines); effective_exit = $r.exit_code; local = @($local.lines); local_exit = $local.exit_code }
  }
  $beforePath = Join-Path $evidence 'before.json'
  $beforeFile = [IO.File]::Open($beforePath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
  try {
    if ($beforeFile.Length -gt 524288) { throw 'preservation metadata exceeds control-frame bound' }
    $reader = [IO.StreamReader]::new($beforeFile, [Text.Encoding]::UTF8, $true, 1024, $true)
    try { $before = $reader.ReadToEnd() | ConvertFrom-Json } finally { $reader.Dispose() }
  } finally { $beforeFile.Dispose() }
  if ($before.source -cne $src -or $before.head -cne $expectedHead -or @($before.documents).Count -ne 2) { throw 'preservation metadata identity mismatch' }
  $documents = @()
  foreach ($p in $paths[2..3]) {
    $saved = @($before.documents | Where-Object { $_.path -ceq $p })
    $backup = Join-Path $evidence ([IO.Path]::GetFileName($p) + '.original')
    if ($saved.Count -ne 1 -or $saved[0].backup -cne $backup) { throw 'preserved document path mismatch' }
    $hash = File-Hash $backup
    $f = Get-Item -LiteralPath $backup
    $documents += [ordered]@{ path = $p; backup = $backup; bytes = $f.Length; sha256 = $hash; matches_before = $hash -ceq $saved[0].sha256 -and $f.Length -eq $saved[0].bytes; current_exists = [IO.File]::Exists((Join-Path $src $p)); current_sha256 = if ([IO.File]::Exists((Join-Path $src $p))) { File-Hash (Join-Path $src $p) } else { $null } }
  }
  $patch = Join-Path $evidence 'four-paths.before.patch'
  if ($before.binary_diff -cne $patch) { throw 'preserved diff path mismatch' }
  $patchHash = File-Hash $patch
  $stash = Git-Read @('rev-parse', '--verify', '--quiet', 'refs/stash') -MissingOkay
  $stashInfo = $null
  if ($stash.lines.Count -gt 0) {
    if ($stash.lines.Count -ne 1 -or $stash.lines[0] -cnotmatch '^[0-9a-f]{40}$') { throw 'invalid current stash identity' }
    $oid = $stash.lines[0]
    $parent = Git-Read @('rev-parse', ($oid + '^1'))
    $subject = Git-Read @('show', '-s', '--format=%s', $oid)
    $changed = Git-Read @('diff', '--name-status', '--no-renames', ($oid + '^1'), $oid)
    $names = Git-Read @('diff', '--name-only', '--no-renames', ($oid + '^1'), $oid)
    $exact = $names.lines.Count -eq 4
    foreach ($p in $paths) { if ($names.lines -cnotcontains $p) { $exact = $false } }
    $expectedName = 'kiln-windows-recovery-preserved-' + [IO.Path]::GetFileName($evidence)
    $stashInfo = [ordered]@{ oid = $oid; is_new_from_before = @($before.previous_stash) -cnotcontains $oid; parent = @($parent.lines); parent_matches = $parent.lines.Count -eq 1 -and $parent.lines[0] -ceq $expectedHead; subject = @($subject.lines); expected_name = $expectedName; name_matches = $subject.lines.Count -eq 1 -and $subject.lines[0].EndsWith(': ' + $expectedName, [StringComparison]::Ordinal); changed_count = $changed.lines.Count; changed_first20 = @($changed.lines | Select-Object -First 20); exact_four_paths = $exact }
  }
  $links = @($paths[0..1] | ForEach-Object { $p = $_; $f = Get-Item -LiteralPath (Join-Path $src $p) -Force -ErrorAction SilentlyContinue; [ordered]@{ path = $p; exists = $null -ne $f; attributes = if ($f) { $f.Attributes.ToString() } else { $null } } })
  [ordered]@{ type = 'windows-recovery-checkout-inspection-v1'; observed = $true; source = $src; head = @($head.lines); expected_head = $expectedHead; head_matches = $head.lines.Count -eq 1 -and $head.lines[0] -ceq $expectedHead; flags = $flags; status_count = $status.lines.Count; status_first20 = @($status.lines | Select-Object -First 20); status_complete = $status.lines.Count -le 20; preservation = $evidence; before_sha256 = File-Hash $beforePath; documents = $documents; patch_sha256 = $patchHash; patch_matches_before = $patchHash -ceq $before.binary_diff_sha256; stash = $stashInfo; links = $links; writes = $false; no_stash_apply_reset_checkout_build = $true } | ConvertTo-Json -Depth 9 -Compress
} catch {
  [ordered]@{ type = 'windows-recovery-checkout-inspection-v1'; observed = $false; error_type = $_.Exception.GetType().FullName; reason = $_.Exception.Message; writes = $false } | ConvertTo-Json -Compress
  exit 1
}
