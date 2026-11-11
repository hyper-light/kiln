#!/usr/bin/env python3
"""Coordinator-only preflight using the shipped Kiln hook; never signs or triggers a build."""
import argparse
import base64
import datetime
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import ssl
import subprocess
from urllib.parse import urlsplit
from urllib.request import Request, urlopen


METHOD = 'kiln.__git-hook.update'
RECEIPT_KIND = 'kiln-coordinator-commit-verdict-v1'
BUNDLE = Path('/home/adalundhe/.kiln/reach/e30189ea29182644fe5a4b2e06c7dac2/kiln-manual-f555-delivery-20261111-d84fb07652')


class Refused(Exception):
    pass


def write_new(path, data):
    with path.open('xb') as file:
        os.chmod(path, 0o600)
        file.write(data)
        file.flush()
        os.fsync(file.fileno())


def json_bytes(value):
    return (json.dumps(value, sort_keys=True, separators=(',', ':')) + '\n').encode()


def file_hash(path):
    digest = hashlib.sha256()
    with path.open('rb') as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def run(args):
    output = args.output.resolve()
    output.mkdir(mode=0o700)
    source = args.source_repo.resolve(strict=True)
    kiln = args.kiln.resolve(strict=True)
    if file_hash(kiln) != args.kiln_sha256:
        raise Refused('ShippedBinaryMismatch')
    env = os.environ.copy()
    # The view must not inherit an original repository's refs/quarantine namespace.
    for key in ['GIT_DIR', 'GIT_COMMON_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_OBJECT_DIRECTORY', 'GIT_ALTERNATE_OBJECT_DIRECTORIES', 'GIT_QUARANTINE_PATH', 'GIT_NAMESPACE']:
        env.pop(key, None)
    env['GIT_NO_REPLACE_OBJECTS'] = '1'

    def git(*words, cwd=source, data=None):
        result = subprocess.run(['git', *words], cwd=cwd, env=env, input=data, capture_output=True, timeout=30)
        if result.returncode:
            raise Refused('GitReadOrScratchOperationFailed')
        return result.stdout

    raw = git('cat-file', 'commit', args.sha)
    if git('hash-object', '-t', 'commit', '--stdin', data=raw).decode().strip() != args.sha:
        raise Refused('GitObjectMismatch')
    parents = [line[7:].decode('ascii') for line in raw.split(b'\n\n', 1)[0].splitlines() if line.startswith(b'parent ')]
    if any(not re.fullmatch('[0-9a-f]{40}', parent) for parent in parents):
        raise Refused('InvalidParent')
    common = Path(git('rev-parse', '--git-common-dir').decode().strip())
    objects = ((source / common) if not common.is_absolute() else common).resolve() / 'objects'
    if not objects.is_dir():
        raise Refused('MissingObjectDirectory')

    helper_spec = importlib.util.spec_from_file_location('kiln_publish_helper', BUNDLE / 'kiln-offline-publish-20261111.py')
    helper = importlib.util.module_from_spec(helper_spec)
    helper_spec.loader.exec_module(helper)
    token = helper.credential(Path('/home/adalundhe/.config/kiln/server.env'))

    def get(path):
        request = Request('http://127.0.0.1:7070' + path, headers={'Authorization': 'Bearer ' + token})
        with urlopen(request, timeout=15) as response:
            if response.status != 200:
                raise Refused('MetadataRefused')
            return response.read()

    ca = ssl.PEM_cert_to_DER_cert(get('/ca.pem').decode('ascii'))
    if hashlib.sha256(ca).hexdigest() != args.root_sha256:
        raise Refused('TrustedRootMismatch')
    revocation_bytes = get('/api/identities/revocations')
    revocations = json.loads(revocation_bytes)
    if not isinstance(revocations, dict) or not isinstance(revocations.get('revocations'), list):
        raise Refused('CompleteRevocationsMissing')
    rows = revocations['revocations']
    if any(not isinstance(row, dict) or not all(key in row for key in ['serial', 'key', 'at', 'reason', 'by']) for row in rows):
        raise Refused('RevocationRecordIncomplete')
    # This is a coordinator's strict CMS-agent check, not a claimed upstream GitHub push verdict.
    policy = {'mode': 'required', 'signers': [{'refs': ['main'], 'by': ['agents']}]}
    context = {'org': args.org, 'root': base64.b64encode(ca).decode(), 'revocations': rows, 'policy': policy, 'required': True, 'agent_for': None, 'ssh': []}
    context_dir = output / 'hook-context'
    context_dir.mkdir(mode=0o700)
    (context_dir / 'verdicts').mkdir(mode=0o700)
    write_new(context_dir / 'context.json', json_bytes(context))
    write_new(output / 'revocations.json', revocation_bytes)

    view = output / 'parent-only.git'
    git('init', '--bare', '--quiet', str(view))
    write_new(view / 'objects' / 'info' / 'alternates', (str(objects) + '\n').encode())
    for index, parent in enumerate(parents):
        git('--git-dir', str(view), 'update-ref', 'refs/checked-parents/' + str(index), parent)
    listed = git('--git-dir', str(view), 'rev-list', args.sha, '--not', '--all').decode().splitlines()
    if listed != [args.sha]:
        raise Refused('TargetMustBeTheOnlyUnreferencedCommit')
    hook_env = env.copy()
    hook_env['GIT_DIR'] = str(view)
    started = datetime.datetime.now(datetime.timezone.utc).isoformat()
    hook = subprocess.run([str(kiln), '__git-hook', 'update', str(context_dir), 'refs/heads/main', parents[0] if parents else '0' * 40, args.sha], cwd=view, env=hook_env, capture_output=True, timeout=30)
    completed = datetime.datetime.now(datetime.timezone.utc).isoformat()
    write_new(output / 'hook.stderr', hook.stderr)
    write_new(output / 'hook.stdout', hook.stdout)
    files = list((context_dir / 'verdicts').glob('*.json'))
    if len(files) != 1:
        raise Refused('OneVerdictRequired')
    verdict = json.loads(files[0].read_bytes())
    write_new(output / 'hook-verdict.json', json_bytes(verdict))
    commits = verdict.get('commits')
    if hook.returncode or verdict.get('refname') != 'refs/heads/main' or verdict.get('refused') is not None or verdict.get('warnings') != [] or not isinstance(commits, list) or len(commits) != 1:
        raise Refused('HookRefusedOrIncomplete')
    found = commits[0]
    signer = found.get('words')
    if found.get('sha') != args.sha or found.get('seen') != 'by' or not re.fullmatch('[0-9a-f]{64}', found.get('signature') or '') or not isinstance(signer, str):
        raise Refused('TargetSignatureNotVerified')
    identity = urlsplit(signer)
    parts = identity.path.strip('/').split('/')
    # Core already parsed/verified this URI; bind its actual agent to the authorized signer.
    if identity.scheme != 'kiln' or identity.netloc != args.org or len(parts) not in [6, 10] or parts[0] != 'person' or parts[2] != 'agent' or parts[3] != args.agent or parts[4] != 'epoch':
        raise Refused('UnexpectedVerifiedSigner')
    receipt = {'kind': RECEIPT_KIND, 'method': METHOD, 'passed': True, 'repository': 'hyper-light/kiln', 'source_sha': args.sha, 'org': args.org, 'signer': signer, 'expected_agent': args.agent, 'root_sha256': args.root_sha256, 'revocations_sha256': hashlib.sha256(revocation_bytes).hexdigest(), 'revocation_count': len(rows), 'kiln_sha256': args.kiln_sha256, 'checked_started_at': started, 'checked_at': completed, 'hook_verdict': verdict}
    encoded = json_bytes(receipt)
    if len(encoded) > 512 * 1024:
        raise Refused('ReceiptExceedsExistingControlFrame')
    receipt_path = output / ('commit-verdict-' + args.sha + '.json')
    write_new(receipt_path, encoded)
    return {'passed': True, 'source_sha': args.sha, 'signer': signer, 'org': args.org, 'method': METHOD, 'receipt': str(receipt_path), 'receipt_sha256': hashlib.sha256(encoded).hexdigest(), 'context_retained_privately': str(output)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source-repo', type=Path, required=True)
    parser.add_argument('--sha', required=True)
    parser.add_argument('--agent', required=True)
    parser.add_argument('--org', required=True)
    parser.add_argument('--root-sha256', required=True, help='existing trusted DER root fingerprint; never TOFU')
    parser.add_argument('--kiln', type=Path, required=True)
    parser.add_argument('--kiln-sha256', required=True, help='existing trusted shipped binary digest')
    parser.add_argument('--output', type=Path, required=True, help='new task-private evidence directory; must not exist')
    args = parser.parse_args()
    if not re.fullmatch('[0-9a-f]{40}', args.sha) or not re.fullmatch('[0-9a-f]{32}', args.agent) or any(not re.fullmatch('[0-9a-f]{64}', value) for value in [args.root_sha256, args.kiln_sha256]):
        parser.error('exact lowercase commit/root/binary digests required')
    try:
        print(json.dumps(run(args), separators=(',', ':')))
        return 0
    except Exception as error:
        # Context may contain public certificate/revocation material; keep it out of normal output.
        print(json.dumps({'passed': False, 'error_type': type(error).__name__, 'code': str(error) if isinstance(error, Refused) else None, 'evidence': str(args.output)}, separators=(',', ':')))
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
