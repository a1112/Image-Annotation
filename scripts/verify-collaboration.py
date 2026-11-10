#!/usr/bin/env python3
"""Exercise the deployed collaboration API without printing credentials or tickets.

Credentials JSON: {"email": ..., "password": ..., "outsider": {"email": ..., "password": ...}}
The fixture project is retained for desktop/mobile acceptance. Use an isolated
staging database/bucket first; this script creates explicitly named test data.
"""
import argparse
import concurrent.futures
import datetime
import hashlib
import json
from pathlib import Path
import secrets
import struct
import time
import urllib.error
import urllib.request
import uuid
import zlib


def png_fixture():
    width, height = 640, 480
    rows = []
    for y in range(height):
        row = bytearray()
        for x in range(width):
            if 130 < x < 420 and 110 < y < 310:
                color = (54, 114, 224)
            elif (x - 440) ** 2 + (y - 340) ** 2 < 60 ** 2:
                color = (238, 145, 63)
            else:
                color = (238, 242, 247)
            row.extend(color)
        rows.append(b'\0' + row)
    def chunk(kind, data):
        return struct.pack('!I', len(data)) + kind + data + struct.pack('!I', zlib.crc32(kind + data) & 0xffffffff)
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('!2I5B', width, height, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(b''.join(rows))) + chunk(b'IEND', b'')


