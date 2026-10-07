# Client-side rendering

## Why

Today the server composes every client's frames, animations included, and
sends them as terminal output. On a slow link that output is the cost:
a sideways scroll is about 14 full-screen redraws, and opening the overview
is 400–800 KB. With the client drawing, the network carries what changed in
the panes and the layout, and every animation runs locally at full rate.

## Shape

- **The server** owns the panes (PTYs and their emulators), the layout,
  focus, and named workspaces. It answers programs' queries, as now, and
  is the only one that does.
- **Each client** keeps a copy of every pane it shows: an alacritty
  emulator fed the same output the server's was. It draws, animates, picks
  up mouse and keys, and keeps everything that's its own: its view of the
  strip, the overview, selection, scrollback position, effects and
  thumbnails.
- **Between them**, one protocol over the Unix socket, or over ssh through
  `tiri bridge` for a server on another machine.

## Pane output

The server forwards each pane's output to clients as the bytes the program
wrote. Pane sizes are shared (panes follow the client used last), so a
client's copy replays to exactly the server's screen.

Each pane's output is numbered by byte offset. Clients acknowledge how far
they've taken in. A client that falls too far behind, as on a slow link
under a busy pane, gets a **snapshot** instead of the backlog: escape
sequences that rebuild the screen, its modes, and recent history in a fresh
copy. Attaching starts from a snapshot too.

Snapshots carry the screen and the last thousand lines of history, and say
whether that's all there is. A client scrolling to within a screen of the
top of its copy, when there's more, asks for the pane again with twice the
history. The answer is the same terminal further back, so the client keeps
its place and selection in it.

## Layout

Workspaces, columns, panes in columns, sizes, focus, titles and the like go
as structured state, sent whole when it changes; it's small. A client turns
its keys and mouse into actions: some its own (scrolling back, selecting,
the overview), the rest sent to the server (focus, moving and resizing
columns, opening and closing panes).

## Config

Keys, theme and animations come from the client's config. Layout presets
(column widths and pane heights) stay the server's, since layout is shared.

## Steps

1. `--host`: attach to a server on another machine through `ssh host tiri
   bridge`, with today's protocol.
2. A snapshot serializer for a pane's emulator, tested by replaying it.
3. Protocol: layout state, pane output with offsets, snapshots, actions.
4. The client draws: drawing, input and per-client state move to it, and
   the server stops rendering.
5. Flow control: acknowledgements, and snapshots for clients left behind.
6. Older history on demand.
