import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from wavexlr import installation


class InstallationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.build = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.build.cleanup)
        cls.stage = Path(cls.build.name) / 'stage'
        cls.runtime_prefix = Path('/opt/openwave-test')
        cls.runtime_site = Path('/opt/python-test/site-packages')
        subprocess.run(
            ['make', 'install', f'DESTDIR={cls.stage}', f'PREFIX={cls.runtime_prefix}',
             f'SITEPKG={cls.runtime_site}', f'PYTHON={sys.executable}', 'INSTALL_METHOD=manual'],
            cwd=Path(__file__).resolve().parents[1], check=True, capture_output=True, text=True,
            env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'},
        )

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.tree = self.root / 'stage'
        shutil.copytree(self.stage, self.tree)
        self.prefix = self.tree / self.runtime_prefix.relative_to('/')
        self.module = self.tree / self.runtime_site.relative_to('/') / 'wavexlr'
        self.receipt = self.prefix / 'share/openwave/install-manifest.json'
        owner = patch.object(installation, '_owned', return_value=None)
        owner.start()
        self.addCleanup(owner.stop)
        sandbox = patch.dict(os.environ, {}, clear=True)
        sandbox.start()
        self.addCleanup(sandbox.stop)

    def inspect(self):
        return installation.inspect_installation(module_dir=self.module)

    def rewrite(self, change):
        data = json.loads(self.receipt.read_text())
        change(data)
        self.receipt.write_text(json.dumps(data))

    def assert_refused(self):
        result = self.inspect()
        self.assertEqual(result.method, 'unknown')
        self.assertTrue(result.problem)
        self.assertEqual(result.files, ())
        self.assertEqual(result.directories, ())

    def test_destdir_external_sitepkg_records_runtime_paths_and_relocates_together(self):
        raw = json.loads(self.receipt.read_text())
        self.assertEqual(raw['prefix'], str(self.runtime_prefix))
        self.assertEqual(raw['module_dir'], str(self.runtime_site / 'wavexlr'))
        self.assertNotIn(str(self.stage), self.receipt.read_text())
        result = self.inspect()
        self.assertEqual(result.method, 'manual', result.problem)
        self.assertEqual(result.prefix, self.prefix)
        self.assertEqual(result.module_dir, self.module)
        actual = {path for path in self.tree.rglob('*') if path.is_file()}
        self.assertEqual(set(result.files), actual)
        installation.validate_installation(result)
        self.assertNotIn(self.prefix, result.directories)
        self.assertNotIn(self.module.parent, result.directories)

    def test_missing_recorded_files_remain_in_retry_inventory(self):
        result = self.inspect()
        target = self.prefix / 'bin/openwave-diag'
        target.unlink()
        installation.validate_installation(result)
        retried = self.inspect()
        self.assertEqual(retried, result)
        self.assertIn(target, retried.files)
        unrelated = self.module / 'unrelated.py'
        unrelated.write_text('private user file\n')
        self.assertNotIn(unrelated, self.inspect().files)

    def test_replaced_remaining_file_blocks_retry(self):
        result = self.inspect()
        (self.prefix / 'bin/openwave-diag').unlink()
        (self.module / '__main__.py').write_text('changed code\n')
        with self.assertRaises(ValueError):
            installation.validate_installation(result)
        self.assert_refused()

    def test_traversal_and_shared_directory_cannot_be_receipt_authority(self):
        original = self.receipt.read_text()
        mutations = (
            lambda data: data['files'].append('/etc/passwd'),
            lambda data: data['files'].append(str(self.runtime_prefix / 'share/openwave/../unrelated')),
            lambda data: data['directories'].append(str(self.runtime_prefix / 'bin')),
            lambda data: data['files'].append(data['files'][0]),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                self.receipt.write_text(original)
                self.rewrite(mutation)
                self.assert_refused()

    def test_malformed_receipt_never_falls_back_to_legacy(self):
        for value in ('{broken', '[]', '{"schema":1,"schema":1,"application":"openwave","method":"manual"}'):
            with self.subTest(value=value):
                self.receipt.write_text(value)
                self.assert_refused()

    def test_symlinked_file_and_directory_boundaries_are_refused(self):
        result = self.inspect()
        target = self.module / '__main__.py'
        outside = self.root / 'outside.py'
        shutil.move(target, outside)
        target.symlink_to(outside)
        with self.assertRaises(ValueError):
            installation.validate_installation(result)
        self.assert_refused()
        target.unlink()
        shutil.move(outside, target)
        icons = self.prefix / 'share/openwave/icons'
        outside_icons = self.root / 'outside-icons'
        shutil.move(icons, outside_icons)
        icons.symlink_to(outside_icons, target_is_directory=True)
        with self.assertRaises(ValueError):
            installation.validate_installation(result)
        self.assert_refused()

    def test_updated_receipt_cannot_authorize_changed_file_for_old_plan(self):
        result = self.inspect()
        path = self.module / '__main__.py'
        path.write_text('changed code\n')
        runtime = str(self.runtime_site / 'wavexlr/__main__.py')
        self.rewrite(lambda data: data['sha256'].__setitem__(runtime, installation._digest(path)))
        with self.assertRaises(ValueError):
            installation.validate_installation(result)

    def test_package_ownership_wins_over_manual_metadata_and_modified_wrapper(self):
        (self.prefix / 'bin/openwave').write_text('package wrapper\n')
        def owner(paths):
            return 'deb' if self.prefix / 'bin/openwave' in paths else None
        with patch.object(installation, '_owned', side_effect=owner):
            result = self.inspect()
        self.assertEqual(result.method, 'deb')
        self.assertEqual(result.files, ())
        self.assertEqual(result.directories, ())
        with self.assertRaises(ValueError):
            installation.validate_installation(result)

    def test_managed_receipt_does_not_depend_on_pre_wrapping_hashes(self):
        self.rewrite(lambda data: data.update(method='nix'))
        (self.prefix / 'bin/openwave').write_text('wrapped by package builder\n')
        result = self.inspect()
        self.assertEqual(result.method, 'nix')
        self.assertEqual(result.files, ())
        self.assertEqual(result.directories, ())

    def test_source_checkout_never_exposes_raw_removal_inventory(self):
        source = self.root / 'source'
        module = source / 'wavexlr'
        module.mkdir(parents=True)
        (source / 'Makefile').write_text('')
        (source / 'wavexlr.desktop').write_text('')
        result = installation.inspect_installation(module_dir=module)
        self.assertEqual(result.method, 'source')
        self.assertEqual(result.files, ())
        with self.assertRaises(ValueError):
            installation.validate_installation(result)

    def legacy(self, prefix, module):
        module.mkdir(parents=True, exist_ok=True)
        for name in ('__init__.py', '__main__.py'):
            (module / name).write_text('# historical module\n')
        launcher = prefix / 'bin/openwave'
        launcher.parent.mkdir(parents=True, exist_ok=True)
        launcher.write_text('#!/bin/sh\nexec python3 -m wavexlr "$@"\n')
        desktop = prefix / 'share/applications/openwave.desktop'
        desktop.parent.mkdir(parents=True, exist_ok=True)
        desktop.write_text('[Desktop Entry]\nName=OpenWave\n')
        (prefix / 'share/openwave').mkdir(parents=True, exist_ok=True)
        return launcher

    def test_legacy_launcher_is_identifiable_and_snapshot_retry_is_bounded(self):
        prefix = self.root / 'legacy'
        module = prefix / 'lib/python3.13/site-packages/wavexlr'
        launcher = self.legacy(prefix, module)
        historical = (
            'share/icons/hicolor/symbolic/apps/openwave-symbolic.svg',
            'share/icons/hicolor/symbolic/apps/openwave-muted-symbolic.svg',
            'share/icons/hicolor/symbolic/apps/openwave-attention-symbolic.svg',
            'share/openwave/icons/openwave-symbolic.svg',
            'share/openwave/icons/openwave-muted-symbolic.svg',
            'share/openwave/icons/openwave-attention-symbolic.svg',
            'share/doc/openwave/openwave.svg',
        )
        for relative in historical:
            path = prefix / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('<svg/>\n')
        unrelated_icon = prefix / 'share/openwave/icons/openwave-custom.svg'
        unrelated_icon.write_text('<svg/>\n')
        result = installation.inspect_installation(module_dir=module, launcher=launcher)
        self.assertEqual(result.method, 'manual', result.problem)
        self.assertTrue(result.legacy)
        self.assertTrue({prefix / relative for relative in historical}.issubset(result.files))
        self.assertNotIn(unrelated_icon, result.files)
        launcher.unlink()
        installation.validate_installation(result)
        unrelated = module / 'unrelated.py'
        unrelated.write_text('unrelated\n')
        self.assertNotIn(unrelated, result.files)
        installation.validate_installation(result)

    def test_legacy_one_line_install_can_use_interpreter_global_site_outside_prefix(self):
        prefix = self.root / 'system/local'
        module = self.root / 'system/lib/python3.13/site-packages/wavexlr'
        launcher = self.legacy(prefix, module)
        with patch.object(installation, '_legacy_interpreter_sites', return_value=(module.parent,)):
            result = installation.inspect_installation(module_dir=module, launcher=launcher)
        self.assertEqual(result.method, 'manual', result.problem)
        self.assertIn(launcher, result.files)
        self.assertIn(module / '__main__.py', result.files)
        self.assertEqual(result.prefix, prefix)
        installation.validate_installation(result)

    def test_legacy_ambiguous_prefix_and_split_prefix_fail_closed(self):
        outer = self.root / 'legacy'
        inner = outer / 'local'
        module = inner / 'lib/python3.13/site-packages/wavexlr'
        self.legacy(outer, module)
        launcher = self.legacy(inner, module)
        result = installation.inspect_installation(module_dir=module, launcher=launcher)
        self.assertEqual(result.method, 'unknown')
        self.assertEqual(result.files, ())
        external = self.root / 'external/site-packages/wavexlr'
        self.legacy(inner, external)
        result = installation.inspect_installation(module_dir=external, launcher=launcher)
        self.assertEqual(result.method, 'unknown')
        self.assertEqual(result.files, ())


if __name__ == '__main__':
    unittest.main()
