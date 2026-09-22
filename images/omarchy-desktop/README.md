# Omarchy desktop image

AHVM's Omarchy 4.0.4 image uses the upstream desktop applications and user
configuration on a clean Arch Linux filesystem. AHVM supplies the kernel,
virtual networking, systemd entrypoint and private WayVNC transport. It does
not install Omarchy's physical-machine bootloader or repartition the host.

Pinned upstream release: `v4.0.4`, commit
`c668141e9c42b13c80c9ca4ea108e11708c5e8a5`.

## Included software

- Hyprland, Quickshell, Foot, Chromium, Firefox and Nautilus.
- Git, Neovim, tmux, ripgrep, fzf, lazygit, build tools and Python.
- LibreOffice, Obsidian, OBS Studio, Kdenlive, mpv and the upstream application set.
- Mise, Node.js LTS, Bun, Claude Code, Codex, OpenCode and Pi, preinstalled.
- Docker/Compose and lazydocker packages. Kernel-dependent Docker functionality
  must be qualified separately; installation alone does not guarantee it works.
- Omarchy's Bash configuration, themes, application launchers and keyboard bindings.

Other AI launchers provided by upstream may install their tools on first use.
AI tools ship without accounts or API keys; users sign in themselves.
The desktop account is `desktop`, with passwordless sudo inside its own VM.
This does not grant access to the host. Arch uses `pacman`, not Ubuntu's `apt`.

## Build

On a Linux x86_64 builder, as root:

```sh
images/omarchy-desktop/build-image.sh /path/to/new-omarchy.ext4 /path/to/ahvm-forge
```

The build includes a fallback for `omarchy-version` because AHVM installs the
runtime source without the physical-machine meta-package. Upstream system
upgrades remain unqualified; publish a new AHVM image for a tested update.

The output must be new. The builder verifies the pinned Arch bootstrap and
Omarchy archive, installs signature-checked packages from the Omarchy stable
and Arch repositories, and produces a sparse 40 GiB ext4 disk. Arch packages
and coding tools roll at build time; exact installed versions are recorded in
`/usr/share/ahvm/packages.txt` and `/usr/share/ahvm/tools.json`. The source pin
alone does not make the build reproducible.

For development only, `prepare-image.sh CLEAN_ARCH.ext4 NEW.ext4 OMARCHY_SOURCE`
accepts an existing clean Arch base. Never use a user's running or exported VM
as an image source. See [publication](../../docs/IMAGE-PUBLISHING.md) for signing,
qualification and promotion, and [usage](../../docs/OMARCHY-DESKTOP.md).

The initialization wrapper execs systemd as PID 1. System services supervise
Forge and the PAM-backed desktop session. `user@1000.service` supplies the user
manager and D-Bus session; user services supervise WayVNC and the private vsock
relay. Forge waits for the relay before advertising readiness. The normal Ubuntu
image and host services are unchanged.

The bundled 6.12.44 kernel supports this boot and cgroup v2. The image overrides
Arch's PID maximum with the supported 32768 limit. AHVM owns virtual networking,
so networkd/resolved are disabled in this image; its first-run Wi-Fi prompt is
skipped when no wireless device exists. Journal storage is bounded to 64 MiB.

Power management, desktop login/lock, sound, Omarchy updates, hardware helpers,
clipboard/international keyboard integration and GPU isolation remain outside
this qualification. RAM snapshots are unsupported. Keep Ubuntu as the default.

## Qualification

The initial 4.0.0.alpha preview was visually tested on `agent_house`. For the
4.0.4 replacement, an isolated 2-vCPU / 8-GiB VM booted in 6.31 seconds and
restarted in 5.91 seconds (local storage, single observations). VNC delivered
1280×720 with the Omarchy bar, wallpaper, Bash prompt and welcome notification.
Systemd reported no failed units. HTTPS and Chromium's sandboxed headless
renderer fetched a public page. A saved file survived a cold restart.

The desktop user's interactive Bash found Git, Neovim, Node.js, npm, Bun,
Python, Claude Code, Codex, OpenCode and Pi; tool version commands and
passwordless guest sudo passed. These local checks precede the signed-download
and replicated Cloud qualification recorded with publication.
