#!/usr/bin/env python3
"""Verify dataset-group access using two existing accounts and an isolated fixture."""
import argparse
import importlib.util
import json
from pathlib import Path

spec = importlib.util.spec_from_file_location('web_contract', Path(__file__).with_name('verify-web-workspace.py'))
web = importlib.util.module_from_spec(spec)
spec.loader.exec_module(web)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--owner-credentials', required=True, type=Path)
    parser.add_argument('--viewer-credentials', required=True, type=Path)
    parser.add_argument('--base-url', required=True)
    parser.add_argument('--fixture', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    report = {'passed': False, 'checks': []}

    def check(name, value):
        report['checks'].append({'name': name, 'passed': bool(value)})
        if not value:
            raise RuntimeError(name)

    try:
        owner = web.WebSession(args.base_url)
        viewer = web.WebSession(args.base_url)
        owner.login(web.load_credentials(args.owner_credentials, args.base_url)[0])
        viewer.login(web.load_credentials(args.viewer_credentials, args.base_url)[0])
        fixture = json.loads(args.fixture.read_text())
        project_id = fixture.get('dataProjectId') or fixture['projectId']
        state_args = {'dataProjectId': project_id, 'branch': 'main'}
        before = owner.command('get_data_project_dataset_groups', state_args)
        denied = viewer.command_result('get_data_project_dataset_groups', state_args)
        check('non-member cannot read project group structure', denied.status in (403, 404))
        added = owner.request('POST', f'/api/v1/data-projects/{project_id}/members', body=json.dumps({'userId': viewer.user_id, 'role': 'viewer'}).encode(), content_type='application/json')
        check('owner can grant an existing fixture account viewer access', added.status == 200)
        readonly = viewer.command('get_data_project_dataset_groups', state_args)
        check('viewer reads identical groups and receives canEdit false', readonly['canEdit'] is False and readonly['projectMetadata'] == before['projectMetadata'])
        denied = viewer.command_result('set_data_project_dataset_groups', {**state_args, 'expectedRevision': readonly['revision'], 'groups': []})
        check('viewer group mutation is rejected', denied.status == 403)
        after = owner.command('get_data_project_dataset_groups', state_args)
        check('rejected viewer write preserves groups and revision', after['projectMetadata'] == before['projectMetadata'] and after['revision'] == before['revision'])
        report.update({'passed': True, 'dataProjectId': project_id})
    except Exception as error:
        report['failure'] = {'kind': type(error).__name__, 'status': getattr(error, 'status', None), 'code': getattr(error, 'code', None)}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({'passed': report['passed'], 'checks': len(report['checks']), 'failure': report.get('failure')}))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
