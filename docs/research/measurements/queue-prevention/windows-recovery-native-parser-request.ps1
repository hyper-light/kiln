$ErrorActionPreference = 'Stop'
try {
  $packed = [Convert]::FromBase64String('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')
  $inputBytes = [IO.MemoryStream]::new([byte[]]$packed)
  $gzip = [IO.Compression.GZipStream]::new($inputBytes, [IO.Compression.CompressionMode]::Decompress)
  $decoded = [IO.MemoryStream]::new()
  try { $gzip.CopyTo($decoded); $bytes = $decoded.ToArray() }
  finally { $gzip.Dispose(); $inputBytes.Dispose(); $decoded.Dispose() }
  if ($bytes.Length -ne 6893) { throw 'decoded step length mismatch' }
  $hasher = [Security.Cryptography.SHA256]::Create()
  try { $hash = [BitConverter]::ToString($hasher.ComputeHash($bytes)).Replace('-', '').ToLowerInvariant() }
  finally { $hasher.Dispose() }
  if ($hash -cne '340dbc0cff489c55a52a9c3298358255ad599bedc89cfa6972a994467b674d76') { throw 'decoded step SHA256 mismatch' }
  $source = [Text.Encoding]::UTF8.GetString($bytes)
  $tokens = $null
  $errors = $null
  $ast = [Management.Automation.Language.Parser]::ParseInput($source, [ref]$tokens, [ref]$errors)
  $issues = @($errors | ForEach-Object { [ordered]@{ id = $_.ErrorId; message = $_.Message; line = $_.Extent.StartLineNumber; column = $_.Extent.StartColumnNumber } })
  # Same parameter signature and literal-array call shape; this mock only returns data.
  function Test-CheckedNativeBinding([string]$exe, [string[]]$arguments) {
    return [ordered]@{ exe = $exe; arguments = @($arguments); argument_count = $arguments.Count }
  }
  $binding = Test-CheckedNativeBinding git @('fetch', '--no-tags', 'C:\parser fixture', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa')
  $bindingPass = $binding.exe -ceq 'git' -and $binding.argument_count -eq 4 -and $binding.arguments[0] -ceq 'fetch' -and $binding.arguments[2] -ceq 'C:\parser fixture' -and $binding.arguments[3] -ceq 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
  $sourcePath = 'C:\Users\corph\.kiln\reach\e30189ea29182644fe5a4b2e06c7dac2\kiln-offline-fix-20261110-8a5ae447ef'
  $targetPath = 'C:\Users\corph\.cache\kiln-offline-release-20261111\windows-native-target'
  $head = @(& git.exe --no-optional-locks -C $sourcePath rev-parse HEAD 2>$null)
  $headExit = $LASTEXITCODE
  $sample = [Collections.Generic.List[string]]::new()
  [long]$statusCount = 0
  # Stream status: count all lines but retain at most20; no optional index update/fsmonitor daemon.
  & git.exe --no-optional-locks -C $sourcePath -c core.fsmonitor=false status --porcelain --untracked-files=all 2>&1 | ForEach-Object {
    $statusCount++
    if ($sample.Count -lt 20) { [void]$sample.Add($_.ToString()) }
  }
  $statusExit = $LASTEXITCODE
  $filemode = @(& git.exe --no-optional-locks -C $sourcePath config --get core.filemode 2>$null)
  $filemodeExit = $LASTEXITCODE
  $autocrlf = @(& git.exe --no-optional-locks -C $sourcePath config --get core.autocrlf 2>$null)
  $autocrlfExit = $LASTEXITCODE
  $git = [ordered]@{ source = $sourcePath; source_exists = [IO.Directory]::Exists($sourcePath); head = $head; head_exit = $headExit; status_exit = $statusExit; status_line_count = $statusCount; status_first20 = @($sample.ToArray()); status_sample_complete = $statusCount -le 20; core_filemode = $filemode; core_filemode_exit = $filemodeExit; core_autocrlf = $autocrlf; core_autocrlf_exit = $autocrlfExit; cargo_target = $targetPath; cargo_target_exists = [IO.Directory]::Exists($targetPath) }
  $pass = $issues.Count -eq 0 -and $bindingPass
  [ordered]@{ type = 'windows-recovery-parser-v1'; passed = $pass; workflow_sha256 = '46610bbd138a560d15977934a1cf3bdeb26a399e835bc5c826110d7273fba3a4'; step_sha256 = $hash; step_bytes = $bytes.Length; parser_errors = $issues; checked_native_binding = $binding; binding_pass = $bindingPass; readonly_git_preflight = $git; step_executed = $false; explicit_file_mutations = $false } | ConvertTo-Json -Depth 6 -Compress
  if (-not $pass) { exit 1 }
} catch {
  [ordered]@{ type = 'windows-recovery-parser-v1'; passed = $false; error_type = $_.Exception.GetType().FullName; step_executed = $false; explicit_file_mutations = $false } | ConvertTo-Json -Compress
  exit 1
}
