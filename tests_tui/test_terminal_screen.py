"""Terminal-emulator contracts for real cursor-addressed incremental frames."""

import pyte
import pytest

from harness import TerminalScreen


@pytest.mark.parametrize("chunks, expected", [
    (["界界", "\x1b[1G│", "\x1b[3G "], "│       "),
    (["界界", "\x1b[1Ga"], "a 界    "),
    (["界界", "\x1b[2Gz"], " z界    "),
    (["界界", "\x1b[2G界"], " 界     "),
    (["界界", "\x1b[1Gabcd"], "abcd    "),
    (["界界", "\x1b[3G界"], "界界    "),
])
def test_incremental_repaint_erases_overwritten_wide_glyph(chunks, expected):
    screen = TerminalScreen(8, 2)
    stream = pyte.Stream(screen)
    for chunk in chunks:
        stream.feed(chunk)
        # Every drained frame must remain observable, not just the final one.
        assert len(screen.display) == 2
    assert screen.display == [expected, " " * 8]


def test_repaint_wide_glyph_preserves_neighbor_styles():
    screen = TerminalScreen(8, 2)
    stream = pyte.Stream(screen)
    stream.feed("\x1b[31m界\x1b[32m界\x1b[1G\x1b[34m│")
    assert screen.display == ["│ 界    ", " " * 8]
    assert screen.buffer[0][0].fg == "blue"
    assert screen.buffer[0][1].fg == "red"
    assert screen.buffer[0][2].fg == "green"


@pytest.mark.parametrize("prefix, row", [("12345678", 1), ("\x1b[2;1H12345678", 1)])
def test_repaint_after_wrap_or_scroll_targets_the_new_row(prefix, row):
    screen = TerminalScreen(8, 2)
    stream = pyte.Stream(screen)
    stream.feed(prefix + "界\x1b[" + str(row + 1) + ";1Hx")
    assert screen.display[row] == "x       "


def test_repaint_wide_glyph_with_autowrap_disabled():
    screen = TerminalScreen(8, 2)
    stream = pyte.Stream(screen)
    stream.feed("\x1b[?7l123456界x")
    assert screen.display == ["123456 x", " " * 8]
