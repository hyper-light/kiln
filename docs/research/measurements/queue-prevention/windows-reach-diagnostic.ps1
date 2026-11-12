param([switch]$Fixture)
$ErrorActionPreference = 'Stop'
$clock = [Diagnostics.Stopwatch]::StartNew()
$result = [ordered]@{
    utc = [DateTime]::UtcNow.ToString('o')
    type = 'windows-reach-diagnostic-v1'
    task = 'kiln-agent'
    fixture_requested = [bool]$Fixture
    deadline_seconds = 30
}
function Error-Code($errorRecord) {
    return [ordered]@{ type = $errorRecord.Exception.GetType().FullName; hresult = $errorRecord.Exception.HResult }
}
function Before-Deadline {
    if ($clock.Elapsed.TotalSeconds -ge 30) { throw [TimeoutException]::new('diagnostic deadline') }
}
try {
    Before-Deadline
    # Keep the exact task XML/arguments in memory; neither is emitted.
    $scheduler = New-Object -ComObject Schedule.Service
    $scheduler.Connect()
    $task = $scheduler.GetFolder('\').GetTask('kiln-agent')
    [xml]$xml = $task.Xml
    $ns = New-Object Xml.XmlNamespaceManager($xml.NameTable)
    $ns.AddNamespace('t', 'http://schemas.microsoft.com/windows/2004/02/mit/task')
    $exec = @($xml.SelectNodes('/t:Task/t:Actions/t:Exec', $ns))
    if ($exec.Count -ne 1) { throw [InvalidOperationException]::new('task action ambiguous') }
    $arguments = [string]$exec[0].Arguments
    if ($arguments -notmatch '(?:^|\s)--\s+agent(?:\s|$)') { throw [InvalidOperationException]::new('not runner task') }
    $stateMatches = [regex]::Matches($arguments, '(?:^|\s)--state-dir(?:=|\s+)(?:"([^"]+)"|([^\s"]+))')
    if ($stateMatches.Count -gt 1) { throw [InvalidOperationException]::new('state directory ambiguous') }
    if ($stateMatches.Count -eq 1) {
        $state = $stateMatches[0].Groups[1].Value
        if (-not $state) { $state = $stateMatches[0].Groups[2].Value }
        $result.state_source = 'task-state-dir'
    } else {
        # The default is safe to select only for the current task principal.
        $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
        $taskUser = [string]$xml.SelectSingleNode('/t:Task/t:Principals/t:Principal/t:UserId', $ns).InnerText
        if (($taskUser -ine $identity.Name) -and ($taskUser -ine $identity.User.Value)) {
            throw [InvalidOperationException]::new('task default principal unverified')
        }
        $state = [IO.Path]::Combine([Environment]::GetFolderPath('LocalApplicationData'), 'kiln', 'agent')
        $result.state_source = 'verified-current-principal-default'
    }
    if (-not [IO.Path]::IsPathRooted($state)) { throw [InvalidOperationException]::new('state directory is relative') }
    $log = [IO.Path]::Combine($state, 'kiln.log')
    $result.state_dir = $state
    $result.log_path = $log
    $result.task_state = [int]$task.State
    Before-Deadline
    $stream = [IO.File]::Open($log, [IO.FileMode]::Open, [IO.FileAccess]::Read, ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
    try {
        $length = $stream.Length
        $count = [int][Math]::Min(32768, $length)
        $start = $length - $count
        [void]$stream.Seek($start, [IO.SeekOrigin]::Begin)
        $buffer = New-Object byte[] $count
        $got = 0
        while ($got -lt $count) {
            Before-Deadline
            $read = $stream.Read($buffer, $got, $count - $got)
            if ($read -eq 0) { break }
            $got += $read
        }
        $lines = @([Text.Encoding]::UTF8.GetString($buffer, 0, $got) -split '\r?\n')
        if (($start -gt 0) -and ($lines.Count -gt 0)) { $lines = @($lines | Select-Object -Skip 1) }
        $safe = @($lines | Where-Object {
            ($_ -match 'reach is off on this runner|placement.s epoch couldn.t be kept|reach ready') -and
            ($_ -notmatch '(?i)token|authorization|password|private.?key|BEGIN .*KEY|secret|credential')
        } | Select-Object -Last 12 | ForEach-Object {
            $line = $_ -replace '\x1b\[[0-9;]*m', ''
            if ($line.Length -gt 800) { $line = $line.Substring(0, 800) }
            $line
        })
        $result.log = [ordered]@{ total_bytes = $length; tail_bytes_read = $got; filtered_lines = $safe }
    } finally { $stream.Dispose() }
} catch { $result.diagnostic_error = Error-Code $_ }

if ($Fixture) {
    $fixtureResult = [ordered]@{ type = 'private-20-byte-flush'; cleanup = 'not-created' }
    $privateDir = $null
    $ownedFile = $null
    $ownedCreated = $false
    $expected = [Text.Encoding]::ASCII.GetBytes('0123456789abcdefghij')
    try {
        Before-Deadline
        # FileStream.Flush(true) alone is not equivalent to Rust sync_all on a read-only handle.
        Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
namespace KilnEpochSyncFixture {
    public static class Native {
        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool FlushFileBuffers(SafeFileHandle handle);
    }
}
'@
        $candidate = [IO.Path]::Combine([IO.Path]::GetTempPath(), 'kiln-epoch-sync-' + [Guid]::NewGuid().ToString('N'))
        if ([IO.Directory]::Exists($candidate)) { throw [IO.IOException]::new('private path already exists') }
        [void][IO.Directory]::CreateDirectory($candidate)
        $privateDir = $candidate
        $ownedFile = [IO.Path]::Combine($privateDir, 'epoch.tmp')
        $created = [IO.File]::Open($ownedFile, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        $ownedCreated = $true
        try { $created.Write($expected, 0, $expected.Length) } finally { $created.Dispose() }
        $fixtureResult.bytes = $expected.Length
        $fixtureResult.path = $ownedFile
        # Retain this exact ownership receipt even if a subsequent OS flush blocks.
        [ordered]@{ type = 'fixture-owned-path'; path = $ownedFile; bytes = 20; content_ascii = '0123456789abcdefghij' } | ConvertTo-Json -Compress
        $cases = @()
        foreach ($access in @([IO.FileAccess]::Read, [IO.FileAccess]::Write)) {
            Before-Deadline
            $case = [ordered]@{ access = $access.ToString() }
            $file = [IO.File]::Open($ownedFile, [IO.FileMode]::Open, $access, [IO.FileShare]::None)
            try {
                try { $file.Flush($true); $case.managed_flush = 'returned' }
                catch { $case.managed_error = Error-Code $_ }
                $ok = [KilnEpochSyncFixture.Native]::FlushFileBuffers($file.SafeFileHandle)
                $nativeError = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
                $case.native_flush = [bool]$ok
                if (-not $ok) { $case.native_error = $nativeError }
            } finally { $file.Dispose() }
            $cases += $case
        }
        $fixtureResult.cases = $cases
    } catch { $fixtureResult.error = Error-Code $_ }
    finally {
        try {
            if ($ownedCreated -and [IO.File]::Exists($ownedFile)) {
                $actual = [IO.File]::ReadAllBytes($ownedFile)
                if ([Convert]::ToBase64String($actual) -cne [Convert]::ToBase64String($expected)) {
                    throw [IO.IOException]::new('owned content changed; preserved')
                }
                [IO.File]::Delete($ownedFile)
            }
            if ($privateDir) { [IO.Directory]::Delete($privateDir, $false) }
            if ($privateDir) { $fixtureResult.cleanup = 'exact-owned-content-removed' }
        } catch { $fixtureResult.cleanup = 'failed-preserved'; $fixtureResult.cleanup_error = Error-Code $_ }
    }
    $result.fixture = $fixtureResult
}
$result.elapsed_ms = $clock.ElapsedMilliseconds
$result | ConvertTo-Json -Depth 8 -Compress
