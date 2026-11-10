#!/usr/bin/env python3
"""Exercise dataset hosting against an authorized isolated or live server.

Creates named acceptance fixtures. Credentials/tokens/tickets never enter the
report. Quota mutation is opt-in and must only run against an isolated org.
"""
import argparse
import concurrent.futures
import datetime
import hashlib
import io
import json
from pathlib import Path
import secrets
import struct
import time
import urllib.error
import urllib.request
import uuid
import zipfile
import zlib


def operation():
    return str(uuid.uuid4())


def png():
    def chunk(kind, data):
        return struct.pack('!I', len(data)) + kind + data + struct.pack('!I', zlib.crc32(kind + data) & 0xffffffff)
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!2I5B', 64, 48, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress((b'\0' + b'\x24\x86\xba' * 64) * 48)) + chunk(b'IEND', b'')


class Acceptance:
    def __init__(self, base, credentials):
        self.base = base.rstrip('/')
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.results = []
        self.prefix = uuid.uuid4().hex[:10]
        self.token = None
        self.credentials = credentials
        self.fixtures = {}

    def check(self, name, value):
        if not value:
            raise AssertionError(name)
        self.results.append({'name': name, 'passed': True})

    def request(self, method, path, body=None, token=True, expected=(200, 201, 202, 204), headers=None, raw=False):
        url = path if path.startswith(('http://', 'https://')) else self.base + path
        request_headers = dict(headers or {})
        if token:
            request_headers['Authorization'] = 'Bearer ' + (self.token if token is True else token)
        if body is not None and not isinstance(body, bytes):
            body = json.dumps(body, ensure_ascii=False).encode()
            request_headers.setdefault('Content-Type', 'application/json')
        request = urllib.request.Request(url, data=body, headers=request_headers, method=method)
        try:
            response = self.opener.open(request, timeout=90)
            status, data, response_headers = response.status, response.read(), dict(response.headers)
        except urllib.error.HTTPError as error:
            status, data, response_headers = error.code, error.read(), dict(error.headers)
        except (TimeoutError, urllib.error.URLError) as error:
            safe_path = '[private transfer/invitation]' if 'object-tickets' in path or '/invitations/' in path else path.split('?')[0]
            raise AssertionError(f'{method} {safe_path}: transport {type(error).__name__}') from error
        if status not in expected:
            safe_path = '[private transfer/invitation]' if 'object-tickets' in path or '/invitations/' in path else path.split('?')[0]
            raise AssertionError(f'{method} {safe_path}: received {status}, expected {expected}: {data[:500].decode(errors="replace")}')
        if raw:
            return status, data, response_headers
        return json.loads(data) if data else {}

    def login(self):
        session = self.request('POST', '/api/v1/auth/login', {'email': self.credentials['email'], 'password': self.credentials['password'], 'deviceId': operation(), 'platform': 'desktop', 'deviceName': '数据集托管验收'}, token=False)
        self.token = session['accessToken']

    def dataset(self, suffix):
        return self.request('POST', '/api/v1/datasets', {'name': f'数据集托管验收 {self.prefix} {suffix}', 'description': '自动化验收样本'})['id']

    def capture(self, dataset, project, op=None):
        return self.request('POST', f'/api/v1/datasets/{dataset}/capture', {'projectId': project, 'operationId': op or operation()})

    def version(self, dataset, name='v1', op=None):
        result = self.request('POST', f'/api/v1/datasets/{dataset}/versions', {'name': name, 'operationId': op or operation()})
        return result.get('versionId', result.get('id'))

    def wait(self, job):
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            result = self.request('GET', f'/api/v1/dataset-jobs/{job}')
            if result['status'] in ('completed', 'succeeded', 'failed', 'cancelled'):
                return result
            time.sleep(.3)
        raise AssertionError('dataset job did not finish within 90 seconds')

    def upload(self, dataset, format_name, data, op=None, expected=(200, 201, 202)):
        return self.request('POST', f'/api/v1/datasets/{dataset}/imports?format={format_name}', data, expected=expected, headers={'Content-Type': 'application/zip', 'X-Operation-Id': op or operation(), 'X-Content-SHA256': hashlib.sha256(data).hexdigest()})

    def run(self, mutate_quota):
        self.check('health', self.request('GET', '/health', token=False)['status'] == 'ok')
        self.login()
        original_quota = self.request('GET', '/api/v1/storage/quota')
        self.check('organization quota readable', original_quota['limitBytes'] >= 0 and original_quota['maxAssets'] >= 0)
        source = self.request('POST', '/api/v1/projects', {'name': f'托管源项目 {self.prefix}', 'mode': 'cloud_linked'})['id']
        self.fixtures['sourceProjectId'] = source
        self.request('PUT', f'/api/v1/projects/{source}/label-schema', {'classes': [{'id': 0, 'label': 'target', 'color': '#2486ba'}], 'baseRevision': 0})
        image_bytes = png()
        image_hash = hashlib.sha256(image_bytes).hexdigest()
        upload = self.request('POST', f'/api/v1/projects/{source}/assets/upload-session', {'clientKey': 'same-key', 'fileName': 'sample.png', 'contentHash': image_hash, 'mimeType': 'image/png', 'width': 64, 'height': 48, 'byteSize': len(image_bytes)})
        image = upload['assetId']
        self.request('PUT', upload['uploadUrl'], image_bytes, token=False, headers={'Content-Type': 'image/png'})
        self.request('POST', f'/api/v1/projects/{source}/assets/{image}/complete')
        objects = [{'id': 'original-box', 'classId': 0, 'label': 'target', 'type': 'bbox', 'bbox': {'x': 8, 'y': 6, 'width': 24, 'height': 18}, 'attributes': {}}]
        saved = self.request('PUT', f'/api/v1/projects/{source}/images/{image}/annotation', {'objects': objects, 'baseRevision': 0, 'operationId': operation()})
        self.request('POST', f'/api/v1/projects/{source}/images/{image}/submit')
        before_replay = self.request('GET', '/api/v1/storage/quota')
        completion_replay = self.request('POST', f'/api/v1/projects/{source}/assets/{image}/complete')
        after_replay = self.request('GET', '/api/v1/storage/quota')
        self.check('completion replay preserves submitted image status', completion_replay['status'] == 'pending_review')
        self.check('completion replay never double charges quota', before_replay['usedBytes'] == after_replay['usedBytes'] and before_replay['usedAssets'] == after_replay['usedAssets'])
        dataset = self.dataset('主数据集')
        self.fixtures['datasetId'] = dataset
        capture_op = operation()
        captured = self.capture(dataset, source, capture_op)
        self.check('capture includes uploaded image after submission', captured['capturedItems'] == 1)
        self.check('capture operation replay is stable', self.capture(dataset, source, capture_op) == captured)
        listing = self.request('GET', '/api/v1/datasets')['items']
        self.check('independent dataset listed', any(row['id'] == dataset and row['itemCount'] == 1 for row in listing))
        detail = self.request('GET', f'/api/v1/datasets/{dataset}')
        self.check('dataset reports stored image bytes', detail['totalBytes'] >= len(image_bytes))
        self.request('PATCH', f'/api/v1/datasets/{dataset}', {'description': '更新描述', 'baseRevision': detail['revision']})
        self.request('PATCH', f'/api/v1/datasets/{dataset}', {'description': '过期更新', 'baseRevision': detail['revision']}, expected=(409,))
        self.check('dataset revision CAS rejects stale metadata', True)
        v1_op = operation()
        version = self.version(dataset, op=v1_op)
        self.fixtures['versionId'] = version
        self.check('version operation replay stable', self.version(dataset, op=v1_op) == version)
        self.request('POST', f'/api/v1/datasets/{dataset}/versions', {'name': 'different', 'operationId': v1_op}, expected=(409,))
        self.check('operation ID cannot be reused for different version payload', True)
        frozen = self.request('GET', f'/api/v1/datasets/{dataset}/versions/{version}')
        self.check('immutable manifest contains image and annotation', frozen['itemCount'] == 1 and frozen['manifest']['items'][0]['annotation'][0]['id'] == 'original-box')
        current = self.request('GET', f'/api/v1/projects/{source}/images/{image}/annotation')
        changed = [{**objects[0], 'id': 'changed-box', 'bbox': {'x': 9, 'y': 7, 'width': 20, 'height': 16}}]
        self.request('PUT', f'/api/v1/projects/{source}/images/{image}/annotation', {'objects': changed, 'baseRevision': current['revision'], 'operationId': operation()})
        self.capture(dataset, source)
        v2 = self.version(dataset, 'v2')
        self.check('new version after recapture', v2 != version)
        self.check('old version unchanged after source edits', self.request('GET', f'/api/v1/datasets/{dataset}/versions/{version}') == frozen)
        second_source = self.request('POST', '/api/v1/projects', {'name': f'同名图源项目 {self.prefix}'})['id']
        second_upload = self.request('POST', f'/api/v1/projects/{second_source}/assets/upload-session', {'clientKey': 'same-key', 'fileName': 'sample.png', 'contentHash': image_hash, 'mimeType': 'image/png', 'width': 64, 'height': 48, 'byteSize': len(image_bytes)})
        self.request('PUT', second_upload['uploadUrl'], image_bytes, token=False, headers={'Content-Type': 'image/png'})
        self.request('POST', f'/api/v1/projects/{second_source}/assets/{second_upload["assetId"]}/complete')
        self.request('PUT', f'/api/v1/projects/{second_source}/images/{second_upload["assetId"]}/annotation', {'objects': [{**objects[0], 'id': 'other-project-box'}], 'baseRevision': 0, 'operationId': operation()})
        self.capture(dataset, second_source)
        self.check('different projects with same local image key remain separate', self.request('GET', f'/api/v1/datasets/{dataset}')['itemCount'] == 2)
        multi_version = self.version(dataset, '同名图片组合')
        multi_job = self.request('POST', f'/api/v1/datasets/{dataset}/exports', {'versionId': multi_version, 'format': 'native', 'operationId': operation()})
        self.check('same-name image export completes', self.wait(multi_job['id'])['status'] in ('completed', 'succeeded'))
        _, multi_archive, _ = self.request('GET', f'/api/v1/dataset-jobs/{multi_job["id"]}/download', raw=True)
        multi_dest = self.dataset('同名图片回导')
        multi_import = self.upload(multi_dest, 'native', multi_archive)
        self.check('same-name images roundtrip without overwrite', self.wait(multi_import['id'])['status'] in ('completed', 'succeeded') and self.request('GET', f'/api/v1/datasets/{multi_dest}')['itemCount'] == 2)
        reuse_op = operation()
        reused = self.request('POST', f'/api/v1/datasets/{dataset}/reuse', {'versionId': version, 'projectName': f'托管复用 {self.prefix}', 'operationId': reuse_op})
        target = reused['projectId']
        self.fixtures['reusedProjectId'] = target
        replay = self.request('POST', f'/api/v1/datasets/{dataset}/reuse', {'versionId': version, 'projectName': f'托管复用 {self.prefix}', 'operationId': reuse_op})
        self.check('reuse operation creates one project', replay == reused)
        bootstrap = self.request('GET', f'/api/v1/projects/{target}/sync-bootstrap')
        self.check('reused project supports existing client bootstrap', len(bootstrap['assets']) == 1)
        copied_image = bootstrap['assets'][0]['id']
        annotation = self.request('GET', f'/api/v1/projects/{target}/images/{copied_image}/annotation')
        self.check('reuse takes frozen annotation rather than current source', annotation['objects'][0]['id'] == 'original-box')
        ticket = self.request('GET', f'/api/v1/assets/{copied_image}/download-url')
        _, downloaded, _ = self.request('GET', ticket['downloadUrl'], token=False, raw=True)
        self.check('reused original bytes verified', hashlib.sha256(downloaded).hexdigest() == image_hash)
        self.request('PUT', f'/api/v1/projects/{target}/images/{copied_image}/annotation', {'objects': [], 'baseRevision': annotation['revision'], 'operationId': operation()})
        self.check('target annotation edit isolated from source', len(self.request('GET', f'/api/v1/projects/{source}/images/{image}/annotation')['objects']) == 1)
        self.check('target annotation edit isolated from version', self.request('GET', f'/api/v1/datasets/{dataset}/versions/{version}') == frozen)
        invitation = self.request('POST', f'/api/v1/projects/{source}/invitations', {'email': f'hosting-viewer-{self.prefix}@example.invalid', 'role': 'viewer'})
        viewer = self.request('POST', '/api/v1/auth/invitations/' + invitation['token'] + '/accept', {'displayName': 'Hosting Viewer', 'password': secrets.token_urlsafe(24), 'deviceId': operation(), 'platform': 'ios'}, token=False)
        viewer_token = viewer['accessToken']
        viewer_id = self.request('GET', '/api/v1/auth/me', token=viewer_token)['id']
        self.request('GET', f'/api/v1/datasets/{dataset}', token=viewer_token, expected=(403, 404))
        self.check('same-organization user needs dataset membership', True)
        self.request('POST', f'/api/v1/datasets/{dataset}/members', {'email': f'hosting-viewer-{self.prefix}@example.invalid', 'role': 'viewer'})
        self.check('dataset member can be granted by email', True)
        self.check('granted viewer reads version', self.request('GET', f'/api/v1/datasets/{dataset}/versions/{version}', token=viewer_token)['id'] == version)
        self.request('PATCH', f'/api/v1/datasets/{dataset}', {'name': 'forbidden'}, token=viewer_token, expected=(403,))
        self.request('PATCH', '/api/v1/storage/quota', {'limitBytes': original_quota['limitBytes'] + 1}, token=viewer_token, expected=(403,))
        self.check('viewer cannot mutate dataset or organization quota', True)
        archives = {}
        for format_name in ['native', 'coco', 'yolo', 'labelme']:
            job = self.request('POST', f'/api/v1/datasets/{dataset}/exports', {'versionId': version, 'format': format_name, 'operationId': operation()})
            finished = self.wait(job['id'])
            self.check(f'{format_name} export completes', finished['status'] in ('completed', 'succeeded'))
            _, archive, headers = self.request('GET', f'/api/v1/dataset-jobs/{job["id"]}/download', raw=True)
            with zipfile.ZipFile(io.BytesIO(archive)) as zipped:
                self.check(f'{format_name} ZIP CRC and frozen image count', zipped.testzip() is None and sum(name.endswith('.png') for name in zipped.namelist()) == 1)
            expected_hash = next((value for key, value in headers.items() if key.lower() == 'x-content-sha256'), None)
            self.check(f'{format_name} download hash', expected_hash == hashlib.sha256(archive).hexdigest())
            archives[format_name] = archive
            dest = self.dataset(format_name + '回导')
            import_op = operation()
            import_job = self.upload(dest, format_name, archive, import_op)
            self.check(f'{format_name} import operation replay', self.upload(dest, format_name, archive, import_op)['id'] == import_job['id'])
            finished = self.wait(import_job['id'])
            self.check(f'{format_name} import completes', finished['status'] in ('completed', 'succeeded'))
            imported = self.request('GET', f'/api/v1/datasets/{dest}')
            self.check(f'{format_name} roundtrip item count', imported['itemCount'] == 1)
            versions = self.request('GET', f'/api/v1/datasets/{dest}/versions')['items']
            self.check(f'{format_name} import creates version', len(versions) == 1)
            manifest = self.request('GET', f'/api/v1/datasets/{dest}/versions/{versions[0]["id"]}')['manifest']
            self.check(f'{format_name} roundtrip annotation geometry', manifest['items'][0]['annotation'][0]['bbox'] == objects[0]['bbox'])
        concurrent_dest = self.dataset('并发回导')
        concurrent_op = operation()
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
            duplicates = list(executor.map(lambda _: self.upload(concurrent_dest, 'native', archives['native'], concurrent_op), range(2)))
        self.check('concurrent same import operation is idempotent', duplicates[0]['id'] == duplicates[1]['id'])
        self.check('concurrent import completes once', self.wait(duplicates[0]['id'])['status'] in ('completed', 'succeeded'))
        self.upload(concurrent_dest, 'native', archives['native'] + b'changed', concurrent_op, expected=(409,))
        self.check('import operation rejects changed archive', True)
        unsafe = io.BytesIO()
        with zipfile.ZipFile(unsafe, 'w') as zipped:
            zipped.writestr('../outside.txt', 'outside')
        self.upload(concurrent_dest, 'native', unsafe.getvalue(), expected=(400, 422))
        self.check('ZIP traversal rejected', True)
        quota_before_bad = self.request('GET', '/api/v1/storage/quota')
        self.upload(concurrent_dest, 'native', b'not a ZIP', expected=(400, 422))
        self.check('bad archive leaves no quota reservation', self.request('GET', '/api/v1/storage/quota')['reservedBytes'] == quota_before_bad['reservedBytes'])
        if mutate_quota:
            try:
                q = self.request('GET', '/api/v1/storage/quota')
                self.request('PATCH', '/api/v1/storage/quota', {'limitBytes': q['limitBytes'], 'maxAssets': q['usedAssets'] + 1})
                def reuse_for_quota(_):
                    status, data, _ = self.request('POST', f'/api/v1/datasets/{dataset}/reuse', {'versionId': version, 'projectName': f'并发配额 {self.prefix}', 'operationId': operation()}, expected=(200, 201, 409), raw=True)
                    return status
                with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
                    outcomes = list(executor.map(reuse_for_quota, range(2)))
                self.check('concurrent quota permits exactly one new asset', sorted(outcomes) in ([200, 409], [201, 409]))
                q = self.request('GET', '/api/v1/storage/quota')
                self.request('PATCH', '/api/v1/storage/quota', {'limitBytes': q['usedBytes'] + q['reservedBytes'], 'maxAssets': original_quota['maxAssets']})
                self.request('POST', f'/api/v1/projects/{source}/assets/upload-session', {'clientKey': 'over-quota', 'fileName': 'denied.png', 'contentHash': image_hash, 'mimeType': 'image/png', 'width': 64, 'height': 48, 'byteSize': len(image_bytes)}, expected=(409,))
                self.check('project upload cannot bypass dataset byte quota', True)
            finally:
                self.request('PATCH', '/api/v1/storage/quota', {'limitBytes': original_quota['limitBytes'], 'maxAssets': original_quota['maxAssets']})
        self.request('DELETE', f'/api/v1/projects/{source}')
        old_job = self.request('POST', f'/api/v1/datasets/{dataset}/exports', {'versionId': version, 'format': 'native', 'operationId': operation()})
        self.check('dataset survives source project deletion', self.wait(old_job['id'])['status'] in ('completed', 'succeeded'))
        outsider_credentials = self.credentials.get('outsider')
        if outsider_credentials:
            outsider_session = self.request('POST', '/api/v1/auth/login', {'email': outsider_credentials['email'], 'password': outsider_credentials['password'], 'deviceId': operation(), 'platform': 'ios'}, token=False)
            outside_token = outsider_session['accessToken']
            self.request('GET', f'/api/v1/datasets/{dataset}', token=outside_token, expected=(403, 404))
            self.request('GET', f'/api/v1/datasets/{dataset}/versions/{version}', token=outside_token, expected=(403, 404))
            self.request('GET', f'/api/v1/dataset-jobs/{old_job["id"]}', token=outside_token, expected=(403, 404))
            self.request('GET', f'/api/v1/dataset-jobs/{old_job["id"]}/download', token=outside_token, expected=(403, 404))
            self.check('outside organization cannot read datasets versions or job downloads', True)
            self.request('POST', f'/api/v1/datasets/{dataset}/members', {'email': outsider_credentials['email'], 'role': 'viewer'}, expected=(403,))
            self.check('outside organization cannot be granted dataset membership', True)
        self.request('DELETE', f'/api/v1/datasets/{dataset}/members/{viewer_id}')
        self.request('GET', f'/api/v1/datasets/{dataset}/versions/{version}', token=viewer_token, expected=(403, 404))
        self.check('dataset membership revocation applies immediately', True)
        self.request('DELETE', f'/api/v1/datasets/{concurrent_dest}')
        self.check('archive retains dataset history', self.request('GET', f'/api/v1/datasets/{concurrent_dest}')['status'] == 'archived')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base-url', required=True)
    parser.add_argument('--credentials', type=Path, required=True)
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--isolated-quota-tests', action='store_true')
    args = parser.parse_args()
    acceptance = Acceptance(args.base_url, json.loads(args.credentials.read_text()))
    error = None
    try:
        acceptance.run(args.isolated_quota_tests)
    except Exception as caught:
        error = f'{type(caught).__name__}: {caught}'
    report = {'checkedAt': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'baseUrl': args.base_url, 'passed': error is None, 'checkCount': len(acceptance.results), 'checks': acceptance.results, 'fixtures': acceptance.fixtures, 'isolatedQuotaTests': args.isolated_quota_tests, 'error': error}
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({'passed': error is None, 'checkCount': len(acceptance.results), 'error': error}, ensure_ascii=False))
    raise SystemExit(1 if error else 0)


if __name__ == '__main__':
    main()