class Checks:
    def __init__(self, base):
        self.base = base.rstrip('/')
        self.results = []
        self.prefix = uuid.uuid4().hex[:12]
    def check(self, name, condition):
        if not condition:
            raise AssertionError(name)
        self.results.append({'name': name, 'passed': True})
    def request(self, method, path, body=None, token=None, expected=(200, 201), raw=False):
        url = path if path.startswith(('http://', 'https://')) else self.base + path
        headers = {}
        if token:
            headers['Authorization'] = 'Bearer ' + token
        if body is not None:
            if isinstance(body, bytes):
                headers['Content-Type'] = 'image/png'
            else:
                body = json.dumps(body, ensure_ascii=False).encode()
                headers['Content-Type'] = 'application/json'
        request = urllib.request.Request(url, data=body, headers=headers, method=method)
        try:
            response = urllib.request.urlopen(request, timeout=60)
            status, payload = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, payload = error.code, error.read()
        if status not in expected:
            safe_path = path.split('?')[0]
            if 'object-tickets' in safe_path or '/invitations/' in safe_path:
                safe_path = '[private transfer/invitation route]'
            raise AssertionError(f'{method} {safe_path}: expected {expected}, received {status}: {payload[:300].decode(errors="replace")}')
        if raw:
            return status, payload
        if not payload:
            return {}
        try:
            return json.loads(payload)
        except json.JSONDecodeError:
            if status >= 400:
                return {'error': payload.decode(errors='replace')}
            raise
    def login(self, credentials, platform):
        device = str(uuid.uuid4())
        session = self.request('POST', '/api/v1/auth/login', {
            'email': credentials['email'], 'password': credentials['password'],
            'deviceId': device, 'platform': platform, 'deviceName': '协作验收 ' + platform,
        })
        return session, device
    def push(self, token, device, project, operation):
        response = self.request('POST', '/api/v1/sync/push', {'deviceId': device, 'projectId': project, 'operations': [operation]}, token)
        return response['results'][0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base-url', required=True)
    parser.add_argument('--credentials', type=Path, required=True)
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    credentials = json.loads(args.credentials.read_text())
    checks = Checks(args.base_url)
    start = time.monotonic()
    health = checks.request('GET', '/health')
    checks.check('health', health['status'] == 'ok')
    admin, desktop_device = checks.login(credentials, 'desktop')
    admin_token = admin['accessToken']
    checks.request('POST', '/api/v1/auth/login', {'email': credentials['email'], 'password': 'invalid-' + secrets.token_urlsafe(12), 'deviceId': str(uuid.uuid4()), 'platform': 'ios'}, expected=(401,))
    checks.check('invalid password rejected', True)
    me = checks.request('GET', '/api/v1/auth/me', token=admin_token)
    checks.check('session identity and devices', bool(me['id']) and bool(me['devices']))
    project = checks.request('POST', '/api/v1/projects', {'name': '移动协作验收 ' + checks.prefix, 'description': '自动化验收样本，可在桌面和移动端打开。', 'mode': 'cloud_linked'}, admin_token)['id']
    schema = checks.request('PUT', f'/api/v1/projects/{project}/label-schema', {'classes': [{'id': 0, 'label': '目标', 'color': '#3672e0'}], 'baseRevision': 0}, admin_token)
    checks.check('published label schema', bool(schema))
    image_bytes = png_fixture()
    content_hash = hashlib.sha256(image_bytes).hexdigest()
    upload = checks.request('POST', f'/api/v1/projects/{project}/assets/upload-session', {'clientKey': 'acceptance-image', 'fileName': '协作样本.png', 'contentHash': content_hash, 'mimeType': 'image/png', 'width': 640, 'height': 480, 'byteSize': len(image_bytes)}, admin_token)
    image = upload['assetId']
    checks.request('PUT', upload['uploadUrl'], image_bytes, expected=(200,201,204), raw=True)
    checks.request('POST', f'/api/v1/projects/{project}/assets/{image}/complete', token=admin_token)
    download = checks.request('GET', f'/api/v1/assets/{image}/download-url', token=admin_token)
    _, downloaded = checks.request('GET', download['downloadUrl'], raw=True)
    checks.check('private storage ticket preserves bytes', hashlib.sha256(downloaded).hexdigest() == content_hash)
    checks.check('transfer URL is externally usable', download['downloadUrl'].startswith(args.base_url.rstrip('/')))
    thumbnail = checks.request('GET', f'/api/v1/assets/{image}/download-url?variant=thumb', token=admin_token)
    _, thumbnail_bytes = checks.request('GET', thumbnail['downloadUrl'], raw=True)
    checks.check('thumbnail has independent verified metadata',
                 thumbnail['mimeType'] == 'image/jpeg'
                 and thumbnail['contentHash'] != content_hash
                 and hashlib.sha256(thumbnail_bytes).hexdigest() == thumbnail['contentHash']
                 and len(thumbnail_bytes) == thumbnail['byteSize']
                 and thumbnail_bytes.startswith(b'\xff\xd8'))
    members = {}
    for role, platform in [('annotator', 'android'), ('annotator2', 'ios'), ('reviewer', 'ios'), ('viewer', 'web')]:
        invitation = checks.request('POST', f'/api/v1/projects/{project}/invitations', {'email': f'{role}-{checks.prefix}@example.invalid', 'role': 'annotator' if role == 'annotator2' else role}, admin_token)
        device = str(uuid.uuid4())
        accept_path = '/api/v1/auth/invitations/' + invitation['token'] + '/accept'
        accept_body = {'displayName': '验收 ' + role, 'password': secrets.token_urlsafe(24), 'deviceId': device, 'platform': platform, 'deviceName': role}
        session = checks.request('POST', accept_path, accept_body)
        members[role] = (session['accessToken'], device)
        checks.request('POST', accept_path, accept_body, expected=(400,404,409,410))
    checks.check('role invitations are single use', True)
    mobile_token, mobile_device = members['annotator']
    reviewer_token, reviewer_device = members['reviewer']
    viewer_token, _ = members['viewer']
    task = checks.request('POST', f'/api/v1/projects/{project}/tasks', {'name': '移动标注与审阅', 'status': 'active', 'imageIds': [image]}, admin_token)['id']
    checks.request('POST', f'/api/v1/tasks/{task}/items/{image}/claim', token=mobile_token)
    checks.request('POST', f'/api/v1/tasks/{task}/items/{image}/claim', token=members['annotator2'][0], expected=(409,))
    checks.check('task claim excludes another annotator', True)
    items = checks.request('GET', f'/api/v1/tasks/{task}/items', token=mobile_token)['items']
    checks.check('mobile task claim', len(items) == 1 and bool(items[0]['assigneeId']))
    def objects(name, x=130):
        return [{'id': name, 'classId': 0, 'label': '目标', 'type': 'bbox', 'bbox': {'x': x, 'y': 110, 'width': 290, 'height': 200}, 'attributes': {}}]
    operation = {'operationId': str(uuid.uuid4()), 'entityType': 'annotation', 'entityId': image, 'operation': 'update', 'baseRevision': 0, 'payload': {'imageId': image, 'objects': objects('box-1')}}
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        duplicates = list(executor.map(lambda _: checks.push(mobile_token, mobile_device, project, operation), range(2)))
    for index, result in enumerate(duplicates):
        if result['status'] == 'retryable':
            duplicates[index] = checks.push(mobile_token, mobile_device, project, operation)
    checks.check('concurrent operation id is idempotent', all(result['status'] in ('applied','duplicate') for result in duplicates))
    versions = checks.request('GET', f'/api/v1/projects/{project}/images/{image}/annotation/versions', token=admin_token)
    version_items = versions.get('items', versions.get('versions', [])) if isinstance(versions,dict) else versions
    checks.check('duplicate creates one annotation version', len(version_items) == 1)
    first_revision = duplicates[0]['serverRevision']
    newer = {**operation, 'operationId': str(uuid.uuid4()), 'baseRevision': first_revision, 'payload': {'imageId': image, 'objects': objects('desktop', 140)}}
    accepted = checks.push(admin_token, desktop_device, project, newer)
    stale = {**operation, 'operationId': str(uuid.uuid4()), 'baseRevision': first_revision, 'payload': {'imageId': image, 'objects': objects('offline-mobile', 150)}}
    conflict = checks.push(mobile_token, mobile_device, project, stale)
    checks.check('stale offline edit is visible conflict', conflict['status'] == 'conflict' and bool(conflict.get('remotePayload')))
    resolved = checks.push(mobile_token, mobile_device, project, {**stale, 'operationId': str(uuid.uuid4()), 'baseRevision': accepted['serverRevision']})
    checks.check('explicit resolution uses latest revision', resolved['status'] == 'applied')
    submit = checks.push(mobile_token,mobile_device,project,{'operationId':str(uuid.uuid4()),'entityType':'annotation','entityId':image,'operation':'submit','baseRevision':resolved['serverRevision'],'payload':{'imageId':image}})
    checks.check('mobile submits saved annotation', submit['status'] == 'applied')
    queue = checks.request('GET', f'/api/v1/projects/{project}/review-queue', token=reviewer_token)['items']
    checks.check('second device sees review queue', any(item['imageId'] == image for item in queue))
    current = checks.request('GET', f'/api/v1/projects/{project}/images/{image}/annotation', token=reviewer_token)
    checks.request('POST',f'/api/v1/projects/{project}/images/{image}/review',{'decision':'approved','annotationRevision':0,'operationId':str(uuid.uuid4())},reviewer_token,expected=(409,))
    checks.request('POST',f'/api/v1/projects/{project}/images/{image}/review',{'decision':'approved','annotationRevision':current['revision'],'operationId':str(uuid.uuid4()),'note':'跨设备验收通过'},reviewer_token)
    final = checks.request('GET', f'/api/v1/projects/{project}/images/{image}/annotation', token=admin_token)
    checks.check('review is revision checked and returns to desktop', final['status'] == 'approved')
    issue_key = 'issue-' + checks.prefix
    issue = checks.push(mobile_token, mobile_device, project, {'operationId': str(uuid.uuid4()), 'entityType': 'issue', 'entityId': issue_key, 'operation': 'create', 'payload': {'imageId': image, 'title': '移动端发现缺陷', 'description': '离线缺陷同步验收', 'severity': 'minor', 'source': 'manual'}})
    checks.check('annotator creates an issue', issue['status'] == 'applied')
    issue_id = issue['remotePayload']['id']
    comment_key = 'comment-' + checks.prefix
    comment_op = {'operationId': str(uuid.uuid4()), 'entityType': 'issue_comment', 'entityId': comment_key, 'operation': 'comment', 'payload': {'issueId': issue_id, 'content': '断网重试后只出现一次 ' + checks.prefix}}
    for _ in range(2):
        result = checks.push(mobile_token, mobile_device, project, comment_op)
        checks.check('comment retry accepted ' + str(_), result['status'] in ('applied', 'duplicate'))
    checks.request('POST', f'/api/v1/issues/{issue_id}/comments', {'content': 'viewer cannot write'}, viewer_token, expected=(403,))
    issue_snapshot = checks.request('GET', f'/api/v1/projects/{project}/sync-bootstrap', token=mobile_token)
    checks.check('append only comment is not duplicated', len([item for item in issue_snapshot['issueComments'] if item.get('clientKey') == comment_key]) == 1)
    folder_key = 'folder-' + checks.prefix
    folder = checks.push(admin_token, desktop_device, project, {'operationId': str(uuid.uuid4()), 'entityType': 'folder', 'entityId': folder_key, 'operation': 'create', 'payload': {'name': '删除同步验收'}})
    checks.check('folder created for deletion sync', folder['status'] == 'applied')
    folder_id = folder['remotePayload']['id']
    deleted = checks.push(admin_token, desktop_device, project, {'operationId': str(uuid.uuid4()), 'entityType': 'folder', 'entityId': folder_id, 'operation': 'delete', 'baseRevision': folder['serverRevision'], 'payload': {}})
    checks.check('folder deletion produces tombstone', deleted['status'] == 'applied')
    reject_key = 'qa-issue-' + checks.prefix
    changed = checks.push(mobile_token, mobile_device, project, {**operation, 'operationId': str(uuid.uuid4()), 'baseRevision': final['revision'], 'payload': {'imageId': image, 'objects': objects('review-rework', 155)}})
    resubmitted = checks.push(mobile_token, mobile_device, project, {'operationId': str(uuid.uuid4()), 'entityType': 'annotation', 'entityId': image, 'operation': 'submit', 'baseRevision': changed['serverRevision'], 'payload': {'imageId': image}})
    rejected = checks.push(reviewer_token, reviewer_device, project, {'operationId': str(uuid.uuid4()), 'entityType': 'qa_review', 'entityId': image, 'operation': 'transition', 'baseRevision': resubmitted['serverRevision'], 'payload': {'imageId': image, 'decision': 'rejected', 'note': '需要修正', 'issueClientKey': reject_key}})
    checks.check('queued review uses accepted annotation revision', rejected['status'] == 'applied')
    followup_issue = checks.push(mobile_token, mobile_device, project, {'operationId': str(uuid.uuid4()), 'entityType': 'issue', 'entityId': reject_key, 'operation': 'create', 'payload': {'imageId': image, 'title': '质检驳回', 'description': '需要修正', 'severity': 'major', 'source': 'qa_review'}})
    checks.check('client and server share rejection issue identity', followup_issue['status'] == 'applied')
    rejection_snapshot = checks.request('GET', f'/api/v1/projects/{project}/sync-bootstrap', token=mobile_token)
    checks.check('rejection creates one issue across devices', len([item for item in rejection_snapshot['issues'] if item.get('clientKey') == reject_key]) == 1)
    for status in ['in_progress','resolved','pending_review','closed']:
        detail=checks.request('GET', f'/api/v1/issues/{issue_id}', token=reviewer_token)
        checks.request('POST', f'/api/v1/issues/{issue_id}/transition', {'status':status,'baseRevision':detail['revision']}, reviewer_token)
    checks.check('issue follows review lifecycle', True)
    reworked=checks.push(mobile_token,mobile_device,project,{**operation,'operationId':str(uuid.uuid4()),'baseRevision':rejected['serverRevision'],'payload':{'imageId':image,'objects':objects('final-approved',155)}})
    resubmitted=checks.push(mobile_token,mobile_device,project,{'operationId':str(uuid.uuid4()),'entityType':'annotation','entityId':image,'operation':'submit','baseRevision':reworked['serverRevision'],'payload':{'imageId':image}})
    checks.request('POST',f'/api/v1/projects/{project}/images/{image}/review',{'decision':'approved','annotationRevision':resubmitted['serverRevision'],'operationId':str(uuid.uuid4())},reviewer_token)
    final=checks.request('GET',f'/api/v1/projects/{project}/images/{image}/annotation',token=admin_token)
    checks.request('PUT',f'/api/v1/projects/{project}/images/{image}/annotation',{'objects':objects('forbidden'),'baseRevision':final['revision']},viewer_token,expected=(403,))
    checks.request('POST',f'/api/v1/projects/{project}/invitations',{'email':'forbidden@example.invalid','role':'manager'},mobile_token,expected=(403,))
    checks.check('viewer and annotator cannot exceed roles',True)
    snapshot = checks.request('GET',f'/api/v1/projects/{project}/sync-bootstrap',token=mobile_token)
    checks.check('bootstrap includes tasks reviews and schemas', all(key in snapshot for key in ['tasks','taskItems','qaReviews','labelSchemas']))
    changes = checks.request('GET', f'/api/v1/projects/{project}/changes?cursor=0&limit=500', token=mobile_token)['changes']
    sequences = [item['sequence'] for item in changes]
    checks.check('changes are ordered and contain review', sequences == sorted(set(sequences)) and any(item['entityType']=='qa_review' for item in changes))
    disposable = checks.request('POST', '/api/v1/projects', {'name': '删除项目验收 ' + checks.prefix, 'mode': 'cloud_linked'}, admin_token)['id']
    checks.request('DELETE', f'/api/v1/projects/{disposable}', token=admin_token)
    deleted_changes = checks.request('GET', f'/api/v1/projects/{disposable}/changes?cursor=0', token=admin_token)['changes']
    checks.check('existing member can receive project deletion', any(item['entityType'] == 'project' and item['operation'] == 'delete' for item in deleted_changes))
    rotated = checks.request('POST','/api/v1/auth/refresh',{'refreshToken':admin['refreshToken'],'deviceId':desktop_device})
    checks.request('POST','/api/v1/auth/refresh',{'refreshToken':admin['refreshToken'],'deviceId':desktop_device},expected=(401,))
    checks.check('refresh token rotates once', bool(rotated['accessToken']))
    extra, extra_device = checks.login(credentials,'ios')
    current_me=checks.request('GET','/api/v1/auth/me',token=rotated['accessToken'])
    device_row=next(row for row in current_me['devices'] if row['deviceId']==extra_device)
    checks.request('DELETE','/api/v1/auth/devices/'+device_row['id'],token=rotated['accessToken'])
    checks.request('GET','/api/v1/auth/me',token=extra['accessToken'],expected=(401,))
    checks.check('device revocation invalidates existing access',True)
    if credentials.get('outsider'):
        outsider,_=checks.login(credentials['outsider'],'web')
        checks.request('GET',f'/api/v1/projects/{project}',token=outsider['accessToken'],expected=(403,404))
        checks.check('cross organization access rejected',True)
    report={'time':datetime.datetime.now(datetime.timezone.utc).isoformat(),'baseUrl':args.base_url,'projectId':project,'imageId':image,'taskId':task,'seconds':round(time.monotonic()-start,2),'checks':checks.results,'passed':True}
    args.report.parent.mkdir(parents=True,exist_ok=True)
    args.report.write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps({'passed':True,'checks':len(checks.results),'projectId':project,'report':str(args.report)},ensure_ascii=False))

if __name__=='__main__':
    main()
