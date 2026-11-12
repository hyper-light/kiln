$ErrorActionPreference = 'Stop'
$src = 'C:\Users\corph\.kiln\reach\e30189ea29182644fe5a4b2e06c7dac2\kiln-offline-fix-20261110-8a5ae447ef'
$preserved = 'C:\Users\corph\.cache\kiln-offline-release-20261111\checkout-preservation-7cd1f5ee3aa84f89b2494a21d1de7697'
$head = 'f555736bd887a4196c8841564cb9b4d6c1ede95f'
$stash = '00b2f5ee6cf95957e38c1fd1aaab33010e7903cb'
$expectedPatch = '98030f40fdc7f3e9247dddbc2d67c3f87327fc7fd18f5bdc470b8a3cb07e341e'
$paths = @('deploy/helm/kiln/dashboards', 'deploy/helm/kiln/rules', 'docs/research/measurements/state-view/POLICY_READ.json', 'docs/research/measurements/state-view/ROOT.md')
function File-Hash([string]$path) { return (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Git-Start([string[]]$arguments) {
  $all = @('--no-optional-locks', '-C', $src, '-c', 'core.fsmonitor=false', '-c', 'core.filemode=false') + $arguments
  $quoted = @($all | ForEach-Object { if ($_ -match '["\r\n]') { throw 'invalid fixed Git argument' }; '"' + $_ + '"' })
  $start = [Diagnostics.ProcessStartInfo]::new()
  $start.FileName = 'git.exe'; $start.Arguments = $quoted -join ' '
  $start.UseShellExecute = $false; $start.CreateNoWindow = $true
  $start.RedirectStandardOutput = $true; $start.RedirectStandardError = $true
  $p = [Diagnostics.Process]::new(); $p.StartInfo = $start
  if (-not $p.Start()) { $p.Dispose(); throw 'Git did not start' }
  return $p
}
function Git-Read([string[]]$arguments) {
  $p = Git-Start $arguments
  try {
    $stdout = $p.StandardOutput.ReadToEndAsync(); $stderr = $p.StandardError.ReadToEndAsync()
    $p.WaitForExit()
    $text = $stdout.GetAwaiter().GetResult(); $warnings = $stderr.GetAwaiter().GetResult()
    if ($p.ExitCode -ne 0) { throw ('Git read failed: ' + $p.ExitCode) }
    return @([regex]::Split($text, '\r?\n') | Where-Object { $_.Length -ne 0 })
  } finally { $p.Dispose() }
}
function Check-Source {
  $actual = @(Git-Read @('rev-parse', 'HEAD'))
  if ($actual.Count -ne 1 -or $actual[0] -cne $head) { throw 'HEAD changed' }
  $dirty = @(Git-Read @('status', '--porcelain=v1', '--untracked-files=all'))
  if ($dirty.Count -ne 0) { throw 'source is not clean' }
}
try {
  Check-Source
  $current = @(Git-Read @('rev-parse', 'refs/stash'))
  if ($current.Count -ne 1 -or $current[0] -cne $stash) { throw 'current stash changed' }
  $parent = @(Git-Read @('rev-parse', ($stash + '^1')))
  if ($parent.Count -ne 1 -or $parent[0] -cne $head) { throw 'stash parent changed' }
  $beforePath = Join-Path $preserved 'before.json'
  if ((Get-Item -LiteralPath $beforePath).Length -gt 524288) { throw 'before receipt exceeds control metadata bound' }
  $before = [IO.File]::ReadAllText($beforePath) | ConvertFrom-Json
  if ($before.head -cne $head -or $before.source -cne $src -or @($before.documents).Count -ne 2) { throw 'before identity mismatch' }
  $documents = @()
  foreach ($path in $paths[2..3]) {
    $record = @($before.documents | Where-Object { $_.path -ceq $path })
    $backup = Join-Path $preserved ([IO.Path]::GetFileName($path) + '.original')
    if ($record.Count -ne 1 -or $record[0].backup -cne $backup) { throw 'backup path mismatch' }
    $hash = File-Hash $backup
    $length = (Get-Item -LiteralPath $backup).Length
    if ($hash -cne $record[0].sha256 -or $length -ne $record[0].bytes) { throw 'backup differs from before receipt' }
    $documents += [ordered]@{ path = $path; bytes = $length; sha256 = $hash }
  }
  $patch = Join-Path $preserved 'four-paths.before.patch'
  if ($before.binary_diff -cne $patch -or $before.binary_diff_sha256 -cne $expectedPatch -or (File-Hash $patch) -cne $expectedPatch) { throw 'preserved patch hash mismatch' }
  $length = (Get-Item -LiteralPath $patch).Length
  if ($length -ge [int]::MaxValue) { throw 'preserved patch exceeds MemoryStream capacity' }
  $memory = [IO.MemoryStream]::new([int]($length + 1))
  $p = Git-Start (@('diff', '--binary', '--full-index', '--no-ext-diff', '--no-textconv', '--no-renames', $head, $stash, '--') + $paths)
  try {
    $stderr = $p.StandardError.ReadToEndAsync()
    # At most the pinned patch length plus one detection byte is retained/read.
    $buffer = New-Object byte[] ([int]($length + 1))
    while ($true) {
      $remaining = $length + 1 - $memory.Length
      if ($remaining -le 0) { throw 'stash diff exceeds pinned patch length' }
      $n = $p.StandardOutput.BaseStream.Read($buffer, 0, [int]$remaining)
      if ($n -eq 0) { break }
      $memory.Write($buffer, 0, $n)
    }
    $p.WaitForExit(); $warnings = $stderr.GetAwaiter().GetResult()
    if ($p.ExitCode -ne 0 -or $memory.Length -ne $length) { throw 'stash diff failed or length differs' }
    $memory.Position = 0
    $hasher = [Security.Cryptography.SHA256]::Create()
    try { $hash = [BitConverter]::ToString($hasher.ComputeHash($memory)).Replace('-', '').ToLowerInvariant() } finally { $hasher.Dispose() }
    if ($hash -cne $expectedPatch) { throw 'stash diff hash mismatch' }
  } finally { $p.Dispose(); $memory.Dispose() }
  Check-Source
  foreach ($doc in $documents) { if ((File-Hash (Join-Path $preserved ([IO.Path]::GetFileName($doc.path) + '.original'))) -cne $doc.sha256) { throw 'backup changed during proof' } }
  [ordered]@{ type = 'windows-recovery-stash-proof-v1'; passed = $true; head = $head; clean = $true; stash = $stash; stash_parent = $head; diff_sha256 = $hash; diff_bytes = $length; preserved_patch_matches = $true; documents = $documents; writes = $false; no_stash_config_checkout_build_mutation = $true } | ConvertTo-Json -Depth 6 -Compress
} catch {
  [ordered]@{ type = 'windows-recovery-stash-proof-v1'; passed = $false; error_type = $_.Exception.GetType().FullName; reason = $_.Exception.Message; writes = $false } | ConvertTo-Json -Compress
  exit 1
}
