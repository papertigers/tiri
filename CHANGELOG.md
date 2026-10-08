# Changelog

What changed in each release of tiri. A release that changed how the
client and server talk says so: after upgrading, stop servers left running
from before with `tiri kill-server`.

## [0.3.0](https://github.com/papertigers/tiri/compare/v0.2.0...v0.3.0) - 2026-10-08

- render: narrow cells go without U+FE0F, which terminals draw two wide, wiping out the narrow emoji when the next cell's drawn ([#28](https://github.com/papertigers/tiri/pull/28))
- client: a frame is owed whenever something comes due, so the last step of a slide isn't left undrawn ([#29](https://github.com/papertigers/tiri/pull/29))
- trace: with $TIRI_TRACE set, the client records each frame it draws, and what led to it, for replaying ([#30](https://github.com/papertigers/tiri/pull/30))
- trace: notes only input and layout changes, not every message ([#31](https://github.com/papertigers/tiri/pull/31))
- client: when to draw lives in Pacing, where every wake owes a frame, with tests ([#32](https://github.com/papertigers/tiri/pull/32))
- keyboard: ask the terminal for the kitty keyboard protocol when it speaks it, so keys like Shift+Enter come in distinct ([#34](https://github.com/papertigers/tiri/pull/34))
- keyboard: panes speak the kitty keyboard protocol to programs that ask, and snapshots carry the flags in force ([#35](https://github.com/papertigers/tiri/pull/35))
- keyboard: keys go to programs that asked for the kitty keyboard protocol in its form, Shift+Enter as CSI 13;2u and the like ([#36](https://github.com/papertigers/tiri/pull/36))
- README: programs in panes can have the kitty keyboard protocol ([#37](https://github.com/papertigers/tiri/pull/37))
- protocol: each side greets the other with its version first, so a client finds a server it can't talk to and says so, and kill-server works across versions ([#39](https://github.com/papertigers/tiri/pull/39))
- kill-server: stops a server from before greetings too, by its process, which the socket names ([#40](https://github.com/papertigers/tiri/pull/40))
- socket: a test that each system names the process at a socket's other end, run by CI on illumos, macOS and Linux ([#44](https://github.com/papertigers/tiri/pull/44))
- 0.3.0: clients and servers greet each other with their versions, so this one can't talk to 0.2 servers ([#42](https://github.com/papertigers/tiri/pull/42))
- agent: the proxy test takes a hang-up before its request is sent as no answer, as it is, not a failure ([#47](https://github.com/papertigers/tiri/pull/47))