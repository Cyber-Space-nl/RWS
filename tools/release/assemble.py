#!/usr/bin/env python3
"""Build an unpublished, source-revision-bound Linux beta archive."""
import argparse
import gzip
import hashlib
import json
import os
import pathlib
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile

VERSION = '0.1.0-beta.4'
TARGET = 'x86_64-unknown-linux-gnu'
BINARIES = ('rws', 'rws-ws-load', 'rws-bench')
ROOT = pathlib.Path(__file__).resolve().parents[2]


def run(args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def output(args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=pathlib.Path, required=True,
                        help='New directory for the archive and SHA256SUMS')
    args = parser.parse_args()
    if platform.system() != 'Linux' or platform.machine() not in ('x86_64', 'amd64'):
        parser.error('this package recipe requires an x86-64 Linux build host')
    if args.output.exists():
        parser.error('output directory must not already exist')
    if output(['git', 'status', '--porcelain', '--untracked-files=no'], cwd=ROOT):
        parser.error('tracked worktree changes must be committed before building')
    for variable in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RWS_NATIVE_CPU',
                     'CARGO_BUILD_TARGET', 'CARGO_TARGET_DIR', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER'):
        if os.environ.get(variable):
            parser.error(f'unset {variable} to avoid host-specific build overrides')
    revision = output(['git', 'rev-parse', 'HEAD'], cwd=ROOT)
    epoch = int(output(['git', 'show', '-s', '--format=%ct', 'HEAD'], cwd=ROOT))
    rustc = output(['rustc', '-vV'])
    with tempfile.TemporaryDirectory(prefix='rws-release-') as temporary:
        temp = pathlib.Path(temporary)
        source = temp / 'source'
        source.mkdir()
        source_tar = temp / 'source.tar'
        with source_tar.open('wb') as stream:
            run(['git', 'archive', '--format=tar', revision], cwd=ROOT, stdout=stream)
        with tarfile.open(source_tar) as archive:
            archive.extractall(source, filter='data')
        build = temp / 'target'
        env = dict(os.environ, CARGO_TARGET_DIR=str(build))
        for binary in BINARIES:
            run(['cargo', 'build', '--locked', '--release', '--target', TARGET, '-p', binary], cwd=source, env=env)
        metadata = json.loads(output(['cargo', 'metadata', '--locked', '--format-version', '1'], cwd=source, env=env))
        tree = output(['cargo', 'tree', '--locked', '--target', TARGET, '--workspace', '--edges', 'normal,build', '--prefix', 'none', '--format', '{p}'], cwd=source, env=env)
        selected = {(m.group(1), m.group(2)) for line in tree.splitlines()
                    if (m := re.match(r'^([^ ]+) v([^ ]+)', line))}
        packages = {(p['name'], p['version']): p for p in metadata['packages']}
        if not selected.issubset(packages):
            raise RuntimeError('dependency tree does not match metadata')
        workspace = set(metadata['workspace_members'])
        bundle_name = f'rws-v{VERSION}-{TARGET}'
        bundle = temp / bundle_name
        (bundle / 'bin').mkdir(parents=True)
        (bundle / 'licenses').mkdir()
        for binary in BINARIES:
            source_binary = build / TARGET / 'release' / binary
            version = output([str(source_binary), '--version'])
            if version != f'{binary} {VERSION}':
                raise RuntimeError(f'unexpected {binary} version: {version}')
            shutil.copy2(source_binary, bundle / 'bin' / binary)
        for path in ('LICENSE', 'README.md', 'rws-icon.png', 'UBUNTU_INSTALL.md', 'CHANGELOG.md', 'SECURITY.md',
                     'CONTRIBUTING.md', 'Cargo.lock', 'config.example.toml',
                     'RWS_NGINX_APACHE_BENCHMARK.html', 'RWS_MEMORY_STABILITY_20M.html'):
            shutil.copy2(source / path, bundle / path)
        shutil.copytree(source / 'docs', bundle / 'docs')
        inventory = ['# Dependency license inventory', '',
                     'Overinclusive union of normal and build dependencies for the Linux x86-64 workspace.',
                     'License expressions are Cargo package metadata; collected files require review.', '',
                     '| Package | Version | Declared license |', '|---|---|---|']
        for key in sorted(selected):
            package = packages[key]
            if package['id'] in workspace:
                continue
            license_name = package.get('license') or ('See collected license file' if package.get('license_file') else None)
            if not license_name:
                raise RuntimeError(f'missing license metadata for {key}')
            package_dir = pathlib.Path(package['manifest_path']).parent
            notices = [p for p in package_dir.rglob('*') if p.is_file() and not p.is_symlink()
                       and p.name.lower().startswith(('license', 'copying', 'notice', 'copyright'))]
            if not notices:
                raise RuntimeError(f'missing local license/notice files for {key}')
            destination = bundle / 'licenses' / f'{key[0]}-{key[1]}'
            for notice in notices:
                relative = notice.relative_to(package_dir)
                out = destination / relative
                out.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(notice, out)
            inventory.append(f'| {key[0]} | {key[1]} | {license_name} |')
        (bundle / 'DEPENDENCIES.md').write_text('\n'.join(inventory) + '\n', encoding='utf-8')
        info = {'version': VERSION, 'target': TARGET, 'source_revision': revision,
                'rustc': rustc, 'build_kernel': platform.release(), 'build_libc': platform.libc_ver(),
                'note': 'Build-host libc is not a minimum supported libc. Review binary symbol requirements.'}
        (bundle / 'BUILD_INFO.json').write_text(json.dumps(info, indent=2) + '\n', encoding='utf-8')
        (bundle / 'BINARY_SHA256SUMS').write_text(''.join(f'{sha(bundle / "bin" / b)}  bin/{b}\n' for b in BINARIES), encoding='ascii')
        args.output.mkdir(parents=True)
        destination = args.output / f'{bundle_name}.tar.gz'
        with destination.open('wb') as raw:
            with gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode='w') as archive:
                    for path in [bundle, *sorted(bundle.rglob('*'))]:
                        info = archive.gettarinfo(str(path), arcname=str(path.relative_to(temp)))
                        info.uid = info.gid = 0
                        info.uname = info.gname = ''
                        info.mtime = epoch
                        if path.is_file():
                            with path.open('rb') as stream:
                                archive.addfile(info, stream)
                        else:
                            archive.addfile(info)
        (args.output / 'SHA256SUMS').write_text(f'{sha(destination)}  {destination.name}\n', encoding='ascii')
        print(f'prepared {destination} from {revision}; no release was published')


if __name__ == '__main__':
    main()
