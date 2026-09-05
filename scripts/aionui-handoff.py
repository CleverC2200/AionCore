"""Verified, opt-in handoff from one completed Core platform run to AionUi."""
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import zipfile

TARGETS = {
    'macos-arm64': 'aarch64-apple-darwin', 'macos-x64': 'x86_64-apple-darwin',
    'windows-x64': 'x86_64-pc-windows-msvc', 'windows-arm64': 'aarch64-pc-windows-msvc',
}

def gh(*args, token=None):
    env = dict(os.environ)
    if token:
        env['GH_TOKEN'] = token
    return subprocess.check_output(['gh', *args], timeout=60, env=env)

def api(endpoint):
    return json.loads(gh('api', endpoint))

def create(archive, platform, ui_ref):
    if ui_ref and (platform not in TARGETS or not re.fullmatch('[a-f0-9]{40}', ui_ref)):
        raise ValueError('UI handoff requires one desktop platform and an exact UI commit')
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    record = {'schema': 1, 'repository': os.environ['GITHUB_REPOSITORY'],
              'runId': int(os.environ['GITHUB_RUN_ID']), 'attempt': int(os.environ['GITHUB_RUN_ATTEMPT']),
              'headSha': commit, 'platform': platform, 'target': TARGETS.get(platform, {'linux-x64': 'x86_64-unknown-linux-gnu', 'linux-arm64': 'aarch64-unknown-linux-gnu'}.get(platform, platform)),
              'artifact': f'aioncore-manual-{platform}', 'archive': Path(archive).name,
              'sha256': hashlib.sha256(Path(archive).read_bytes()).hexdigest(), 'uiRef': ui_ref}
    Path(archive).with_name('aioncore-manifest.json').write_text(json.dumps(record), encoding='utf-8')

def validate(run, artifact, record, archive, repository):
    if run.get('repository', {}).get('full_name') != repository or run.get('head_repository', {}).get('full_name') != repository:
        raise ValueError('repository mismatch')
    if run.get('path') != '.github/workflows/build-manual.yml' or run.get('event') != 'workflow_dispatch' or run.get('status') != 'completed' or run.get('conclusion') != 'success':
        raise ValueError('Core run is not a trusted completed manual build')
    if record.get('schema') != 1 or record.get('repository') != repository or record.get('runId') != run['id'] or record.get('attempt') != run['run_attempt'] or record.get('headSha') != run['head_sha']:
        raise ValueError('manifest provenance mismatch')
    platform = record.get('platform')
    if platform not in TARGETS or record.get('target') != TARGETS[platform]:
        raise ValueError('unsupported target')
    if artifact.get('expired') or artifact.get('workflow_run', {}).get('id') != run['id'] or artifact.get('name') != f'aioncore-manual-{platform}' or record.get('artifact') != artifact['name']:
        raise ValueError('artifact identity mismatch')
    if hashlib.sha256(archive).hexdigest() != record.get('sha256'):
        raise ValueError('inner archive checksum mismatch')
    if not re.fullmatch('[a-f0-9]{40}', record.get('uiRef', '')):
        raise ValueError('UI commit must be pinned')
    return {'branch': record['uiRef'], 'platform': platform, 'aioncore_run_id': str(run['id']),
            'aioncore_run_attempt': str(run['run_attempt']), 'aioncore_repository': repository, 'aioncore_expected_head_sha': run['head_sha'],
            'aioncore_sha256s': json.dumps({artifact['name']: record['sha256']}),
            'internal_test_build': 'false', 'skip_code_quality': 'false'}

def dispatch(run_id):
    repository = os.environ['GITHUB_REPOSITORY']
    run = api(f'repos/{repository}/actions/runs/{int(run_id)}')
    artifacts = api(f'repos/{repository}/actions/runs/{int(run_id)}/artifacts?per_page=100')['artifacts']
    # A tiny opt-in request avoids downloading every historical binary on completion.
    markers = [a for a in artifacts if a['name'] == 'aionui-handoff-request' and not a.get('expired')]
    if not markers:
        print('No opted-in UI handoff')
        return
    if len(markers) != 1 or markers[0].get('workflow_run', {}).get('id') != run['id']:
        raise ValueError('ambiguous handoff request')
    marker = markers[0]
    if marker.get('size_in_bytes', 0) > 65536:
        raise ValueError('oversized handoff request')
    blob = gh('api', f"repos/{repository}/actions/artifacts/{marker['id']}/zip")
    if marker.get('digest') != 'sha256:' + hashlib.sha256(blob).hexdigest():
        raise ValueError('request artifact digest mismatch')
    with zipfile.ZipFile(io.BytesIO(blob)) as bundle:
        if bundle.namelist() != ['aioncore-manifest.json']:
            raise ValueError('unexpected request contents')
        request = json.loads(bundle.read('aioncore-manifest.json'))
    selected = [a for a in artifacts if a['name'] == request.get('artifact') and not a.get('expired')]
    if len(selected) != 1:
        raise ValueError('requested Core artifact unavailable')
    artifact = selected[0]
    blob = gh('api', f"repos/{repository}/actions/artifacts/{artifact['id']}/zip")
    if artifact.get('digest') != 'sha256:' + hashlib.sha256(blob).hexdigest():
        raise ValueError('outer artifact digest mismatch')
    with zipfile.ZipFile(io.BytesIO(blob)) as bundle:
        if set(bundle.namelist()) != {'aioncore-manifest.json', request['archive']}:
            raise ValueError('unexpected artifact contents')
        record = json.loads(bundle.read('aioncore-manifest.json'))
        if record != request:
            raise ValueError('request does not match Core manifest')
        requests = [validate(run, artifact, record, bundle.read(record['archive']), repository)]
    if len(requests) > 1:
        raise ValueError('Handoff requires separate Core runs for each platform')
    for inputs in requests:
        title = f"Manual Build · {inputs['platform']} · {inputs['branch']} · Core {run_id}.{run['run_attempt']}"
        previous = api('repos/CleverC2200/AionUi/actions/workflows/build-manual.yml/runs?per_page=100')['workflow_runs']
        if any(item.get('display_title') == title for item in previous):
            print('Matching UI run already exists; no duplicate dispatch')
            continue
        arguments = ['workflow', 'run', 'build-manual.yml', '--repo', 'CleverC2200/AionUi', '--ref', 'main']
        for key, value in inputs.items():
            arguments.extend(['-f', f'{key}={value}'])
        # No retry on an ambiguous dispatch response. Inspect the destination run before retrying this job.
        token = os.environ.get('AIONUI_DISPATCH_TOKEN')
        if not token:
            raise ValueError('AIONUI_DISPATCH_TOKEN is required for an opted-in handoff')
        gh(*arguments, token=token)
        print('Dispatched verified UI build')

if __name__ == '__main__':
    if sys.argv[1] == 'create':
        create(sys.argv[2], sys.argv[3], os.environ.get('AIONUI_REF', ''))
    elif sys.argv[1] == 'dispatch':
        dispatch(sys.argv[2])
    else:
        raise ValueError('Expected create or dispatch')
