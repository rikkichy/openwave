import contextlib
import os
from pathlib import Path
import subprocess
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch

from wavexlr import desktop, installation, service, setup, uninstall


class UninstallTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.prefix = self.root / 'app'
        self.module = self.prefix / 'lib/python3.13/site-packages/wavexlr'
        self.module.mkdir(parents=True)
        for name in ('__init__.py', '__main__.py', 'app.py'):
            (self.module / name).write_text('# installed OpenWave module\n')
        (self.prefix / 'bin').mkdir()
        for name, target in (('openwave', 'wavexlr'), ('openwave-daemon', 'wavexlr.daemon')):
            (self.prefix / 'bin' / name).write_text(f'#!/bin/sh\nexec python3 -m {target} "$@"\n')
        entry = self.prefix / 'share/applications/openwave.desktop'
        entry.parent.mkdir(parents=True)
        entry.write_text('[Desktop Entry]\nName=OpenWave\n')
        (self.prefix / 'share/openwave').mkdir()
        self.home = self.root / 'home'
        self.home.mkdir()
        self.config = self.root / 'config'
        self.settings = self.config / 'openwave'
        self.settings.mkdir(parents=True)
        (self.settings / 'scenes.json').write_text('my scenes\n')
        self.audio = self.config / 'pipewire/pipewire.conf.d/52-openwave-mixes.conf'
        self.audio.parent.mkdir(parents=True)
        self.audio.write_text('# OpenWave generated mixes\n')
        self.usb = self.root / 'rules/99-openwave.rules'
        self.usb.parent.mkdir()
        stack = contextlib.ExitStack()
        self.addCleanup(stack.close)
        stack.enter_context(patch.dict(os.environ, {
            'HOME': str(self.home), 'XDG_CONFIG_HOME': str(self.config),
            'XDG_DATA_HOME': str(self.root / 'data'), 'FLATPAK_ID': '',
            'XDG_STATE_HOME': str(self.root / 'state'), 'XDG_CACHE_HOME': str(self.root / 'cache'),
        }))
        stack.enter_context(patch.object(setup, 'CONFIG_ROOT', str(self.config)))
        stack.enter_context(patch.object(setup, 'MIXES_PATH', str(self.audio)))
        stack.enter_context(patch.object(setup, 'WIREPLUMBER_PATH', str(self.config / 'wireplumber/rule.conf')))
        stack.enter_context(patch.object(setup, 'UDEV_PATH', str(self.usb)))
        stack.enter_context(patch.object(setup, 'UDEV_PATH_OLD', str(self.usb.with_name('99-wavexlr.rules'))))
        stack.enter_context(patch.object(service, '_BACKEND', service._Stub()))
        stack.enter_context(patch.object(service, 'backend_name', 'stub'))
        stack.enter_context(patch.object(service, '_pkexec_script', side_effect=AssertionError('Unexpected privileged operation')))
        stack.enter_context(patch.object(installation, '_owned', return_value=None))
        stack.enter_context(patch.object(uninstall, '_openwave_processes', return_value=[]))
        stack.enter_context(patch.object(uninstall.os, 'geteuid', return_value=os.getuid() or 1000))
        self.install = installation.inspect_installation(module_dir=self.module, launcher=self.prefix / 'bin/openwave')
        self.assertEqual(self.install.method, 'manual', self.install.problem)

    def plan(self):
        return uninstall._plan(self.install)

    def systemd(self, fail_stop=False):
        class Backend(service._Systemd):
            effective_command = str(self.prefix / 'bin/openwave-daemon')
            def _user(inner, *args, check=False):
                if args[0] == 'stop' and fail_stop:
                    raise subprocess.CalledProcessError(1, ['systemctl', *args], stderr='stop denied')
                if args[0] == 'show' and '--property=ExecStart' in args:
                    command = inner.effective_command
                    value = f'{{ path={command} ; argv[]={command} ; ignore_errors=no ; pid=0 ; }}'
                    return subprocess.CompletedProcess(['systemctl', *args], 0, value, '')
                return subprocess.CompletedProcess(['systemctl', *args], 0, '', '')
        backend = Backend()
        fragment = Path(backend.unit_path())
        fragment.parent.mkdir(parents=True, exist_ok=True)
        fragment.write_text('[Service]\nExecStart=' + str(self.prefix / 'bin/openwave-daemon') + '\n')
        return backend, fragment

    def test_manual_removal_preserves_settings_shared_files_and_unrecorded_content(self):
        unrelated = self.module / 'personal-note.txt'
        unrelated.write_text('keep this\n')
        shared = self.prefix / 'bin/other-tool'
        shared.write_text('unrelated application\n')
        autostart = Path(desktop.autostart_path())
        autostart.parent.mkdir(parents=True)
        autostart.write_text('[Desktop Entry]\nName=OpenWave\nExec=' + str(self.prefix / 'bin/openwave') + '\n')
        result = uninstall.execute(self.plan(), stop_running_app=False)
        self.assertTrue(result.success, result.error)
        self.assertTrue(result.app_removed)
        self.assertTrue(all(not path.exists() for path in self.install.files))
        self.assertFalse(self.audio.exists())
        self.assertFalse(autostart.exists())
        self.assertEqual((self.settings / 'scenes.json').read_text(), 'my scenes\n')
        self.assertEqual(unrelated.read_text(), 'keep this\n')
        self.assertEqual(shared.read_text(), 'unrelated application\n')

    def test_login_entry_for_another_installation_is_preserved(self):
        other = self.root / 'other-install/bin/openwave'
        other.parent.mkdir(parents=True)
        other.write_text('another installation\n')
        autostart = Path(desktop.autostart_path())
        autostart.parent.mkdir(parents=True)
        contents = '[Desktop Entry]\nName=OpenWave\nExec=' + str(other) + '\n'
        autostart.write_text(contents)
        result = uninstall.execute(self.plan(), stop_running_app=False)
        self.assertTrue(result.success, result.error)
        self.assertEqual(autostart.read_text(), contents)
        self.assertTrue(other.exists())

    def test_login_entry_with_bare_launcher_resolves_this_installation(self):
        launcher = self.prefix / 'bin/openwave'
        launcher.chmod(0o755)
        autostart = Path(desktop.autostart_path())
        autostart.parent.mkdir(parents=True)
        autostart.write_text('[Desktop Entry]\nName=OpenWave\nExec=openwave --hide\n')
        with patch.dict(os.environ, {'PATH': str(launcher.parent)}):
            result = uninstall.execute(self.plan(), stop_running_app=False)
        self.assertTrue(result.success, result.error)
        self.assertFalse(autostart.exists())

    def test_settings_deletion_is_explicit_and_limited_to_openwave(self):
        sibling = self.config / 'other-app'
        sibling.mkdir()
        (sibling / 'settings').write_text('keep\n')
        result = uninstall.execute(self.plan(), delete_settings=True, stop_running_app=False)
        self.assertTrue(result.success, result.error)
        self.assertFalse(self.settings.exists())
        self.assertEqual((sibling / 'settings').read_text(), 'keep\n')

    def test_failed_service_stop_keeps_unit_application_and_audio_configuration(self):
        backend, fragment = self.systemd(fail_stop=True)
        with patch.object(service, '_BACKEND', backend), patch.object(service, 'backend_name', 'systemd'):
            result = uninstall.execute(self.plan(), stop_running_app=False)
        self.assertFalse(result.success)
        self.assertTrue(fragment.exists())
        self.assertTrue(self.audio.exists())
        self.assertTrue(all(path.exists() for path in self.install.files))

    def test_disabled_but_present_service_is_removed(self):
        backend, fragment = self.systemd()
        with patch.object(service, '_BACKEND', backend), patch.object(service, 'backend_name', 'systemd'), \
             patch.object(service, 'is_installed', return_value=False):
            result = uninstall.execute(self.plan(), stop_running_app=False)
        self.assertTrue(result.success, result.error)
        self.assertFalse(fragment.exists())
        self.assertFalse((self.prefix / 'bin/openwave').exists())

    def test_elevation_cancel_reports_partial_cleanup_and_keeps_app_for_retry(self):
        self.usb.write_text('# OpenWave rule\n')
        plan = self.plan()
        with patch.object(service, '_pkexec_script', side_effect=RuntimeError('authorization cancelled')):
            result = uninstall.execute(plan, stop_running_app=False)
        self.assertFalse(result.success)
        self.assertTrue(result.removed)
        self.assertFalse(self.audio.exists())
        self.assertTrue(self.usb.exists())
        self.assertTrue(all(path.exists() for path in self.install.files))
        self.assertTrue(self.settings.exists())
        with patch.object(service, '_pkexec_script', side_effect=lambda _script: self.usb.unlink()):
            retried = uninstall.execute(plan, stop_running_app=False)
        self.assertTrue(retried.success, retried.error)
        self.assertFalse(self.usb.exists())
        self.assertFalse((self.prefix / 'bin/openwave').exists())

    def test_changed_inventory_blocks_before_any_cleanup(self):
        plan = self.plan()
        (self.module / '__main__.py').write_text('changed installation\n')
        result = uninstall.execute(plan, stop_running_app=False)
        self.assertFalse(result.success)
        self.assertFalse(result.removed)
        self.assertTrue(self.audio.exists())
        self.assertTrue((self.prefix / 'bin/openwave').exists())

    def test_managed_package_retains_every_application_file(self):
        managed = installation.Installation('deb', self.prefix, self.module, (), (), None,
                                            'Remove with the package manager')
        result = uninstall.execute(uninstall._plan(managed), stop_running_app=False)
        self.assertTrue(result.success, result.error)
        self.assertFalse(result.app_removed)
        self.assertTrue(all(path.exists() for path in self.install.files))
        self.assertFalse(self.audio.exists())
        self.assertEqual(result.guidance, managed.guidance)

    def test_symlinked_settings_never_remove_the_target(self):
        outside = self.root / 'preserved-settings'
        self.settings.rename(outside)
        self.settings.symlink_to(outside, target_is_directory=True)
        result = uninstall.execute(self.plan(), delete_settings=True, stop_running_app=False)
        self.assertFalse(result.success)
        self.assertEqual((outside / 'scenes.json').read_text(), 'my scenes\n')
        self.assertTrue((self.prefix / 'bin/openwave').exists())

    def test_other_installation_service_blocks_removal(self):
        backend, fragment = self.systemd()
        fragment.write_text('[Service]\nExecStart=/other-install/bin/openwave-daemon\n')
        with patch.object(service, '_BACKEND', backend), patch.object(service, 'backend_name', 'systemd'):
            plan = self.plan()
            self.assertFalse(plan.can_execute)
            result = uninstall.execute(plan, stop_running_app=False)
        self.assertFalse(result.success)
        self.assertTrue(fragment.exists())
        self.assertTrue(self.audio.exists())
        self.assertTrue((self.prefix / 'bin/openwave').exists())

    def test_effective_service_override_cannot_target_another_installation(self):
        backend, fragment = self.systemd()
        backend.effective_command = '/other-install/bin/openwave-daemon'
        with patch.object(service, '_BACKEND', backend), patch.object(service, 'backend_name', 'systemd'):
            plan = self.plan()
            self.assertFalse(plan.can_execute)
            result = uninstall.execute(plan, stop_running_app=False)
        self.assertFalse(result.success)
        self.assertTrue(fragment.exists())
        self.assertTrue(self.audio.exists())
        self.assertTrue((self.prefix / 'bin/openwave').exists())

    def test_package_owned_usb_rules_are_never_removed(self):
        self.usb.write_text('package-owned rule\n')
        def owner(paths):
            return 'deb' if self.usb in paths else None
        with patch.object(installation, '_owned', side_effect=owner):
            result = uninstall.execute(self.plan(), stop_running_app=False)
        self.assertTrue(result.success, result.error)
        self.assertTrue(result.app_removed)
        self.assertEqual(self.usb.read_text(), 'package-owned rule\n')

    def test_package_owned_service_is_stopped_but_not_deleted(self):
        backend, fragment = self.systemd()
        def owner(paths):
            return 'deb' if fragment in paths else None
        with patch.object(service, '_BACKEND', backend), patch.object(service, 'backend_name', 'systemd'), \
             patch.object(installation, '_owned', side_effect=owner):
            result = uninstall.execute(self.plan(), stop_running_app=False)
        self.assertTrue(result.success, result.error)
        self.assertTrue(fragment.exists())

    def test_remaining_daemon_prevents_application_removal(self):
        with patch.object(uninstall, '_openwave_processes', return_value=['12345']):
            result = uninstall.execute(self.plan(), stop_running_app=False)
        self.assertFalse(result.success)
        self.assertTrue(self.audio.exists())
        self.assertTrue(all(path.exists() for path in self.install.files))

    def test_recovery_code_survives_partial_module_removal(self):
        for source in Path(uninstall.__file__).parent.glob('*.py'):
            shutil.copyfile(source, self.module / source.name)
        self.install = installation.inspect_installation(module_dir=self.module,
                                                        launcher=self.prefix / 'bin/openwave')
        plan = self.plan()
        original = Path.unlink
        def fail_late(path, *args, **kwargs):
            if path == self.module / 'service.py':
                raise PermissionError('later module could not be removed')
            return original(path, *args, **kwargs)
        with patch.object(uninstall, '__file__', str(self.module / 'uninstall.py')), \
             patch.object(Path, 'unlink', fail_late):
            failed = uninstall.execute(plan, stop_running_app=False)
        self.assertFalse(failed.success)
        self.assertFalse((self.module / '__main__.py').exists())
        recovery = uninstall._recovery_bundles[uninstall._recovery_key(self.install)]
        code = (
            "import sys; sys.path.insert(0, sys.argv[1]); "
            "from wavexlr import installation,service,uninstall; "
            "installation._owned=lambda paths: None; service.backend_name='stub'; "
            "plan,_=uninstall._load_recovery(sys.argv[2]); "
            "uninstall._remove_application(plan.installation)"
        )
        result = subprocess.run([sys.executable, '-I', '-c', code, str(recovery),
                                 str(recovery / 'plan.json')], cwd=self.root,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(all(not path.exists() for path in self.install.files))
        self.assertTrue((self.settings / 'scenes.json').exists())
        uninstall._recovery_bundles.pop(uninstall._recovery_key(self.install))


if __name__ == '__main__':
    unittest.main()
