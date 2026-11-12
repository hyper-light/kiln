#!/usr/bin/env python3
"""Root-run pure API validation of the final workflow; never triggers a run."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
from urllib.request import Request, urlopen


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--file', type=Path, required=True)
    args = parser.parse_args()
    source = args.file.read_bytes()
    bundle = Path('/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/kiln-manual-f555-delivery-20261111-d84fb07652')
    spec = importlib.util.spec_from_file_location('kiln_publish_helper', bundle / 'kiln-offline-publish-20261111.py')
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    token = helper.credential(Path('/home/adalundhe/.config/kiln/server.env'))
    ask = {'path': '.kiln/windows-recovery.yaml', 'content': source.decode('utf-8'), 'dialect': 'kiln'}
    request = Request('http://127.0.0.1:7070/api/ci/validate', data=json.dumps(ask).encode(), headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    with urlopen(request, timeout=30) as response:
        status = response.status
        checked = json.load(response)
    diagnostics = checked.get('diagnostics')
    passed = status == 200 and checked.get('kind') == 'workflow' and checked.get('dialect') == 'kiln' and diagnostics == []
    print(json.dumps({'passed': passed, 'source_sha256': hashlib.sha256(source).hexdigest(), 'http': status, 'kind': checked.get('kind'), 'dialect': checked.get('dialect'), 'diagnostics': diagnostics, 'outline': checked.get('outline'), 'runs_started': False, 'configuration_changed': False}, separators=(',', ':')))
    return 0 if passed else 1


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except Exception as error:
        print(json.dumps({'passed': False, 'error_type': type(error).__name__, 'runs_started': False, 'configuration_changed': False}, separators=(',', ':')))
        raise SystemExit(1)
