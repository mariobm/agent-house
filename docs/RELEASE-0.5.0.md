# AHVM 0.5.0

Project environments can now be built and rebuilt from a repository. Optional
`.ahvm/install.sh` and `.ahvm/start.sh` scripts prepare the workspace and start
services before the agent begins. Successful builds create a reusable Ubuntu
image, check a fresh VM before activation, and keep the previous environment
available if a rebuild fails.

New project chats use a fresh VM with the prepared environment and safely refresh
repository source. Explicit VM reuse keeps an independent checkout. Build progress,
logs, cancellation, and storage limits are available through the environment API.
Builds upload the full nonzero image and may take several minutes.

Live chat progress is delivered durably to Cloud, improving recovery after
reconnections and preserving text, tool activity, and final run outcomes.

Project image storage is enabled by the host operator. Existing VMs keep their
files and assignments. macOS downloads remain Developer ID signed and notarized;
updates continue through the signed catalog and Homebrew.
