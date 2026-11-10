#!/usr/bin/env python3
"""Compare all file-backed Mach-O sections in a delivery and its isolated GUI copy."""
import argparse
import hashlib
import json
import plistlib
import struct
from pathlib import Path


def loaded_sections(path):
    data = path.read_bytes()
    magic, _, _, _, count, _, _, _ = struct.unpack_from('<8I', data)
    if magic != 0xfeedfacf:
        raise ValueError('expected a little-endian 64-bit Mach-O')
    result = hashlib.sha256()
    offset = 32
    sections = 0
    for _ in range(count):
        command, size = struct.unpack_from('<II', data, offset)
        if command == 0x19:
            nsects = struct.unpack_from('<I', data, offset + 64)[0]
            for index in range(nsects):
                section = offset + 72 + index * 80
                name = data[section:section + 32]
                length = struct.unpack_from('<Q', data, section + 40)[0]
                file_offset = struct.unpack_from('<I', data, section + 48)[0]
                flags = struct.unpack_from('<I', data, section + 64)[0]
                if flags & 0xff in (1, 12, 18):
                    continue
                if file_offset + length > len(data):
                    raise ValueError('invalid file-backed section range')
                result.update(name)
                result.update(data[file_offset:file_offset + length])
                sections += 1
        offset += size
    return result.hexdigest(), sections


def describe(app):
    info = plistlib.loads((app / 'Contents/Info.plist').read_bytes())
    binary = app / 'Contents/MacOS' / info['CFBundleExecutable']
    digest, count = loaded_sections(binary)
    return {'appPath': str(app.resolve()), 'binarySha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'loadedSectionsSha256': digest, 'fileBackedSectionCount': count}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', required=True, type=Path)
    parser.add_argument('--acceptance-app', required=True, type=Path)
    parser.add_argument('--dmg', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    delivery, acceptance = describe(args.app), describe(args.acceptance_app)
    report = {'delivery': delivery, 'acceptance': acceptance, 'dmgSha256': hashlib.sha256(args.dmg.read_bytes()).hexdigest(), 'passed': delivery['loadedSectionsSha256'] == acceptance['loadedSectionsSha256'], 'note': 'Bundle ID and data-root configuration differ only in Info.plist; all file-backed native code and embedded frontend sections must match.'}
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({'passed': report['passed'], 'sections': delivery['fileBackedSectionCount']}))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
