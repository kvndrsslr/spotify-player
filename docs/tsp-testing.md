# Testing the TSP frontend against a real Tern

## Why

`cargo test` covers the TSP frontend in process: the document mapping (every page, popup and the
dock), node ids and frame ops resolving, the session loop (a key or a pointer event changes the
frame Tern is told to draw, and acks keep frames flowing), and the cover/blob path including its
regression guard. All of that runs against an in-memory terminal, so it cannot see what the real
terminal does with the wire.

Only a real Tern can. The freeze this document exists because of — the frontend thread parking on a
lock it already held, leaving the first frame on screen with keys and clicks dead — was invisible to
the in-process tests: every assertion there passed while the wire had stopped.

## Hosting a real headless Tern

```sh
cd /tmp && tern serve --control /tmp/tsp.sock      # a real pane; ^C or `ctl quit` stops it
tern ctl --control /tmp/tsp.sock state             # {"panes":[{"id":2,"cols":160,"rows":45}],...}
```

`serve` hosts a real pty and a real pane in the directory it is started from, with no window.
Verified against `tern 0.8.3`; `tern --version` prints the build.

## Driving and reading it

```sh
S=/tmp/tsp.sock
tern ctl --control $S run "/tmp/prog --flag"   # types the line and Enter into the pane (one argument)
tern ctl --control $S key j                    # one key, as a real keystroke
tern ctl --control $S type "hello"             # typed text
tern ctl --control $S click 400 100            # logical px, the same space as `tree` rects
tern ctl --control $S tree                     # the visible element tree, with text/values/rects
tern ctl --control $S dump                     # path, box and computed geometry of every element
tern ctl --control $S a11y                     # the accessibility tree (roles and names)
tern ctl --control $S pick 400 100             # hit-test a point
tern ctl --control $S state                    # the app's own state JSON
tern ctl --control $S shot NAME                # a PNG + layout JSON of the frame
```

Scenario files hold one command per line (`#` comments, `"quoted"` strings) and run with
`--file`:

```
run "clear; /tmp/prog"
wait 2000
key j
wait 500
shot after-key
```

`run`, `wait`, `key`, `type`, `click` and `shot` are the ones this frontend needs; `tern help dev`
lists the rest.

### A worked example

```sh
S=/tmp/tsp.sock
mkdir -p /tmp/tsp-cache /tmp/tsp-cfg
cp ~/.cache/spotify-player/{credentials.json,user_client_token.json,*_token.json} /tmp/tsp-cache/
tern serve --control $S &                      # start it in a scratch directory
APP=$PWD/target/debug/spotify_player
cat > /tmp/start.txt <<EOF
size 1280 800
run "clear; TERN_TSP_RECORD=/tmp/tsp-live.jsonl $APP -C /tmp/tsp-cache -c /tmp/tsp-cfg -f tsp -o enable_streaming=false -o client_port=8099"
wait 4000
shot library
EOF
tern ctl --control $S --file /tmp/start.txt
tern ctl --control $S tree | grep -o '"text":"[^"]*"' | head   # the library's rows

tern ctl --control $S key j                                    # move the selection one row down
sleep 1
python3 - <<'PY'                                               # what the app sent, both ways
import json
for line in open('/tmp/tsp-live.jsonl'):
    r = json.loads(line)
    if r['verb'] in ('f', 'e') and r['dir'] in ('out', 'in'):
        print(r['dir'], r['verb'], json.dumps(r['body'])[:120])
PY
```

The last frame and its ack, from a real run:

```
out f {"ops": [["set", "main.page.library.panes.playlists.list", {"selected": "main.page.library.panes.playlists.list.i:p:spotify:playlist:5MGGMWANWjbtMy52DSB6dJ"}]], "s": 2, "sf": "sp"}
in  e {"ev": "ack", "s": 2, "sf": "sp"}
```

