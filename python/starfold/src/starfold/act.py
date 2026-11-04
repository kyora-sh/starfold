"""Builders for `Starfold.actions()`: each returns one action of `POST /input`."""

from __future__ import annotations

from typing import Any

Action = dict[str, Any]
Button = str | int


def _at(action: Action, x: int | None, y: int | None) -> Action:
    if (x is None) != (y is None):
        raise ValueError("give both x and y, or neither")
    if x is not None:
        action.update(x=x, y=y)
    return action


def move(x: int, y: int) -> Action:
    return {"type": "move", "x": x, "y": y}


def click(x: int | None = None, y: int | None = None, button: Button = "left", count: int = 1) -> Action:
    return _at({"type": "click", "button": button, "count": count}, x, y)


def down(x: int | None = None, y: int | None = None, button: Button = "left") -> Action:
    return _at({"type": "down", "button": button}, x, y)


def up(x: int | None = None, y: int | None = None, button: Button = "left") -> Action:
    return _at({"type": "up", "button": button}, x, y)


def scroll(dx: int = 0, dy: int = 0, x: int | None = None, y: int | None = None) -> Action:
    """Wheel notches: positive `dy` scrolls down, positive `dx` right."""
    return _at({"type": "scroll", "dx": dx, "dy": dy}, x, y)


def key(combo: str) -> Action:
    """Press and release keys together: "Enter", "ctrl+c", "ctrl+shift+t", "alt+F4"."""
    return {"type": "key", "key": combo}


def key_down(combo: str) -> Action:
    return {"type": "key_down", "key": combo}


def key_up(combo: str) -> Action:
    return {"type": "key_up", "key": combo}


def type_text(text: str) -> Action:
    return {"type": "type", "text": text}


def pause(ms: int) -> Action:
    return {"type": "pause", "ms": ms}


def drag(
    x1: int, y1: int, x2: int, y2: int, button: Button = "left", steps: int = 10, step_ms: int = 10
) -> list[Action]:
    """Press at (x1, y1), move to (x2, y2) in `steps` moves `step_ms` apart, release."""
    steps = max(1, steps)
    path: list[Action] = [down(x1, y1, button)]
    for i in range(1, steps + 1):
        path += [pause(step_ms), move(x1 + (x2 - x1) * i // steps, y1 + (y2 - y1) * i // steps)]
    return [*path, up(button=button)]
