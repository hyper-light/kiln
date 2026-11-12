#!/usr/bin/env node
// Prepare an owned-terminal request; never sends it or executes the workflow step.
import fs from 'node:fs';
import crypto from 'node:crypto';
import { gzipSync } from 'node:zlib';
import { parseDocument } from '../../../../ui/node_modules/yaml/dist/index.js';

const [workflowPath, outputPrefix] = process.argv.slice(2);
if (!workflowPath || !outputPrefix || process.argv.length !== 4) throw new Error('usage: node windows-recovery-native-parser.mjs WORKFLOW OUTPUT_PREFIX');
const workflow = fs.readFileSync(workflowPath);
const document = parseDocument(workflow.toString('utf8'));
if (document.errors.length) throw new Error('workflow YAML did not parse');
const value = document.toJS();
if (JSON.stringify(value.on) !== JSON.stringify({ manual: true }) || Object.keys(value.jobs).join() !== 'native' || value.jobs.native.steps.length !== 1) throw new Error('expected the exact manual-only single-job workflow');
const step = value.jobs.native.steps[0].run;
if (typeof step !== 'string') throw new Error('missing exact step text');
const bytes = Buffer.from(step, 'utf8');
const digest = b => crypto.createHash('sha256').update(b).digest('hex');
const stepHash = digest(bytes);
const workflowHash = digest(workflow);
const payload = gzipSync(bytes).toString('base64');
const script = String.raw`$ErrorActionPreference = 'Stop'
try {
  $packed = [Convert]::FromBase64String('${payload}')
  $inputBytes = [IO.MemoryStream]::new([byte[]]$packed)
  $gzip = [IO.Compression.GZipStream]::new($inputBytes, [IO.Compression.CompressionMode]::Decompress)
  $decoded = [IO.MemoryStream]::new()
  try { $gzip.CopyTo($decoded); $bytes = $decoded.ToArray() }
  finally { $gzip.Dispose(); $inputBytes.Dispose(); $decoded.Dispose() }
  if ($bytes.Length -ne ${bytes.length}) { throw 'decoded step length mismatch' }
  $hasher = [Security.Cryptography.SHA256]::Create()
  try { $hash = [BitConverter]::ToString($hasher.ComputeHash($bytes)).Replace('-', '').ToLowerInvariant() }
  finally { $hasher.Dispose() }
  if ($hash -cne '${stepHash}') { throw 'decoded step SHA256 mismatch' }
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
  [ordered]@{ type = 'windows-recovery-parser-v1'; passed = $pass; workflow_sha256 = '${workflowHash}'; step_sha256 = $hash; step_bytes = $bytes.Length; parser_errors = $issues; checked_native_binding = $binding; binding_pass = $bindingPass; readonly_git_preflight = $git; step_executed = $false; explicit_file_mutations = $false } | ConvertTo-Json -Depth 6 -Compress
  if (-not $pass) { exit 1 }
} catch {
  [ordered]@{ type = 'windows-recovery-parser-v1'; passed = $false; error_type = $_.Exception.GetType().FullName; step_executed = $false; explicit_file_mutations = $false } | ConvertTo-Json -Compress
  exit 1
}
`;
const command = 'powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand ' + Buffer.from(script, 'utf16le').toString('base64');
if (command.length >= 32767) throw new Error('parser command exceeds the requested Windows process-command budget');
const request = { target: 'agent:f766f09294f2e17e3b8a631ffd4e6a4b', cmd: command, cols: 200, rows: 40 };
const metadata = { workflow: workflowPath, workflow_sha256: workflowHash, step_sha256: stepHash, step_bytes: bytes.length, gzip_bytes: Buffer.from(payload, 'base64').length, command_characters: command.length, request_sha256: digest(Buffer.from(JSON.stringify(request) + '\n')), parser_execution: 'unrun', readonly_git_preflight: 'unrun; exact source HEAD/status first20/two named config keys/target existence only', step_execution: 'never by this payload', writes_on_windows: 'none explicit' };
fs.writeFileSync(outputPrefix + '.ps1', script, { flag: 'wx' });
fs.writeFileSync(outputPrefix + '.json', JSON.stringify(request) + '\n', { flag: 'wx' });
fs.writeFileSync(outputPrefix + '.meta.json', JSON.stringify(metadata, null, 2) + '\n', { flag: 'wx' });
console.log(JSON.stringify({ ...metadata, request: outputPrefix + '.json', review_script: outputPrefix + '.ps1', metadata: outputPrefix + '.meta.json' }));
