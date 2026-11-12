from pathlib import Path
import base64, hashlib, importlib.util, json, os, re, socket, struct, time, urllib.request

def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    item = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(item)
    return item

helper = module('credential_helper', '/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/kiln-manual-f555-delivery-20261111-d84fb07652/kiln-offline-publish-20261111.py')
token = helper.credential(Path('/home/adalundhe/.config/kiln/server.env'))
diagnostic = module('diagnostic', Path(__file__).with_name('windows-reach-diagnostic.py'))
deadline = time.monotonic() + 50
created = None
phase = "before-create"
report = {'kind': 'owned-windows-control-diagnostic', 'script_sha256': hashlib.sha256(Path(__file__).with_name('windows-reach-diagnostic.ps1').read_bytes()).hexdigest()}
raw = bytearray()
def api(method, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request('http://127.0.0.1:7070' + path, data=data, method=method, headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=10) as response:
        value = response.read()
        return json.loads(value) if value else None

try:
    before = api('GET', '/api/terminal/sessions')
    source = r"""$ErrorActionPreference='Stop'
Add-Type -TypeDefinition 'using System.Runtime.InteropServices; using Microsoft.Win32.SafeHandles; public static class KilnFlushFixture { [DllImport("kernel32.dll", SetLastError=true)] [return: MarshalAs(UnmanagedType.Bool)] public static extern bool FlushFileBuffers(SafeFileHandle h); }'
$d=Join-Path ([IO.Path]::GetTempPath()) ('kiln-epoch-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($d)
$p=Join-Path $d 'epoch.tmp'
$b=[Text.Encoding]::ASCII.GetBytes('0123456789abcdefghij')
[IO.File]::WriteAllBytes($p,$b)
@{owned_fixture=$p; bytes=20} | ConvertTo-Json -Compress
try {
 foreach($a in @([IO.FileAccess]::Read,[IO.FileAccess]::Write)) {
  $f=[IO.File]::Open($p,[IO.FileMode]::Open,$a,[IO.FileShare]::None)
  try { $ok=[KilnFlushFixture]::FlushFileBuffers($f.SafeFileHandle); $e=[Runtime.InteropServices.Marshal]::GetLastWin32Error(); @{access=$a.ToString(); flushed=$ok; error= $(if($ok){0}else{$e})} | ConvertTo-Json -Compress } finally {$f.Dispose()}
 }
} finally {
 if([Convert]::ToBase64String([IO.File]::ReadAllBytes($p)) -cne [Convert]::ToBase64String($b)){throw 'owned content changed; preserved'}
 [IO.File]::Delete($p);[IO.Directory]::Delete($d);@{cleaned=$true}|ConvertTo-Json -Compress
}
"""
    encoded = base64.b64encode(source.encode('utf-16le')).decode()
    payload = {'target':'agent:f766f09294f2e17e3b8a631ffd4e6a4b','cmd':'powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand '+encoded,'cols':240,'rows':40}
    report['kind']='minimal-owned-windows-native-flush';report['source_sha256']=hashlib.sha256(source.encode()).hexdigest()
    opened = api('POST', '/api/terminal/sessions', payload)
    created = opened['id']
    report['session'] = created
    phase = 'created'
    Path('windows-terminal-owned.json').write_text(json.dumps(report))
    key = base64.b64encode(os.urandom(16)).decode()
    expected = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
    with socket.create_connection(('127.0.0.1', 7070), timeout=10) as connection:
        request = ('GET /api/terminal/ws?session=' + created + ' HTTP/1.1\r\nHost: 127.0.0.1:7070\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: ' + key + '\r\nSec-WebSocket-Version: 13\r\nAuthorization: Bearer ' + token + '\r\n\r\n')
        connection.sendall(request.encode())
        phase = "websocket-handshake"
        buffered = bytearray()
        while b'\r\n\r\n' not in buffered:
            part = connection.recv(4096)
            if not part: raise RuntimeError('WebSocket handshake ended')
            buffered.extend(part)
            if len(buffered) > 65536: raise RuntimeError('WebSocket handshake exceeded header bound')
        head, remaining = bytes(buffered).split(b'\r\n\r\n', 1)
        if not head.startswith(b'HTTP/1.1 101') or expected.encode() not in head: raise RuntimeError('WebSocket handshake refused')
        phase = "websocket-frames"
        pending = bytearray(remaining)
        cursor_queries = 0
        def read(count):
            while len(pending) < count:
                connection.settimeout(max(0.1, deadline-time.monotonic()))
                value = connection.recv(max(4096, count-len(pending)))
                if not value: raise EOFError('terminal stream ended')
                pending.extend(value)
            value = bytes(pending[:count]);del pending[:count]
            return value
        while time.monotonic() < deadline:
            a,b = read(2);length = b & 127
            if length == 126:length = struct.unpack('!H', read(2))[0]
            if length == 127:length = struct.unpack('!Q', read(8))[0]
            if b & 128 or length > 1024*1024:raise RuntimeError('invalid bounded server frame')
            data = read(length);opcode = a & 15
            if opcode in (0,2):
                raw.extend(data)
                count = raw.count(b'\x1b[6n')
                while cursor_queries < count:
                    reply = b'\x1b[1;1R'
                    mask = os.urandom(4)
                    masked = bytes(value ^ mask[index % 4] for index, value in enumerate(reply))
                    connection.sendall(bytes([0x82, 0x80 | len(reply)]) + mask + masked)
                    cursor_queries += 1
                report['cursor_queries_answered'] = cursor_queries
            elif opcode == 1:
                frame = json.loads(data)
                report['last_control_frame'] = {'type':frame.get('type'), 'code':frame.get('code')}
                if frame.get('type') in ('exit','error','missing') or frame.get('t') in ('exit','error','missing'):
                    report['terminal_result'] = frame
                    break
            elif opcode == 8:break
            if len(raw) > 1024*1024:raise RuntimeError('diagnostic output exceeded bound')
        text = re.sub(r'\x1b\[[0-?]*[ -/]*[@-~]', '', raw.decode('utf-8', 'replace'))
        text = re.sub(r'\x1b\][^\x07]*(?:\x07|\x1b\\)', '', text).replace('\r','')
        report['output'] = text
except Exception as error:
    report['error_type'] = type(error).__name__;report['phase']=phase;report['raw_bytes']=len(raw)
finally:
    if created is not None:
        try:api('DELETE', '/api/terminal/sessions/' + created);report['owned_terminal_closed'] = True
        except Exception as error:report['cleanup_error_type'] = type(error).__name__
    if raw and 'output' not in report:report['raw_output']=raw.decode('utf-8','replace')
    report['after'] = api('GET', '/api/terminal/sessions')
    Path('windows-minimal-cursor-control-result.json').write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps(report))
