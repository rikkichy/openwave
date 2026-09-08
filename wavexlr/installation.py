"""Read-only installation ownership and exact, bounded uninstall inventories.

No desktop, device, or service modules are imported here. Receipts describe
runtime paths; the optional build root is used only while recording files.
"""

import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import site
import sys


@dataclass(frozen=True)
class Installation:
    method: str
    prefix: Path | None
    module_dir: Path
    files: tuple[Path, ...]
    directories: tuple[Path, ...]
    receipt: Path | None
    guidance: str = ''
    problem: str | None = None
    legacy: bool = False
    identities: tuple[tuple[Path, str], ...] = ()


_METHODS = {'manual', 'deb', 'rpm', 'arch', 'nix', 'flatpak'}
_RECEIPT = Path('share/openwave/install-manifest.json')
_LOCATION = 'install-location.json'
_GUIDANCE = {
    'deb': 'Remove the managed package with: sudo apt remove openwave',
    'rpm': 'Remove openwave with your RPM package manager (dnf or zypper).',
    'arch': 'Remove the managed package with: sudo pacman -R openwave',
    'nix': 'Remove OpenWave from your Nix configuration or profile and rebuild/switch.',
    'flatpak': 'On the host, run: flatpak uninstall com.github.openwave. Host audio integration is managed separately.',
    'source': 'This is a source checkout. Its files will not be deleted.',
    'unknown': 'Installation ownership could not be established; no application files will be deleted.',
}
_FIXED = (
    'bin/openwave', 'bin/openwave-daemon', 'bin/openwave-diag', 'bin/openwave-probe',
    'share/applications/openwave.desktop',
    'share/openwave/openwave-autostart.desktop',
    'share/openwave/wireplumber/51-openwave-wave-xlr.conf',
    'share/openwave/pipewire/52-openwave-mixes.conf',
    'share/openwave/VERSION',
    'share/metainfo/com.github.openwave.metainfo.xml',
    'share/icons/hicolor/scalable/apps/openwave.svg',
    'share/icons/hicolor/scalable/status/openwave-white.svg',
    'share/icons/hicolor/scalable/status/openwave-black.svg',
    'share/icons/hicolor/scalable/status/openwave-red.svg',
    'share/doc/openwave/README.md', 'share/doc/openwave/icons/openwave.svg',
    'share/doc/openwave/asset-attribution.txt', 'share/licenses/openwave/LICENSE',
)
# Historical removal allowances only: never inputs to a new install manifest.
_LEGACY_FIXED = (
    'share/icons/hicolor/symbolic/apps/openwave-symbolic.svg',
    'share/icons/hicolor/symbolic/apps/openwave-muted-symbolic.svg',
    'share/icons/hicolor/symbolic/apps/openwave-attention-symbolic.svg',
    'share/doc/openwave/openwave.svg',
)
# The receipt is evidence of ownership, not authority to name arbitrary files.
# New shipped modules/assets must deliberately extend these bounded targets.
_MODULE_FILES = frozenset(
    f'{name}.py' for name in (
        '__init__', '__main__', 'app', 'audio', 'calibrate', 'child', 'daemon',
        'desktop', 'device', 'diag', 'effects', 'health', 'icons', 'installation',
        'meter', 'mixdialog', 'mixer', 'mixes', 'mixmatrix', 'paths', 'probe',
        'profiles', 'recovery', 'scenes', 'scheduler', 'service', 'setup',
        'sourcedialog', 'sources', 'tray', 'uninstall', 'uninstall_dialog',
    )
) | {'style.css', _LOCATION}
_ICON_FILES = {'openwave.svg', 'openwave-white.svg', 'openwave-black.svg',
               'openwave-red.svg'}
_LEGACY_ICON_FILES = {'openwave-symbolic.svg', 'openwave-muted-symbolic.svg',
                      'openwave-attention-symbolic.svg'}
_DOC_FILES = {'ARCHITECTURE.md', 'hardware-support.md', 'install-bazzite.md',
              'protocol.md', 'troubleshooting.md'}


def _absolute(value):
    if not isinstance(value, (str, Path)):
        raise ValueError('Inventory paths must be absolute strings')
    text = str(value)
    path = Path(text)
    if not path.is_absolute() or '..' in path.parts or str(path) != text or path == Path('/'):
        raise ValueError(f'Non-canonical inventory path: {text}')
    return path


