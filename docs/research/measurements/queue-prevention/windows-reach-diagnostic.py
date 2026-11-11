#!/usr/bin/env python3
"""Prepare a native-control terminal request. Never sends it or runs PowerShell."""
import argparse
import base64
import json
from pathlib import Path


def request(fixture: bool) -> dict:
    source = Path(__file__).with_suffix('.ps1').read_text(encoding='utf-8')
    flag = ' -Fixture' if fixture else ''
    script = '& {\n' + source + '\n}' + flag
    encoded = base64.b64encode(script.encode('utf-16le')).decode('ascii')
    return {
        'target': 'agent:f766f09294f2e17e3b8a631ffd4e6a4b',
        'cmd': 'powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand ' + encoded,
        'cols': 200,
        'rows': 40,
    }


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixture', action='store_true', help='include explicitly authorized private20B native flush comparison')
    args = parser.parse_args()
    print(json.dumps(request(args.fixture), separators=(',', ':')))
