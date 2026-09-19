# contrib

`tmux-ssh-agent.rb` is the original Ruby implementation that the Rust binary was
ported from. It is kept for reference: it is where the current behaviour comes
from, and it documents the intent behind parts of the port.

It is not installed and is not maintained. Nothing in the build refers to it.
Note that it predates the security and correctness fixes in the Rust version,
so it should not be used as a drop-in replacement.
