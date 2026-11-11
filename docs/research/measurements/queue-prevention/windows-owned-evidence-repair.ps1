param([switch]$Apply)
$ErrorActionPreference = 'Stop'
$expected = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('ewogICJzb3VyY2UiOiAiQzpcXFVzZXJzXFxjb3JwaFxcLmtpbG5cXHJlYWNoXFxlMzAxODllYTI5MTgyNjQ0ZmU1YTRiMmUwNmM3ZGFjMlxca2lsbi1vZmZsaW5lLWZpeC0yMDI2MTExMC04YTVhZTQ0N2VmIiwKICAiaGVhZCI6ICJlZjQ3M2NmNWZhNDAwZWQ1NjdlMTg5Zjg2ZTFhOGNmM2E0MTA4NGNkIiwKICAib3JpZ2luYWxfc3Rhc2giOiAiMDBiMmY1ZWU2Y2Y5NTk1N2UzOGMxZmQxYWFhYjMzMDEwZTc5MDNjYiIsCiAgImVudHJpZXMiOiBbCiAgICB7CiAgICAgICJwYXRoIjogImRvY3MvcmVzZWFyY2gvbWVhc3VyZW1lbnRzL3F1ZXVlLXByZXZlbnRpb24vYXZhaWxhYmlsaXR5LWJyb3dzZXItcGFpbnRlZC9zZXNzaW9uLWF2YWlsYWJpbGl0eS1vd25lci05YzA0NS1pbGFibGUtd2hpbGUtZGV0YWlscy1hcnJpdmUtZGVza3RvcC13ZWJraXQvYXZhaWxhYmlsaXR5LWRlc2t0b3Atd2Via2l0LnBuZyIsCiAgICAgICJtb2RlIjogIjEwMDY0NCIsCiAgICAgICJibG9iIjogImI3MGRmZTVhNTNmZWMwZGU3YjQ5NTA5YTM2MTNjY2VjZWU1ODIwOGQiLAogICAgICAiYnl0ZXMiOiA5MDI5NiwKICAgICAgImZ1bGxfcGF0aF91dGYxNl91bml0cyI6IDI3NwogICAgfSwKICAgIHsKICAgICAgInBhdGgiOiAiZG9jcy9yZXNlYXJjaC9tZWFzdXJlbWVudHMvcXVldWUtcHJldmVudGlvbi9hdmFpbGFiaWxpdHktYnJvd3Nlci1wYWludGVkL3Nlc3Npb24tYXZhaWxhYmlsaXR5LW93bmVyLTljMDQ1LWlsYWJsZS13aGlsZS1kZXRhaWxzLWFycml2ZS1kZXNrdG9wL2F2YWlsYWJpbGl0eS1kZXNrdG9wLnBuZyIsCiAgICAgICJtb2RlIjogIjEwMDY0NCIsCiAgICAgICJibG9iIjogIjQyMTI4MGJlZTc4N2RmNDg0MWFlNDM2MjM3OTk3NDk2YjgyZDVlZTMiLAogICAgICAiYnl0ZXMiOiA5MzI1MywKICAgICAgImZ1bGxfcGF0aF91dGYxNl91bml0cyI6IDI2MwogICAgfSwKICAgIHsKICAgICAgInBhdGgiOiAiZG9jcy9yZXNlYXJjaC9tZWFzdXJlbWVudHMvcXVldWUtcHJldmVudGlvbi9hdmFpbGFiaWxpdHktYnJvd3Nlci1wYWludGVkL3Nlc3Npb24tYXZhaWxhYmlsaXR5LW93bmVyLTljMDQ1LWlsYWJsZS13aGlsZS1kZXRhaWxzLWFycml2ZS1waG9uZS13ZWJraXQvYXZhaWxhYmlsaXR5LXBob25lLXdlYmtpdC5wbmciLAogICAgICAibW9kZSI6ICIxMDA2NDQiLAogICAgICAiYmxvYiI6ICI2YzQwMmZiNDBkNDMwYjU2YTFmNjg2NGEwNDUwOTc0NjNlZWI2MTE3IiwKICAgICAgImJ5dGVzIjogMjA3Mjk3LAogICAgICAiZnVsbF9wYXRoX3V0ZjE2X3VuaXRzIjogMjczCiAgICB9LAogICAgewogICAgICAicGF0aCI6ICJkb2NzL3Jlc2VhcmNoL21lYXN1cmVtZW50cy9xdWV1ZS1wcmV2ZW50aW9uL2F2YWlsYWJpbGl0eS1icm93c2VyL3Nlc3Npb24tYXZhaWxhYmlsaXR5LW93bmVyLTljMDQ1LWlsYWJsZS13aGlsZS1kZXRhaWxzLWFycml2ZS1kZXNrdG9wLXdlYmtpdC9hdmFpbGFiaWxpdHktZGVza3RvcC13ZWJraXQucG5nIiwKICAgICAgIm1vZGUiOiAiMTAwNjQ0IiwKICAgICAgImJsb2IiOiAiYjRkNjc2MGNhYTBlZjE4ZGM2N2M3Zjg3NDYwOTcwNGNjNzQ5ZTk5NCIsCiAgICAgICJieXRlcyI6IDkxMjgwLAogICAgICAiZnVsbF9wYXRoX3V0ZjE2X3VuaXRzIjogMjY5CiAgICB9LAogICAgewogICAgICAicGF0aCI6ICJkb2NzL3Jlc2VhcmNoL21lYXN1cmVtZW50cy9xdWV1ZS1wcmV2ZW50aW9uL2F2YWlsYWJpbGl0eS1icm93c2VyL3Nlc3Npb24tYXZhaWxhYmlsaXR5LW93bmVyLTljMDQ1LWlsYWJsZS13aGlsZS1kZXRhaWxzLWFycml2ZS1waG9uZS13ZWJraXQvYXZhaWxhYmlsaXR5LXBob25lLXdlYmtpdC5wbmciLAogICAgICAibW9kZSI6ICIxMDA2NDQiLAogICAgICAiYmxvYiI6ICJhZGFhZmYzMWM1MmE0ZWU2YjYxMjgzNzVkYzQ1Njk0ZTZhYmU3ZjhjIiwKICAgICAgImJ5dGVzIjogNjExNDMsCiAgICAgICJmdWxsX3BhdGhfdXRmMTZfdW5pdHMiOiAyNjUKICAgIH0KICBdCn0K')) | ConvertFrom-Json
$src = [IO.Path]::GetFullPath($expected.source)
$entries = @($expected.entries)
$paths = @($entries | ForEach-Object { $_.path })
$stage = 'inspect'
$changedConfig = $false
$restored = $false
function Git-Read([string[]]$arguments, [switch]$MissingOkay) {
  $all = @('--no-optional-locks', '-C', $src, '-c', 'core.fsmonitor=false') + $arguments
  $quoted = @($all | ForEach-Object { if ($_ -match '["\r\n]') { throw 'invalid fixed Git argument' }; '"' + $_ + '"' })
  $start = [Diagnostics.ProcessStartInfo]::new()
  $start.FileName = 'git.exe'; $start.Arguments = $quoted -join ' '
  $start.UseShellExecute = $false; $start.CreateNoWindow = $true
  $start.RedirectStandardOutput = $true; $start.RedirectStandardError = $true
  $p = [Diagnostics.Process]::new(); $p.StartInfo = $start
  try {
    if (-not $p.Start()) { throw 'native Git did not start' }
    $stdout = $p.StandardOutput.ReadToEndAsync(); $stderr = $p.StandardError.ReadToEndAsync()
    $p.WaitForExit(); $text = $stdout.GetAwaiter().GetResult(); $warnings = $stderr.GetAwaiter().GetResult()
    if ($p.ExitCode -ne 0 -and -not ($MissingOkay -and $p.ExitCode -eq 1)) { throw ('native Git refused with exit ' + $p.ExitCode) }
    return [pscustomobject]@{ lines = @([regex]::Split($text, '\r?\n') | Where-Object { $_.Length -ne 0 }); exit_code = $p.ExitCode; stderr_present = $warnings.Length -ne 0 }
  } finally { $p.Dispose() }
}
function Check-Identity {
  $head = Git-Read @('rev-parse', 'HEAD')
  $stash = Git-Read @('rev-parse', 'refs/stash')
  if ($head.lines.Count -ne 1 -or $head.lines[0] -cne $expected.head -or $stash.lines.Count -ne 1 -or $stash.lines[0] -cne $expected.original_stash) { throw 'owned HEAD or original stash changed' }
}
function Status { return (Git-Read @('status', '--porcelain=v1', '--untracked-files=all')).lines }
function Exact-Deletions($status) {
  if (@($status).Count -ne $entries.Count) { return $false }
  foreach ($path in $paths) { if ($status -cnotcontains (' D ' + $path)) { return $false } }
  return $true
}
try {
  Check-Identity
  $effective = Git-Read @('config', '--type=bool', '--get', 'core.longpaths') -MissingOkay
  $local = Git-Read @('config', '--local', '--type=bool', '--get', 'core.longpaths') -MissingOkay
  $enabled = $effective.lines.Count -eq 1 -and $effective.lines[0] -ceq 'true'
  $status = @(Status)
  $pathFacts = @()
  $long = $false
  foreach ($entry in $entries) {
    $tree = Git-Read @('ls-tree', 'HEAD', '--', $entry.path)
    $wanted = $entry.mode + ' blob ' + $entry.blob + "`t" + $entry.path
    if ($tree.lines.Count -ne 1 -or $tree.lines[0] -cne $wanted) { throw 'tracked screenshot blob differs from pinned ef HEAD' }
    $size = Git-Read @('cat-file', '-s', $entry.blob)
    if ($size.lines.Count -ne 1 -or [long]$size.lines[0] -ne $entry.bytes) { throw 'tracked screenshot byte size differs' }
    $full = [IO.Path]::GetFullPath((Join-Path $src $entry.path))
    if ($full.Length -ne $entry.full_path_utf16_units) { throw 'actual Windows path length differs from prepared metadata' }
    # MAX_PATH260 is the documented default Win32 boundary, not a new runtime allowance.
    if ($full.Length -ge 260) { $long = $true }
    $extended = ([char]92).ToString() + [char]92 + '?' + [char]92 + $full
    $pathFacts += [ordered]@{ path = $entry.path; head_blob = $entry.blob; head_bytes = $entry.bytes; full_path_utf16_units = $full.Length; normal_exists = [IO.File]::Exists($full); extended_exists = [IO.File]::Exists($extended) }
  }
  $exact = Exact-Deletions $status
  $before = [ordered]@{ head = $expected.head; original_stash = $expected.original_stash; core_longpaths = @($effective.lines); core_longpaths_exit = $effective.exit_code; local_core_longpaths = @($local.lines); local_core_longpaths_exit = $local.exit_code; status_count = $status.Count; status_first_expected_plus_one = @($status | Select-Object -First ($entries.Count + 1)); exact_five_deletions = $exact; path_facts = $pathFacts; local_longpaths_needed_for_restore = (-not $enabled) -and $long }
  if (-not $Apply) {
    [ordered]@{ type = 'windows-owned-png-preview-v1'; observed = $true; ready_for_apply = $exact; before = $before; writes = $false; cause = 'config/path-length facts only; original deletion cause remains unconfirmed' } | ConvertTo-Json -Depth 8 -Compress
    exit 0
  }
  if (-not $exact) { throw 'status does not match exactly the five authorized missing screenshots' }
  $stage = 'recheck'
  Check-Identity
  if (-not (Exact-Deletions @(Status))) { throw 'status changed before scoped repair' }
  if (-not $enabled -and $long) {
    $stage = 'local-longpaths'
    $null = Git-Read @('config', '--local', 'core.longpaths', 'true')
    $changedConfig = $true
  }
  $now = @(Status)
  if ($now.Count -gt 0) {
    if (-not (Exact-Deletions $now)) { throw 'enabled path handling revealed different changes; no files restored' }
    $stage = 'restore-exact-five'
    $null = Git-Read (@('restore', '--source=HEAD', '--worktree', '--') + $paths)
    $restored = $true
  }
  $stage = 'verify'
  Check-Identity
  if (@(Status).Count -ne 0) { throw 'source is not clean after named repair' }
  $verified = @()
  foreach ($entry in $entries) {
    $blob = Git-Read @('hash-object', '--no-filters', '--', $entry.path)
    if ($blob.lines.Count -ne 1 -or $blob.lines[0] -cne $entry.blob) { throw 'restored screenshot bytes differ from HEAD' }
    $verified += [ordered]@{ path = $entry.path; actual_git_blob = $blob.lines[0]; bytes = $entry.bytes }
  }
  $after = Git-Read @('config', '--type=bool', '--get', 'core.longpaths') -MissingOkay
  [ordered]@{ type = 'windows-owned-png-repair-v1'; passed = $true; before = $before; local_longpaths_changed = $changedConfig; restored_exact_five = $restored; final_core_longpaths = @($after.lines); clean = $true; head = $expected.head; original_stash = $expected.original_stash; verified = $verified; no_global_config_reset_stash_clean_or_service_action = $true; cause = 'scoped restore/handling verified; original deletion cause remains unconfirmed' } | ConvertTo-Json -Depth 9 -Compress
} catch {
  [ordered]@{ type = 'windows-owned-png-repair-v1'; passed = $false; stage = $stage; before = $before; local_longpaths_changed = $changedConfig; restored_exact_five = $restored; error_type = $_.Exception.GetType().FullName; reason = $_.Exception.Message; stopped_without_reset_stash_clean_or_service_action = $true } | ConvertTo-Json -Depth 8 -Compress
  exit 1
}
