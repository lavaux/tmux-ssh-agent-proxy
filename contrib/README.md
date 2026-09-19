# contrib

`tmux-ssh-agent.rb` is the original Ruby implementation that the Rust binary was
ported from. It is kept for reference: it is where the current behaviour comes
from, and it documents the intent behind parts of the port.

It is not installed and is not maintained. Nothing in the build refers to it.
Note that it predates the security and correctness fixes in the Rust version,
so it should not be used as a drop-in replacement.

`macos-smoke-test.sh` exercises the tool on macOS at runtime: daemon startup,
socket path length against Darwin's 104-byte limit, file permissions, agent
forwarding, per-pane routing across two attached clients, and clean shutdown.
It prints a pass or fail line per check and touches nothing outside a scratch
directory under `$TMPDIR`.

It exists because macOS support is verified from Linux by cross-compiling,
which proves the code typechecks for Apple targets but says nothing about how
it behaves. Run this on a Mac to close that gap.
