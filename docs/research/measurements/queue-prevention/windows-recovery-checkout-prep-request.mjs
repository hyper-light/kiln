#!/usr/bin/env node
// Produce reviewable owned-terminal payloads only; no terminal/API invocation.
import fs from 'node:fs';
import crypto from 'node:crypto';
import { gzipSync } from 'node:zlib';
const [scriptPath, prefix, kind = 'prep'] = process.argv.slice(2);
if (!scriptPath || !prefix || !['prep', 'inspect'].includes(kind) || ![4, 5].includes(process.argv.length)) throw new Error('usage: node windows-recovery-checkout-prep-request.mjs SCRIPT OUTPUT_PREFIX [prep|inspect]');
const bytes = fs.readFileSync(scriptPath);
const sha = value => crypto.createHash('sha256').update(value).digest('hex');
const hash = sha(bytes);
const packed = gzipSync(bytes).toString('base64');
const decode = String.raw`$ErrorActionPreference = 'Stop'
try {
  $inputFile = [IO.MemoryStream]::new([byte[]][Convert]::FromBase64String('${packed}'))
  $gzip = [IO.Compression.GZipStream]::new($inputFile, [IO.Compression.CompressionMode]::Decompress)
  $outputFile = [IO.MemoryStream]::new()
  try { $gzip.CopyTo($outputFile); $bytes = $outputFile.ToArray() }
  finally { $gzip.Dispose(); $inputFile.Dispose(); $outputFile.Dispose() }
  if ($bytes.Length -ne ${bytes.length}) { throw 'checkout prep script length mismatch' }
  $hasher = [Security.Cryptography.SHA256]::Create()
  try { $hash = [BitConverter]::ToString($hasher.ComputeHash($bytes)).Replace('-', '').ToLowerInvariant() }
  finally { $hasher.Dispose() }
  if ($hash -cne '${hash}') { throw 'checkout prep script hash mismatch' }
  $source = [Text.Encoding]::UTF8.GetString($bytes)
  $tokens = $null; $errors = $null
  $ast = [Management.Automation.Language.Parser]::ParseInput($source, [ref]$tokens, [ref]$errors)
  $issues = @($errors | ForEach-Object { [ordered]@{ id = $_.ErrorId; line = $_.Extent.StartLineNumber; column = $_.Extent.StartColumnNumber; message = $_.Message } })
`;
const modes = {
  parser: String.raw`  [ordered]@{ type = 'windows-recovery-checkout-parser-v1'; passed = $issues.Count -eq 0; script_sha256 = $hash; parser_errors = $issues; executed = $false } | ConvertTo-Json -Depth 5 -Compress
  if ($issues.Count -ne 0) { exit 1 }
`,
};
if (kind === 'inspect') modes.inspect = String.raw`  if ($issues.Count -ne 0) { throw 'checkout inspection script parser refused' }
  & ([ScriptBlock]::Create($source))
`;
else modes.apply = String.raw`  if ($issues.Count -ne 0) { throw 'checkout prep script parser refused' }
  & ([ScriptBlock]::Create($source)) -Apply
`;
const metadata = { script: scriptPath, kind, script_sha256: hash, script_bytes: bytes.length, gzip_bytes: Buffer.from(packed, 'base64').length, native_execution: 'unrun; Root owns execution/cleanup', requests: {} };
for (const [mode, body] of Object.entries(modes)) {
  const script = decode + body + String.raw`} catch {
  [ordered]@{ type = 'windows-recovery-checkout-launcher-v1'; passed = $false; error_type = $_.Exception.GetType().FullName } | ConvertTo-Json -Compress
  exit 1
}
`;
  const command = 'powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand ' + Buffer.from(script, 'utf16le').toString('base64');
  if (command.length >= 32767) throw new Error('owned terminal command exceeds Windows process-command limit');
  const request = JSON.stringify({ target: 'agent:f766f09294f2e17e3b8a631ffd4e6a4b', cmd: command, cols: 200, rows: 40 }) + '\n';
  fs.writeFileSync(prefix + '.' + mode + '.json', request, { flag: 'wx' });
  metadata.requests[mode] = { path: prefix + '.' + mode + '.json', sha256: sha(request), command_characters: command.length };
}
fs.writeFileSync(prefix + '.meta.json', JSON.stringify(metadata, null, 2) + '\n', { flag: 'wx' });
console.log(JSON.stringify(metadata));