def _boundary(path):
    """Reject symlinks and special files, including every existing ancestor."""
    for item in reversed((path, *path.parents)):
        try:
            mode = item.lstat().st_mode
        except FileNotFoundError:
            continue
        if stat.S_ISLNK(mode) or not (stat.S_ISREG(mode) or stat.S_ISDIR(mode)):
            raise ValueError(f'Unsafe installation boundary: {item}')
        if item != path and not stat.S_ISDIR(mode):
            raise ValueError(f'Non-directory installation parent: {item}')


def _json(path):
    _boundary(path)
    if path.stat().st_size > 4 * 1024 * 1024:
        raise ValueError('Installation metadata is too large')
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f'Duplicate metadata key: {key}')
            result[key] = value
        return result
    value = json.loads(path.read_text(), object_pairs_hook=unique)
    if not isinstance(value, dict):
        raise ValueError('Installation metadata must be an object')
    if type(value.get('schema')) is not int or value['schema'] != 1 or value.get('application') != 'openwave':
        raise ValueError('Unrecognized installation metadata')
    if value.get('method') not in _METHODS:
        raise ValueError('Unrecognized installation ownership')
    return value


def _digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest() if hasattr(hashlib, 'file_digest') else hashlib.sha256(stream.read()).hexdigest()


def _owned(paths):
    """Query package databases, never infer ownership from executable presence."""
    probes = tuple(dict.fromkeys(str(path) for path in paths))
    for method, command in (
        ('deb', ('dpkg-query', '-S')),
        ('rpm', ('rpm', '-qf', '--')),
        ('arch', ('pacman', '-Qo', '--')),
    ):
        binary = shutil.which(command[0])
        if not binary:
            continue
        for path in probes:
            try:
                result = subprocess.run((binary, *command[1:], path), capture_output=True,
                                        text=True, timeout=5, check=False, env={**os.environ, 'LC_ALL': 'C'})
            except (OSError, subprocess.TimeoutExpired) as exc:
                raise ValueError(f'Cannot establish package ownership: {exc}') from exc
            diagnostic = (result.stderr + result.stdout).casefold()
            missing = ('no path found matching pattern', 'is not owned by any package', 'no package owns')
            if result.returncode and not any(message in diagnostic for message in missing):
                raise ValueError(f'Package ownership query failed: {result.stderr.strip() or result.stdout.strip()}')
            if result.returncode == 0 and result.stdout.strip():
                return method
    return None


def _roots(prefix, module):
    return (module, prefix / 'share/openwave', prefix / 'share/doc/openwave',
            prefix / 'share/licenses/openwave')


def _allowed_file(path, prefix, module):
    if path in {prefix / item for item in (*_FIXED, *_LEGACY_FIXED)} or path == prefix / _RECEIPT:
        return True
    if path.is_relative_to(module):
        relative = path.relative_to(module)
        return len(relative.parts) == 1 and relative.name in _MODULE_FILES
    icons = prefix / 'share/openwave/icons'
    if path.parent == icons:
        return path.name in _ICON_FILES or path.name in _LEGACY_ICON_FILES
    docs = prefix / 'share/doc/openwave/docs'
    return path.parent == docs and path.name in _DOC_FILES


def _directories(files, prefix, module):
    roots = _roots(prefix, module)
    result = set(roots)
    for path in files:
        for parent in path.parents:
            if any(parent == root or parent.is_relative_to(root) for root in roots):
                result.add(parent)
    return tuple(sorted(result, key=str))


def _check_layout(prefix, module):
    if module.name != 'wavexlr' or module == prefix or prefix.is_relative_to(module):
        raise ValueError('Invalid OpenWave module directory')
    for path in (prefix, module):
        _boundary(path)


def _mapping(data, module):
    """Map a staged/relocated tree without recording its temporary DESTDIR."""
    recorded_prefix = _absolute(data.get('prefix'))
    recorded_module = _absolute(data.get('module_dir'))
    if module == recorded_module:
        return recorded_prefix, lambda path: path
    if recorded_module.is_relative_to(recorded_prefix):
        relative = recorded_module.relative_to(recorded_prefix)
        if module.parts[-len(relative.parts):] != relative.parts:
            raise ValueError('Installation module location disagrees with its receipt')
        prefix = module
        for _ in relative.parts:
            prefix = prefix.parent
        def relocate(path):
            if not path.is_relative_to(recorded_prefix):
                raise ValueError('Cannot relocate a split-prefix installation')
            return prefix / path.relative_to(recorded_prefix)
        return prefix, relocate
    # An external SITEPKG is movable only as part of the same DESTDIR tree.
    suffix = recorded_module.parts[1:]
    if module.parts[-len(suffix):] != suffix:
        raise ValueError('Cannot identify relocated external SITEPKG')
    stage = module
    for _ in suffix:
        stage = stage.parent
    return stage / recorded_prefix.relative_to('/'), lambda path: stage / path.relative_to('/')


