#!/usr/bin/env python3
"""Generate the tap formula from the same catalog payload used by the updater."""
import json
from pathlib import Path
import sys

catalog = json.loads(Path(sys.argv[1]).read_text())
cli = catalog['cli']
version = cli['darwin-aarch64']['version']
lines = ['class Ahvm < Formula', '  desc "Persistent Linux microVMs for coding agents"', '  homepage "https://ahvm.app"', f'  version "{version}"', '  license "LicenseRef-AHVM-Community-1.0"']
for os, entries in [('macos', [('arm', 'darwin-aarch64'), ('intel', 'darwin-x86_64')]), ('linux', [('intel', 'linux-x86_64')])]:
    lines.append(f'  on_{os} do')
    for arch, platform in entries:
        a = catalog.get('client', {}).get(platform, cli[platform])
        lines += [f'    on_{arch} do', f'      url "{a["url"]}"', f'      sha256 "{a["sha256"]}"', '    end']
    lines.append('  end')
lines += ['', '  def install', '    if File.exist?("ahvm")', '      bin.install "ahvm"', '      bin.install "ahvm-desktop" if File.exist?("ahvm-desktop")', '      pkgshare.install Dir["ahvm-desktop-*"] unless Dir["ahvm-desktop-*"].empty?', '    else', '      bin.install Dir["ahvm-*"][0] => "ahvm"', '    end', '    chmod 0755, bin/"ahvm"', '  end', '', '  test do', '    assert_match version.to_s, shell_output("#{bin}/ahvm --version")', '  end', 'end']
Path(sys.argv[2]).write_text('\n'.join(lines) + '\n')