`TERN_TSP_RECORD=<file>` makes the SDK record every message in both directions as JSONL
(`{"t":ms,"dir":"out"|"in","verb":"f","params":{…},"body":{…}}`) — the fastest way to see the wire.
It records the protocol only: acks are handled inside the SDK and never surface as input, so the
recording is what proves credit flow.

## Gotchas

- **Never** use bare `tern run|new|split|capture|send|focus`: with no `$TERN_WINDOW_KEY` they act on
  *the user's first window*. Always address the headless session with `tern ctl --control <sock>`.
- `tern serve --help` ignores `--help` and starts serving; `tern ctl --help` sends `--help` as a
  control command. Both are harmless, both are confusing.
- `expect` does not work in a headless serve (its wait runs on the session's virtual clock): assert
  with `wait <ms>` plus `tree`/`shot`.
- `shot NAME` needs a `size W H` first — without one the window is tiny and the PNG holds only the
  tab bar. Shots land in `<serve's cwd>/target/shots/tern/<scenario>/NAME.png` with a
  `NAME.layout.json` beside it.
- `resize W H` sets the viewport in pixels; `size W H` sets the window in cells. A viewport a few
  pixels tall makes input look broken (the pane is not visible, so Tern routes no keys to it), which
  is easy to mistake for the app hanging.
- `surface-play` is a scenario command, not a CLI subcommand, and Tern ships no recorder for what it
  replays.
- A program that dies while holding unread TSP input leaves those bytes in the pty: the shell then
  prints them as typed text (`ESC _` is zsh's insert-last-word) and the next typed line gets
  mangled. Run `stty sane` and `clear` in the pane — or start a fresh pane — before the next launch.
- The terminal grid is canvas-drawn and is not in `tree`/`dump`; a TSP *surface* is DOM, so its text
  is. Grid text needs `shot` and eyes.

## Testing our binary in that pane

- Copy the credential cache into a scratch folder; never point `-C` at the real one and never write
  the originals:

  ```sh
  mkdir -p /tmp/tsp-cache /tmp/tsp-cfg
  cp ~/.cache/spotify-player/{credentials.json,user_client_token.json,*_token.json} /tmp/tsp-cache/
  ```

- Launch it in the pane with that cache and config:

  ```
  spotify_player -C /tmp/tsp-cache -c /tmp/tsp-cfg -f tsp -o enable_streaming=false -o client_port=8099
  ```

  `-f tsp` makes the frontend mandatory, so a terminal that does not speak TSP is a hard error
  instead of a silent fall back to the TUI. `enable_streaming=false` keeps librespot from
  registering a second Connect device (the `never` spelling is rejected: `enable_streaming` parses
  as a bool or `always`/`on_demand`). `client_port=8099` avoids the CLI socket the user's own
  instance holds.
- Navigate only — keys and clicks. Do not press play: the app shares the account's credentials, and
  starting playback moves the user's device.
- The app logs to `<tmp cache>/spotify-player-*.log` (grep `Frontend:` and `Tern detected`); a
  TSP thread that stopped logs `The TSP frontend stopped`.

## What only the live rig catches

| Only a real Tern shows | `cargo test` covers |
| --- | --- |
| the lock/freeze class: a thread parked forever reads as a frozen frame here, and `sample <pid>` (macOS) hands you the stack | the document mapping, ids, ops, and the session loop's frames (a key or a select changes the wire, acks keep frames flowing) |
| the handshake as the terminal runs it: raw mode, DA1 ordering, the probe's extra connect, the `TERM_PROGRAM` gate | the cover/blob path, including its deadlock regression guard (build on a thread, fail on a timeout) |
| what the terminal accepts and draws — an unsupported kind or prop comes back as an `error` event, and `tree` shows the nodes it really created | the theme → palette mapping and the stylesheet |
| real ack timing and pointer routing (a click reaching the app as a `select`) | pointer events routed to handlers, from a hand-built event |
