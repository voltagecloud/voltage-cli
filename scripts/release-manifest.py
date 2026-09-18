#!/usr/bin/env python3
"""Create checksums for every built archive and a concrete Homebrew formula from the Unix ones."""
import hashlib
import re
import sys
from pathlib import Path

version, directory = sys.argv[1], Path(sys.argv[2])
assert re.fullmatch(r'\d+\.\d+\.\d+(?:-[a-zA-Z0-9.-]+)?', version), 'Invalid version'
homebrew_targets = ['aarch64-apple-darwin', 'x86_64-apple-darwin', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu']
archives = {target: f'voltage-v{version}-{target}.tar.gz' for target in homebrew_targets}
archives['x86_64-pc-windows-msvc'] = f'voltage-v{version}-x86_64-pc-windows-msvc.zip'
checksums = {target: hashlib.sha256((directory / archive).read_bytes()).hexdigest() for target, archive in archives.items()}
(directory / 'SHA256SUMS').write_text(''.join(f'{checksums[t]}  {archives[t]}\n' for t in archives))
lines = ['class Voltage < Formula', '  desc "Command-line access to the Voltage API"', '  homepage "https://github.com/voltagecloud/voltage-cli"', f'  version "{version}"']
for os_name, suffix in [('macos', 'apple-darwin'), ('linux', 'unknown-linux-gnu')]:
    lines += [f'  on_{os_name} do']
    for arch, cpu in [('aarch64', 'arm'), ('x86_64', 'intel')]:
        t = f'{arch}-{suffix}'
        lines += [f'    on_{cpu} do', f'      url "https://github.com/voltagecloud/voltage-cli/releases/download/v{version}/{archives[t]}"', f'      sha256 "{checksums[t]}"', '    end']
    lines += ['  end']
lines += ['  def install', '    bin.install "voltage"', '    bash_completion.install "completions/voltage.bash" => "voltage"', '    zsh_completion.install "completions/_voltage"', '    fish_completion.install "completions/voltage.fish"', '  end', '  test do', '    assert_match "voltage", shell_output("#{bin}/voltage --version")', '  end', 'end']
(directory / 'voltage.rb').write_text('\n'.join(lines) + '\n')