def data_prefix(module_dir):
    """Resolve an installed locator without package queries or removal inventory."""
    module = Path(os.path.abspath(module_dir))
    pointer = module / _LOCATION
    if not pointer.exists():
        return None
    return _mapping(_json(pointer), module)[0]


def _from_receipt(receipt, module):
    data = _json(receipt)
    prefix, relocate = _mapping(data, module)
    if receipt != prefix / _RECEIPT:
        raise ValueError('Receipt is outside its declared prefix')
    _check_layout(prefix, module)
    method = data['method']
    if method != 'manual':
        return Installation(method, prefix, module, (), (), receipt, _GUIDANCE[method])
    def paths(key):
        values = data.get(key)
        if not isinstance(values, list) or not values or len(values) > 20000:
            raise ValueError(f'Invalid installation {key}')
        result = tuple(relocate(_absolute(value)) for value in values)
        if tuple(sorted(set(result), key=str)) != result:
            raise ValueError(f'Duplicate or unordered installation {key}')
        return result
    files, directories = paths('files'), paths('directories')
    if not all(_allowed_file(path, prefix, module) for path in files):
        raise ValueError('Inventory contains a non-OpenWave file')
    if directories != _directories(files, prefix, module):
        raise ValueError('Inventory contains missing or non-OpenWave directories')
    required = {receipt, module / _LOCATION, module / '__init__.py', module / '__main__.py', prefix / 'bin/openwave'}
    if not required.issubset(files):
        raise ValueError('Incomplete installation inventory')
    hashes = data.get('sha256')
    if not isinstance(hashes, dict):
        raise ValueError('Missing file identity inventory')
    identities = {relocate(_absolute(path)): digest for path, digest in hashes.items()}
    if identities.keys() != set(files) - {receipt}:
        raise ValueError('File identities do not match the inventory')
    for path, digest in identities.items():
        if not isinstance(digest, str) or re.fullmatch('[0-9a-f]{64}', digest) is None:
            raise ValueError('Malformed file identity')
        _boundary(path)
        if path.exists() and (not path.is_file() or _digest(path) != digest):
            raise ValueError(f'Installed file has changed: {path}')
    for path in directories:
        _boundary(path)
        if path.exists() and not path.is_dir():
            raise ValueError(f'Installed directory has changed: {path}')
    identities[receipt] = _digest(receipt)
    return Installation('manual', prefix, module, files, directories, receipt,
                        identities=tuple(sorted(identities.items(), key=lambda item: str(item[0]))))


def write_manifest(prefix, sitepkg, *, destdir='', method='manual'):
    """Record precisely the files installed by this checkout's Makefile."""
    prefix, sitepkg = _absolute(prefix), _absolute(sitepkg)
    if method not in _METHODS:
        raise ValueError('Unsupported installation method')
    stage = _absolute(destdir) if destdir else None
    def staged(path):
        return stage / path.relative_to('/') if stage else path
    module = sitepkg / 'wavexlr'
    _check_layout(staged(prefix), staged(module))
    receipt = prefix / _RECEIPT
    location = module / _LOCATION
    files = {prefix / item for item in _FIXED}
    source = Path(__file__).absolute().parent
    # Enumerate source inputs, not preexisting destination content.
    files.update(module / path.name for path in source.iterdir()
                 if path.suffix in {'.py', '.css'} and path.is_file())
    files.update(prefix / 'share/openwave/icons' / name for name in _ICON_FILES)
    docs = source.parent / 'docs'
    if docs.exists():
        files.update(prefix / 'share/doc/openwave/docs' / path.relative_to(docs)
                     for path in docs.rglob('*') if path.is_file())
    files.update((receipt, location))
    if not all(_allowed_file(path, prefix, module) for path in files):
        raise ValueError('Build contains unsupported inventory paths')
    for path in files:
        _boundary(staged(path))
        if path not in {receipt, location} and not staged(path).is_file():
            raise ValueError(f'Installed input is missing: {path}')
    metadata = dict(schema=1, application='openwave', method=method,
                    prefix=str(prefix), module_dir=str(module))
    staged(location).write_text(json.dumps(metadata, sort_keys=True) + '\n')
    staged(location).chmod(0o644)
    metadata.update(files=[str(path) for path in sorted(files, key=str)],
                    directories=[str(path) for path in _directories(files, prefix, module)],
                    sha256={str(path): _digest(staged(path)) for path in sorted(files, key=str) if path != receipt})
    staged(receipt).write_text(json.dumps(metadata, indent=2, sort_keys=True) + '\n')
    staged(receipt).chmod(0o644)
    return staged(receipt)


