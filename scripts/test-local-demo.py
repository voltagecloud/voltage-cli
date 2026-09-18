#!/usr/bin/env python3
"""Test the running local auth/frontend stack and save browser demo recordings."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--frontend', type=Path, default=root.parent / 'frontend-turbo-cli-demo')
parser.add_argument('--auth', type=Path, default=root.parent / 'auth-service')
parser.add_argument('--headed', action='store_true')
args = parser.parse_args()
frontend, auth = args.frontend.resolve(), args.auth.resolve()
binary = root / 'target/debug/voltage'
if not binary.is_file():
    sys.exit('Run ./scripts/voltage-local --build first.')
if not (frontend / 'node_modules/@playwright/test/cli.js').is_file():
    sys.exit('Install frontend dependencies first: yarn install --immutable')
if not (frontend / 'packages/e2e/tests/cli-local-stack.spec.ts').is_file():
    sys.exit('The frontend checkout must contain the CLI device-login PR.')
state = root / '.work/local-demo'
state.mkdir(mode=0o700, parents=True, exist_ok=True)
os.umask(0o077)
env = {key: value for key, value in os.environ.items() if not key.startswith(('VOLTAGE_', 'E2E_'))}
env.update(E2E_LOCAL_CLI_STACK='true', E2E_CLI_BINARY=str(binary), E2E_LOCAL_AUTH_REPO=str(auth),
           E2E_BASE_URL='http://localhost:3210', E2E_EMAIL='', E2E_PASSWORD='', E2E_MFA_SECRET='',
           E2E_BASIC_AUTH_USERNAME='', E2E_BASIC_AUTH_PASSWORD='', E2E_AUTH_FILE=str(state / 'unused-auth.json'),
           PLAYWRIGHT_OUTPUT_DIR=str(state / 'evidence'))
command = ['node', 'node_modules/@playwright/test/cli.js', 'test', '-c', 'packages/e2e/playwright.config.ts',
           'cli-local-stack.spec.ts', '--project', 'chromium', '--workers', '1', '--retries', '0', '--reporter', 'line']
if args.headed:
    command.append('--headed')
result = subprocess.run(command, cwd=frontend, env=env)
if result.returncode:
    sys.exit(result.returncode)
for flow in ['password', 'mfa', 'deny']:
    folder = next((state / 'evidence').glob(f'*-{flow}-chromium'))
    video = next((folder / 'video').glob('*.webm'))
    shutil.copyfile(video, state / f'{flow}.webm')
    shutil.copyfile(folder / 'cli-transcript.txt', state / f'{flow}-cli.txt')
print(f'Local demo verified. Browser recordings and CLI transcripts: {state}')
