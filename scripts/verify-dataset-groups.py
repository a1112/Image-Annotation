#!/usr/bin/env python3
"""Real API acceptance for versioned dataset groups in isolated named fixtures.

Use --credentials for Web or --native-base-url for a separately rooted native
acceptance backend. Credentials and raw HTTP errors never enter the report.
"""
from __future__ import annotations
import argparse
import copy
import datetime
import hashlib
import importlib.util
import json
import urllib.request
import uuid
from pathlib import Path


def module(name, file):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(file))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


web = module('groups_web_contract', 'verify-web-workspace.py')
native = module('groups_native_contract', 'verify-data-project-versioning.py')


class GroupWebSession(web.WebSession):
    def request(self, method, path, **kwargs):
        headers = dict(kwargs.pop('extra_headers', None) or {})
        headers.setdefault('X-Data-Project-Capabilities', 'dataset-groups-v1')
        return super().request(method, path, extra_headers=headers, **kwargs)


class Acceptance:
    def __init__(self, args):
        self.args = args
        self.report = {'schema': 'dataset-groups-acceptance/v1', 'passed': False, 'checkedAt': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'checks': []}
        if args.native_base_url:
            self.client = native.Acceptance(args.native_base_url)
            self.call = self.client.call
            self.session = None
        else:
            credentials, base = web.load_credentials(args.credentials, args.base_url)
            self.session = GroupWebSession(base)
            self.session.login(credentials)
            self.call = self.session.command
        self.p = None

    def write(self):
        self.args.output.parent.mkdir(parents=True, exist_ok=True)
        self.args.output.write_text(json.dumps(self.report, ensure_ascii=False, indent=2) + '\n')

    def check(self, name, passed):
        self.report['checks'].append({'name': name, 'passed': bool(passed)})
        self.write()
        if not passed:
            raise RuntimeError(name)
        print(name, flush=True)

    def rejected(self, command, args):
        if self.session:
            result = self.session.command_result(command, args)
            return result.status in (400, 403, 404, 409, 422)
        return bool(self.client.call(command, args, error=True))

    def state(self, branch='main'):
        return self.call('get_data_project_dataset_groups', {'dataProjectId': self.p, 'branch': branch})

    def groups(self, branch='main'):
        return self.state(branch)['projectMetadata']['datasetGroups']

    def set_groups(self, groups, branch='main'):
        state = self.state(branch)
        return self.call('set_data_project_dataset_groups', {'dataProjectId': self.p, 'branch': branch, 'expectedRevision': state['revision'], 'groups': groups})

    def commit(self, message, branch='main'):
        value = self.call('commit_data_project_branch', {'dataProjectId': self.p, 'branch': branch, 'message': message})
        return value.get('commit', value)

    def switch(self, branch):
        return self.call('switch_data_project_branch', {'dataProjectId': self.p, 'branch': branch})

    def create_branch(self, name, commit):
        return self.call('create_data_project_branch', {'dataProjectId': self.p, 'name': name, 'fromCommitId': commit})

    @staticmethod
    def image_entries(commit):
        return sorted([(row['datasetId'], row['imageId'], row['imageHash'], row['annotationHash'], row['status'], row['qaStatus']) for row in commit.get('entries', [])])

    def run(self):
        if self.args.resume_report:
            old = web.load_json(self.args.resume_report)
            self.p = old['projectId']
            self.report.update({key: old[key] for key in ['projectId', 'datasetIds', 'expectedGroups', 'initialImageEntries', 'finalCommitId']})
            self.check('group structure survives service restart', self.groups() == old['expectedGroups'])
            history = self.call('list_data_project_version_commits', {'dataProjectId': self.p, 'branch': 'main'})
            commit = next(item for item in history if item['id'] == old['finalCommitId'])
            self.check('group history survives restart', commit.get('projectMetadata', {}).get('datasetGroups') == old['expectedGroups'])
            self.check('immutable images and labels survive restart', self.image_entries(commit) == [tuple(row) for row in old['initialImageEntries']])
            self.report['passed'] = True
            self.write()
            return
        suffix = uuid.uuid4().hex[:8]
        project = self.call('create_data_project', {'name': '卡片分组完整验收 ' + suffix, 'description': '独立验收项目：分组、版本、并发与图像保留'})
        self.p = project['id']
        datasets = []
        source_bytes = {}
        for index in range(3):
            dataset = self.call('create_dataset_project', {'name': f'分组样本 {suffix} {index + 1}', 'datasetType': 'yolo-detect', 'demoTemplate': 'empty' if self.session else 'demo-bbox', 'dataProjectId': self.p})
            datasets.append(dataset['id'])
            if self.session:
                data = web.png_fixture(64, 48, index + 15)
                uploaded = self.session.upload_png(dataset['id'], f'group-{index}.png', data)
                image_id = uploaded.get('imageId') or uploaded.get('id') or uploaded.get('image', {}).get('id')
                source_bytes[dataset['id']] = (image_id, data)
                helper = web.Acceptance(self.session, output=self.args.output.with_name('group-image-seed.json'))
                helper.save_bbox(dataset['id'], image_id, f'group-source-{index}')
            else:
                self.call('assign_dataset_to_data_project', {'dataProjectId': self.p, 'datasetId': dataset['id']})
        self.report.update({'projectId': self.p, 'datasetIds': datasets})
        self.check('project-scoped dataset list contains only its own data', {item['id'] for item in self.call('list_data_project_datasets', {'dataProjectId': self.p})} == set(datasets))
        initial = self.commit('保存分组前的原图和标签')
        initial_entries = self.image_entries(initial)
        self.report['initialImageEntries'] = initial_entries
        self.check('new project begins with empty groups', self.groups() == [])
        keys = [value.split('::', 1)[-1] for value in datasets]
        first = {'id': str(uuid.uuid4()), 'name': '新分组', 'datasetKeys': keys[:2], 'position': 0}
        state_before = self.state()
        state = self.set_groups([first])
        self.check('group write persists stable group ID and canonical membership', state['projectMetadata']['datasetGroups'] == [first] and state['canEdit'])
        self.check('group mutation advances its compare-and-swap revision', state['revision'] > state_before['revision'])
        stale = {'dataProjectId': self.p, 'branch': 'main', 'expectedRevision': state_before['revision'], 'groups': []}
        self.check('stale group update is rejected', self.rejected('set_data_project_dataset_groups', stale))
        self.check('stale update preserves the latest group structure', self.groups() == [first])
        bad = copy.deepcopy(first); bad['datasetKeys'] = [keys[0], 'foreign-' + uuid.uuid4().hex]
        self.check('foreign dataset keys are rejected', self.rejected('set_data_project_dataset_groups', {'dataProjectId': self.p, 'branch': 'main', 'expectedRevision': self.state()['revision'], 'groups': [bad]}))
        duplicate = {'id': str(uuid.uuid4()), 'name': '重复归组', 'datasetKeys': [keys[0]], 'position': 1}
        self.check('a dataset cannot belong to two groups', self.rejected('set_data_project_dataset_groups', {'dataProjectId': self.p, 'branch': 'main', 'expectedRevision': self.state()['revision'], 'groups': [first, duplicate]}))
        tree = self.call('get_data_project_working_tree', {'dataProjectId': self.p})
        self.check('group-only edits mark the worktree dirty', bool(tree['changes']))
        grouped = self.commit('仅提交数据集卡片分组')
        self.check('group-only commit contains project metadata', grouped.get('projectMetadata', {}).get('datasetGroups') == [first])
        self.check('group-only commit preserves every image and annotation hash', self.image_entries(grouped) == initial_entries)
        self.check('committed groups leave a clean worktree', not self.call('get_data_project_working_tree', {'dataProjectId': self.p})['changes'])
        self.create_branch('group-review', grouped['id'])
        renamed = copy.deepcopy(first); renamed['name'] = '训练样本'
        self.set_groups([renamed]); ours = self.commit('主分支重命名分组')
        self.switch('group-review')
        self.check('branch checkout restores its own group names', self.groups('group-review') == [first])
        second = {'id': str(uuid.uuid4()), 'name': '复核样本', 'datasetKeys': [keys[2]], 'position': 1}
        self.set_groups([first, second], 'group-review'); theirs = self.commit('复核分支整理第三数据集', 'group-review')
        self.switch('main')
        merged = self.call('merge_data_project_branches', {'dataProjectId': self.p, 'oursBranch': 'main', 'theirsBranch': 'group-review', 'message': '合并独立分组变化'})
        merge_commit = merged.get('commit')
        self.check('independent name and membership changes merge without conflict', bool(merge_commit) and not merged.get('conflicts'))
        groups = self.groups()
        self.check('merge retains both independent group changes', next(g for g in groups if g['id'] == first['id'])['name'] == renamed['name'] and next(g for g in groups if g['id'] == second['id'])['datasetKeys'] == [keys[2]])
        self.check('group merge records two parents', merge_commit.get('parentId') == ours['id'] and merge_commit.get('secondParentId') == theirs['id'])
        self.create_branch('group-conflict', merge_commit['id'])
        ours_groups = copy.deepcopy(groups)
        next(g for g in ours_groups if g['id'] == first['id'])['datasetKeys'].remove(keys[0])
        next(g for g in ours_groups if g['id'] == second['id'])['datasetKeys'].append(keys[0])
        self.set_groups(ours_groups); self.commit('当前分支移动到复核组')
        self.switch('group-conflict')
        theirs_groups = copy.deepcopy(groups)
        next(g for g in theirs_groups if g['id'] == first['id'])['datasetKeys'].remove(keys[0])
        third = {'id': str(uuid.uuid4()), 'name': '冲突来源组', 'datasetKeys': [keys[0]], 'position': 2}
        theirs_groups.append(third)
        self.set_groups(theirs_groups, 'group-conflict'); self.commit('来源分支移动到另一组', 'group-conflict')
        self.switch('main')
        preview = self.call('preview_data_project_merge', {'dataProjectId': self.p, 'oursBranch': 'main', 'theirsBranch': 'group-conflict'})
        self.check('different destinations for one dataset produce an explicit conflict', bool(preview.get('conflicts')))
        result = self.call('merge_data_project_branches', {'dataProjectId': self.p, 'oursBranch': 'main', 'theirsBranch': 'group-conflict', 'message': '显式解决分组归属冲突'})
        while result.get('conflicts'):
            self.check('group merge returns a resolvable session', bool(result.get('sessionId')))
            result = self.call('resolve_data_project_merge', {'dataProjectId': self.p, 'sessionId': result['sessionId'], 'conflictKey': result['conflicts'][0]['key'], 'key': result['conflicts'][0]['key'], 'resolution': 'ours'})
        self.check('explicit conflict resolution preserves the chosen group', keys[0] in next(g for g in self.groups() if g['id'] == second['id'])['datasetKeys'])
        restored = self.call('revert_data_project_commit', {'dataProjectId': self.p, 'branch': 'main', 'commitId': grouped['id'], 'message': '恢复初始卡片分组'})
        restored = restored.get('commit', restored)
        self.check('restore creates a new commit and restores group structure', restored['id'] != grouped['id'] and self.groups() == [first])
        self.check('restore preserves original image and annotation references', self.image_entries(restored) == initial_entries)
        history = self.call('list_data_project_version_commits', {'dataProjectId': self.p, 'branch': 'main'})
        ids = {item['id'] for item in history}
        self.check('group restore retains earlier immutable history', {initial['id'], grouped['id'], merge_commit['id'], restored['id']}.issubset(ids))
        if self.session:
            for dataset_id, (image_id, original) in source_bytes.items():
                actual, _ = self.session.download_image(dataset_id, image_id)
                self.check('original image bytes remain intact after group history operations', actual == original)
        self.report.update({'expectedGroups': self.groups(), 'finalCommitId': restored['id']})
        self.report['passed'] = True
        self.write()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--credentials', type=Path)
    parser.add_argument('--base-url')
    parser.add_argument('--native-base-url')
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--resume-report', type=Path)
    args = parser.parse_args()
    if not args.native_base_url and not args.credentials:
        parser.error('provide --credentials or --native-base-url')
    acceptance = None
    try:
        acceptance = Acceptance(args)
        acceptance.run()
        print(json.dumps({'passed': True, 'checks': len(acceptance.report['checks']), 'output': str(args.output)}))
        return 0
    except Exception as exc:
        if acceptance:
            acceptance.report['failure'] = {'kind': type(exc).__name__, 'status': getattr(exc, 'status', None), 'code': getattr(exc, 'code', None)}
            acceptance.write()
        print(json.dumps({'passed': False, 'kind': type(exc).__name__, 'status': getattr(exc, 'status', None), 'code': getattr(exc, 'code', None)}))
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
