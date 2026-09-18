#!/usr/bin/env python3
"""Fail when the versioned contract and explicit command registry diverge."""
import json
from pathlib import Path

root = Path(__file__).resolve().parents[1]
spec = json.loads((root / 'api/openapi.json').read_text())
registry = json.loads((root / 'api/commands.json').read_text())
operations = {
    value['operationId']: (method.upper(), path)
    for path, item in spec['paths'].items()
    for method, value in item.items()
    if method.lower() in {'get', 'post', 'put', 'patch', 'delete', 'head', 'options'}
}
assert len(operations) == 47, 'Review the contract version and update coverage expectations deliberately'
assert operations.keys() == registry.keys(), f'Missing/extra mappings: {operations.keys() ^ registry.keys()}'
commands = [tuple(v['command']) for v in registry.values()]
assert len(set(commands)) == len(commands), 'Duplicate command mappings'
lines = ['# API command registry', '', 'Generated from the checked-in specification and command registry. IDs are positional; enclosing scope uses flags.', '', '| Command | Method | Path |', '|---|---|---|']
for oid, entry in registry.items():
    method, path = operations[oid]
    name = 'voltage ' + ' '.join(entry['command'])
    if entry.get('target'):
        name += ' ' + entry['target'].upper()
    lines.append(f'| `{name}` | {method} | `{path}` |')
expected = '\n'.join(lines) + '\n'
if '--write' in __import__('sys').argv:
    (root / 'docs/commands.md').write_text(expected)
else:
    assert (root / 'docs/commands.md').read_text() == expected, 'Regenerate docs: python3 scripts/check-coverage.py --write'
print(f'{len(operations)} operations mapped uniquely')