def _legacy_interpreter_sites(executable):
    resolved = shutil.which(executable) if not os.path.isabs(executable) else executable
    if not resolved or Path(resolved).resolve() != Path(sys.executable).resolve():
        return ()
    # The historical Makefile used this interpreter's first global site directory.
    locations = site.getsitepackages()
    return (Path(locations[0]),) if locations else ()


def _legacy(module, launcher):
    """Recognize historical Makefile wrappers, never arbitrary Python scripts."""
    candidates = {parent / 'bin/openwave' for parent in module.parents if parent != Path('/')}
    candidates.update((Path('/usr/local/bin/openwave'), Path('/usr/bin/openwave')))
    if launcher is not None:
        candidates.add(_absolute(launcher))
    found = []
    for candidate in sorted(candidates, key=str):
        if not candidate.exists():
            continue
        _boundary(candidate)
        text = candidate.read_text()
        old = re.fullmatch(r'#!/bin/sh\nexec ([^\s\"\x27]+) -m wavexlr "\$@"\n', text)
        prefix = candidate.parent.parent
        modern = (text.startswith('#!/bin/sh\nprefix=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)\n')
                  and '\nexec ' in text and text.endswith(' -m wavexlr "$@"\n'))
        if old:
            # Historical wrappers rely on the interpreter's site-packages. Do
            # not execute them (or arbitrary interpreters) during inspection.
            interpreter = Path(old.group(1))
            if re.fullmatch(r'python(?:3(?:\.\d+)?)?', interpreter.name) is None:
                continue
            standard_site = module.parent in _legacy_interpreter_sites(str(interpreter))
            coherent = module.parent.name in {'site-packages', 'dist-packages'} and (
                module.is_relative_to(prefix) or standard_site)
            if interpreter.is_absolute() and not standard_site:
                coherent = coherent and module.is_relative_to(interpreter.parent.parent)
            if not coherent:
                continue
        elif modern:
            relative = module.parent.relative_to(prefix) if module.is_relative_to(prefix) else None
            expected = f'export PYTHONPATH="$prefix/{relative}${{PYTHONPATH:+:$PYTHONPATH}}"' if relative else ''
            if not expected or expected not in text.splitlines():
                continue
        else:
            continue
        if (prefix / 'share/applications/openwave.desktop').is_file() and (prefix / 'share/openwave').is_dir():
            found.append(prefix)
    if len(found) != 1:
        raise ValueError('Legacy installation is missing or has ambiguous launcher/prefix ownership')
    prefix = found[0]
    _check_layout(prefix, module)
    files = set()
    for root in _roots(prefix, module):
        if root.exists():
            for path in sorted(root.rglob('*'), key=str):
                if path.is_file() and _allowed_file(path, prefix, module):
                    _boundary(path)
                    files.add(path)
    files.update(prefix / item for item in (*_FIXED, *_LEGACY_FIXED) if (prefix / item).is_file())
    if not {module / '__init__.py', module / '__main__.py', prefix / 'bin/openwave'}.issubset(files):
        raise ValueError('Legacy installation is incomplete')
    for path in files:
        _boundary(path)
    ordered = tuple(sorted(files, key=str))
    return Installation('manual', prefix, module, ordered,
                        _directories(files, prefix, module), None, legacy=True,
                        identities=tuple((path, _digest(path)) for path in ordered))


