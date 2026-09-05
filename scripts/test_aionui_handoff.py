"""Provenance and fail-closed tests without network calls or dispatches."""
import copy
import hashlib
import io
import json
import zipfile
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('handoff', Path(__file__).with_name('aionui-handoff.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

class HandoffTest(unittest.TestCase):
    def fixture(self):
        repo = 'owner/core'
        run = dict(repository=dict(full_name=repo), head_repository=dict(full_name=repo), path='.github/workflows/build-manual.yml', event='workflow_dispatch', status='completed', conclusion='success', id=12, run_attempt=1, head_sha='a'*40)
        artifact = dict(name='aioncore-manual-macos-arm64', workflow_run=dict(id=12), expired=False)
        record = dict(schema=1, repository=repo, runId=12, attempt=1, headSha='a'*40, platform='macos-arm64', target='aarch64-apple-darwin', artifact=artifact['name'], sha256=hashlib.sha256(b'archive').hexdigest(), uiRef='b'*40)
        return run, artifact, record, b'archive', repo

    def test_complete_platform_does_not_depend_on_other_platform(self):
        result = m.validate(*self.fixture())
        self.assertEqual(result['platform'], 'macos-arm64')
        self.assertEqual(result['skip_code_quality'], 'false')
        self.assertEqual(result['aioncore_expected_head_sha'], 'a'*40)

    def test_mismatches_fail_closed(self):
        changes = [(0, 'conclusion', 'failure'), (0, 'event', 'push'), (0, 'head_sha', 'c'*40), (2, 'attempt', 2), (2, 'target', 'wrong'), (2, 'sha256', 'wrong'), (2, 'uiRef', 'main'), (1, 'expired', True), (2, 'repository', 'fork/core')]
        for index, key, value in changes:
            with self.subTest(key=key):
                args = copy.deepcopy(self.fixture()); args[index][key] = value
                with self.assertRaises(ValueError): m.validate(*args)

    def test_non_opted_in_run_does_not_download_or_dispatch(self):
        with patch.dict(m.os.environ, {'GITHUB_REPOSITORY': 'owner/core'}), patch.object(m, 'api', side_effect=[{}, {'artifacts': []}]), patch.object(m, 'gh') as gh:
            m.dispatch('12')
            gh.assert_not_called()

    def dispatch_fixture(self):
        run, artifact, record, archive, repo = self.fixture()
        record['archive'] = 'core.tar.gz'
        def zipped(files):
            output = io.BytesIO()
            with zipfile.ZipFile(output, 'w') as z:
                for name, content in files.items(): z.writestr(name, content)
            return output.getvalue()
        manifest = json.dumps(record)
        marker_blob = zipped({'aioncore-manifest.json': manifest})
        core_blob = zipped({'aioncore-manifest.json': manifest, record['archive']: archive})
        marker = dict(id=20, name='aionui-handoff-request', workflow_run=dict(id=12), digest='sha256:'+hashlib.sha256(marker_blob).hexdigest())
        artifact.update(id=21, digest='sha256:'+hashlib.sha256(core_blob).hexdigest())
        return run, [marker, artifact], [marker_blob, core_blob]

    def test_complete_artifacts_dispatch_once_with_pinned_inputs(self):
        run, artifacts, blobs = self.dispatch_fixture()
        with patch.dict(m.os.environ, {'GITHUB_REPOSITORY': 'owner/core', 'AIONUI_DISPATCH_TOKEN': 'test-only'}), patch.object(m, 'api', side_effect=[run, {'artifacts': artifacts}, {'workflow_runs': []}]), patch.object(m, 'gh', side_effect=[*blobs, b'']) as gh:
            m.dispatch('12')
            self.assertEqual(gh.call_count, 3)
            args = gh.call_args.args
            self.assertIn('branch='+'b'*40, args)
            self.assertIn('aioncore_run_attempt=1', args)
            self.assertIn('skip_code_quality=false', args)

    def test_existing_request_is_not_dispatched_twice(self):
        run, artifacts, blobs = self.dispatch_fixture()
        title = 'Manual Build · macos-arm64 · '+'b'*40+' · Core 12.1'
        with patch.dict(m.os.environ, {'GITHUB_REPOSITORY': 'owner/core'}), patch.object(m, 'api', side_effect=[run, {'artifacts': artifacts}, {'workflow_runs': [{'display_title': title}]}]), patch.object(m, 'gh', side_effect=blobs) as gh:
            m.dispatch('12')
            self.assertEqual(gh.call_count, 2)

    def test_outer_checksum_failure_cannot_dispatch(self):
        run, artifacts, blobs = self.dispatch_fixture()
        artifacts[1]['digest'] = 'sha256:'+'0'*64
        with patch.dict(m.os.environ, {'GITHUB_REPOSITORY': 'owner/core'}), patch.object(m, 'api', side_effect=[run, {'artifacts': artifacts}]), patch.object(m, 'gh', side_effect=blobs) as gh:
            with self.assertRaisesRegex(ValueError, 'outer artifact digest'):
                m.dispatch('12')
            self.assertEqual(gh.call_count, 2)

if __name__ == '__main__':
    unittest.main()
