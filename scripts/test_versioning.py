"""Offline regression tests for the repository's explicit version bindings."""
from __future__ import annotations

import ast
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest

from version_manager import Manager, VersionError

ROOT = Path(__file__).resolve().parents[1]


class RepositoryVersionTests(unittest.TestCase):
    def manager(self, root=ROOT):
        return Manager(root)

    def snapshot(self, root=ROOT):
        manager = self.manager(root)
        paths = {source['path'] for unit in manager.config['units'].values()
                 for source in [unit['canonical'], *unit.get('mirrors', []), *unit.get('assertions', [])]}
        return {path: (root / path).read_bytes() for path in paths}

    def fixture(self, root):
        for path in ['versioning.json', *self.snapshot()]:
            target = root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / path, target)

    def test_all_sources_agree(self):
        manager = self.manager()
        self.assertTrue(manager.inspect(manager.select()))

    def test_preview_noop_and_stale_expect_are_read_only(self):
        before = self.snapshot()
        for name in self.manager().config['units']:
            manager = self.manager()
            current = manager.inspect([name])[0].value
            plan, edits = manager.plan(name, part='patch')
            self.assertTrue(edits)
            self.assertEqual(plan['unit'], name)
            _, edits = manager.plan(name, value=current)
            self.assertFalse(edits)
            with self.assertRaises(VersionError):
                manager.plan(name, part='patch', expect='999.999.999')
        self.assertEqual(before, self.snapshot())

    def test_apply_isolates_units_and_only_changes_bound_tokens(self):
        for name in self.manager().config['units']:
            with self.subTest(unit=name), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                self.fixture(root)
                manager = self.manager(root)
                before = self.snapshot(root)
                old_other = {b.unit + ':' + str(b.source): b.value
                             for b in manager.inspect(manager.select()) if b.unit != name}
                bindings = manager.inspect([name])
                plan, edits = manager.plan(name, part='patch')
                manager.apply(edits)
                after_manager = self.manager(root)
                self.assertEqual({b.value for b in after_manager.inspect([name])}, {plan['target']})
                new_other = {b.unit + ':' + str(b.source): b.value
                             for b in after_manager.inspect(after_manager.select()) if b.unit != name}
                self.assertEqual(old_other, new_other)
                after = self.snapshot(root)
                self.assertEqual({p for p in before if before[p] != after[p]}, set(plan['files']))
                for path in plan['files']:
                    text = before[path].decode('utf-8')
                    spans = [b for b in bindings if b.source['path'] == path]
                    # Replace only the exact token spans; dependencies and all unrelated bytes must stay fixed.
                    from version_manager import Version
                    target = Version.parse(plan['target'], manager.config['units'][name]['canonical'].get('format', 'semver'))
                    for b in sorted(spans, key=lambda b: b.start, reverse=True):
                        value = target.render(b.source.get('format', 'semver'))
                        rendered = json.dumps(value, ensure_ascii=False) if b.encoding == 'json' else value
                        text = text[:b.start] + rendered + text[b.end:]
                    self.assertEqual(text.encode('utf-8'), after[path])
                _, noop = after_manager.plan(name, value=plan['target'])
                self.assertFalse(noop)
                after_manager.apply(noop)
                self.assertEqual(after, self.snapshot(root))

    def test_mismatch_refuses_without_partial_writes(self):
        for name, unit in self.manager().config['units'].items():
            if not unit.get('mirrors'):
                continue
            with self.subTest(unit=name), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                self.fixture(root)
                manager = self.manager(root)
                binding = manager.inspect([name])[1]
                path = root / binding.source['path']
                text = path.read_text()
                replacement = '"9.9.9"' if binding.encoding == 'json' else '9.9.9'
                path.write_text(text[:binding.start] + replacement + text[binding.end:])
                before = self.snapshot(root)
                with self.assertRaises(VersionError):
                    self.manager(root).plan(name, part='patch')
                self.assertEqual(before, self.snapshot(root))

    def test_unit_selection_and_invalid_version(self):
        manager = self.manager()
        with self.assertRaises(VersionError):
            manager.plan('not-a-release-unit', part='patch')
        if len(manager.config['units']) > 1:
            with self.assertRaises(VersionError):
                manager.plan(None, part='patch')
        for name in manager.config['units']:
            with self.assertRaises(VersionError):
                manager.plan(name, value='v1.2.3')

    def test_cli_check_and_preview_leave_sources_unchanged(self):
        before = self.snapshot()
        script = ROOT / 'scripts/version_manager.py'
        subprocess.run([sys.executable, str(script), '--root', str(ROOT), 'check'],
                       check=True, capture_output=True, text=True)
        for name in self.manager().config['units']:
            result = subprocess.run([sys.executable, str(script), '--root', str(ROOT),
                                     'bump', 'patch', '--unit', name],
                                    check=True, capture_output=True, text=True)
            self.assertEqual(json.loads(result.stdout)['mode'], 'dry-run')
        self.assertEqual(before, self.snapshot())

    def test_native_workspace_inheritance_is_retained(self):
        for unit, config in self.manager().config['units'].items():
            for assertion in config.get('assertions', []):
                if assertion.get('table') == ['package', 'version']:
                    path = ROOT / assertion['path']
                    self.assertEqual(tomllib.loads(path.read_text())['package']['version'], {'workspace': True})



if __name__ == '__main__':
    unittest.main()