def inspect_installation(*, module_dir=None, launcher=None):
    module = Path(module_dir) if module_dir is not None else Path(__file__).absolute().parent
    module = Path(os.path.abspath(module))
    prefix = None
    receipt = None
    try:
        # Environmental/package ownership always wins over a manual receipt.
        if os.environ.get('FLATPAK_ID') or Path('/.flatpak-info').exists():
            return Installation('flatpak', None, module, (), (), None, _GUIDANCE['flatpak'])
        if module.is_relative_to('/nix/store') or module.resolve().is_relative_to('/nix/store'):
            return Installation('nix', None, module, (), (), None, _GUIDANCE['nix'])
        probes = [module / '__init__.py', module / '__main__.py', module / _LOCATION]
        if launcher is not None:
            probes.append(Path(launcher))
        method = _owned(probes)
        if method:
            return Installation(method, None, module, (), (), None, _GUIDANCE[method])
        _boundary(module)
        if (module.parent / 'Makefile').is_file() and (module.parent / 'wavexlr.desktop').is_file():
            return Installation('source', module.parent, module, (), (), None, _GUIDANCE['source'])
        pointer = module / _LOCATION
        if pointer.exists():
            location = _json(pointer)
            prefix, _ = _mapping(location, module)
            receipt = prefix / _RECEIPT
            if location['method'] != 'manual':
                method = location['method']
                return Installation(method, prefix, module, (), (), receipt, _GUIDANCE[method])
            candidates = [receipt]
        else:
            candidates = [parent / _RECEIPT for parent in module.parents if (parent / _RECEIPT).exists()]
        if len(candidates) > 1:
            raise ValueError('Multiple installation receipts match this module')
        if candidates:
            receipt = candidates[0]
            prefix = receipt.parent.parent.parent
            method = _owned((prefix / 'bin/openwave', receipt))
            if method:
                return Installation(method, prefix, module, (), (), receipt, _GUIDANCE[method])
            result = _from_receipt(receipt, module)
        else:
            result = _legacy(module, launcher)
        method = _owned((result.prefix / 'bin/openwave', result.receipt or result.module_dir / '__init__.py'))
        if method:
            return Installation(method, result.prefix, module, (), (), result.receipt, _GUIDANCE[method])
        return result
    except (OSError, ValueError, TypeError) as exc:
        return Installation('unknown', prefix, module, (), (), receipt, _GUIDANCE['unknown'], str(exc))


def validate_installation(installation):
    """Recheck identity and boundaries; missing recorded files permit retry."""
    if installation.method != 'manual' or installation.problem:
        raise ValueError('Only an identified manual installation may remove files')
    _check_layout(_absolute(installation.prefix), _absolute(installation.module_dir))
    if _owned(installation.files):
        raise ValueError('A package manager owns installation files')
    if installation.legacy:
        if ((installation.prefix / _RECEIPT).exists()
                or (installation.module_dir / _LOCATION).exists()):
            raise ValueError('Legacy recovery cannot override an installation receipt')
        required = {installation.module_dir / '__init__.py',
                    installation.module_dir / '__main__.py', installation.prefix / 'bin/openwave'}
        if not required.issubset(installation.files):
            raise ValueError('Legacy installation anchors are missing from its inventory')
        if (not installation.identities or
                tuple(path for path, _ in installation.identities) != installation.files or
                installation.directories != _directories(installation.files, installation.prefix, installation.module_dir)):
            raise ValueError('Legacy installation identity inventory is incomplete')
        for path, digest in installation.identities:
            if not _allowed_file(_absolute(path), installation.prefix, installation.module_dir):
                raise ValueError('Legacy inventory contains a non-OpenWave file')
            _boundary(path)
            if path.exists() and (not path.is_file() or _digest(path) != digest):
                raise ValueError(f'Legacy installation file changed: {path}')
        for path in installation.directories:
            _boundary(path)
            if path.exists() and not path.is_dir():
                raise ValueError(f'Legacy installation directory changed: {path}')
    else:
        if installation.receipt is None:
            raise ValueError('Manual installation has no receipt')
        current = _from_receipt(installation.receipt, installation.module_dir)
        if current != installation:
            raise ValueError('Installation receipt changed; inspect again before removing it')


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--record', action='store_true', required=True)
    parser.add_argument('--prefix', required=True)
    parser.add_argument('--sitepkg', required=True)
    parser.add_argument('--destdir', default='')
    parser.add_argument('--method', required=True, choices=sorted(_METHODS))
    args = parser.parse_args(argv)
    try:
        print(write_manifest(args.prefix, args.sitepkg, destdir=args.destdir, method=args.method))
    except (OSError, ValueError) as exc:
        parser.exit(1, f'Cannot record installation: {exc}\n')


if __name__ == '__main__':
    main()
