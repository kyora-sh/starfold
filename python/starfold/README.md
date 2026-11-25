# starfold (Python)

A client for agents and scripts driving a starfold desktop: screenshots, waiting for the
screen, mouse and keyboard, recordings and files. It is the client alone: no server binary and
no `starfold` command. To run a server on this machine, or start one from Python, install
[`starfold-server`](https://pypi.org/project/starfold-server/), which brings this package with it.

```sh
uv add starfold   # or: pip install starfold
```

From a checkout: `uv add --editable path/to/starfold/python/starfold` (or `pip install -e python/starfold`).

```python
from starfold import Starfold, act

# password defaults to $STARFOLD_PASSWORD
with Starfold("http://localhost:6901", password="...") as sf:
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
`--read-only`), `RecordingOffError` (it runs without `--enable-recording`), `ForbiddenError`,
`NotFoundError`, `StarfoldTimeoutError` and `StarfoldConnectionError`, all `StarfoldError`.
`wait()` reports a timeout in its result (`timed_out`) rather than raising.

Keys for `key()` are a single character or a name (Enter, Tab, Escape, BackSpace, Delete,
Home, End, PageUp, PageDown, Up, Down, Left, Right, F1-F24, ...), joined with `+` after
modifiers (ctrl, shift, alt, altgr, super). With shift, a letter or a US-layout digit or
punctuation key means the shifted key: `ctrl+shift+t`, `cmd+shift+4`.

## From 0.3.2

Up to 0.3.2 this package also carried the server: the `starfold` command, `start_server()`,
`LocalServer`, `binary_path()`, `check_libraries()` and their errors. From 0.4.0 they are in
`starfold-server`:

| 0.3.2                                                                                                                                                                 | 0.4.0                                                                              |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| `pip install starfold` (for the command or server)                                                                                                                    | `pip install starfold-server`                                                      |
| `from starfold import start_server`, and likewise `LocalServer`, `binary_path`, `check_libraries`, `BinaryNotFoundError`, `MissingLibrariesError`, `ServerStartError` | the same names from `starfold_server`                                              |
| `pip install "starfold[pillow]"` with the server                                                                                                                      | `pip install starfold-server "starfold[pillow]"`                                   |
| `uvx starfold` / `pipx run starfold`                                                                                                                                  | `uvx --from starfold-server starfold` / `pipx run --spec starfold-server starfold` |
| `Starfold`, `AsyncStarfold`, `act`, `StarfoldError` and the client's other errors                                                                                     | unchanged                                                                          |

`from starfold import start_server` (or any other moved name) raises an `ImportError` that says
so. Staying on `starfold==0.3.2` changes nothing: it still carries the server.

## Tests

```sh
uv run pytest
```
