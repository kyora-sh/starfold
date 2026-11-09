# starfold (Python)

A client for agents and scripts driving a starfold desktop: screenshots, waiting for the
screen, mouse and keyboard, recordings and files. Private: not published to PyPI.

```sh
uv add --editable path/to/starfold/python/starfold   # or: pip install -e python/starfold
```

```python
from starfold import Starfold, act

with Starfold("http://localhost:6901", password="...") as sf:  # password defaults to $VOXL_PASSWORD (being renamed to $STARFOLD_PASSWORD)
    shot = sf.screenshot()  # shot.png (bytes), shot.seq, shot.size
    shot.save("before.png")

    sf.click(400, 300)
    r = sf.type_text("hello\n")
    sf.wait(change=True, since=r.seq)  # returns once the typing changed the screen
    sf.wait(idle_ms=300)  # and once it settled

    sf.key("ctrl+shift+t")
    sf.scroll(dy=3, x=400, y=300)
    sf.drag(100, 100, 400, 100)
    sf.actions([act.key("ctrl+l"), act.type_text("example.com"), act.key("Enter")])

    clips = sf.record(5)  # a list of MP4s: one, or more if the encoder changed mid-recording
    sf.files_upload("data.csv", "local/data.csv")
    print([e.name for e in sf.files_list().entries])
```

`AsyncStarfold` has the same methods for asyncio. `Screenshot.image()` returns a Pillow
image if Pillow is installed (`starfold[pillow]`).

Errors: `AuthError` (wrong or missing password), `ReadOnlyError` (the server runs with
`--read-only`), `ForbiddenError`, `NotFoundError`, `StarfoldTimeoutError`,
`StarfoldConnectionError`, all `StarfoldError`. `wait()` reports a timeout in its result
(`timed_out`) rather than raising.

Keys for `key()` are a single character or a name (Enter, Tab, Escape, BackSpace, Delete,
Home, End, PageUp, PageDown, Up, Down, Left, Right, F1-F24, ...), joined with `+` after
modifiers (ctrl, shift, alt, altgr, super). With shift, a letter or a US-layout digit or
punctuation key means the shifted key: `ctrl+shift+t`, `cmd+shift+4`.

## Tests

```sh
uv run pytest                                      # unit tests, no server
STARFOLD_BIN=../../target/release/starfold uv run pytest -m integration   # Linux, Xvfb + xterm
```
