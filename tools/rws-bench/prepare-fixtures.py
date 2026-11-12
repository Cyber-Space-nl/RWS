#!/usr/bin/env python3
"""Create byte-identical distinct-path fixtures in a new directory."""
import argparse
import hashlib
import pathlib
import re
from contextlib import ExitStack

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--fixture', type=pathlib.Path, required=True)
parser.add_argument('--directory', type=pathlib.Path, required=True)
parser.add_argument('--paths', type=pathlib.Path, required=True)
parser.add_argument('--manifest', type=pathlib.Path, help='Optional sha256sum manifest relative to server root')
parser.add_argument('--prefix', default='/cold', help='URL directory corresponding to --directory')
parser.add_argument('--count', type=int, required=True)
args = parser.parse_args()
if not 1 <= args.count <= 1_000_000:
    parser.error('--count must be between 1 and 1000000')
if not re.fullmatch(r'/[A-Za-z0-9_-]+(?:/[A-Za-z0-9_-]+)*', args.prefix):
    parser.error('--prefix must be an absolute URL path of simple directory names')
if args.directory.exists() or args.paths.exists() or (args.manifest and args.manifest.exists()):
    parser.error('fixture directory, path list and manifest must not already exist')
if args.manifest and args.manifest == args.paths:
    parser.error('manifest and path list must be different files')
data = args.fixture.read_bytes()
checksum = hashlib.sha256(data).hexdigest()
args.directory.mkdir()
with ExitStack() as stack:
    path_list = stack.enter_context(args.paths.open('x', encoding='ascii', newline='\n'))
    manifest = stack.enter_context(args.manifest.open('x', encoding='ascii', newline='\n')) if args.manifest else None
    for index in range(args.count):
        name = f'{index:08}.txt'
        (args.directory / name).write_bytes(data)
        path_list.write(f'{args.prefix}/{name}\n')
        if manifest:
            manifest.write(f'{checksum}  {args.prefix.lstrip("/")}/{name}\n')
