"""Terminal-emulator contracts for real cursor-addressed incremental frames."""

import pyte
import pytest

from harness import TerminalScreen

BLANK = " " * 8


@pytest.mark.parametrize("feed, expected, fg", [
    pytest.param(["界界", "\x1b[1G│", "\x1b[3G "], ["│       ", BLANK], None, id="orphan-tails"),
    pytest.param(["界界", "\x1b[1Ga"], ["a 界    ", BLANK], None, id="right-repair"),
    pytest.param(["界界", "\x1b[2Gz"], [" z界    ", BLANK], None, id="left-repair"),
    pytest.param(["界界", "\x1b[2G界"], [" 界     ", BLANK], None, id="misaligned-wide"),
    pytest.param(["界界", "\x1b[1Gabcd"], ["abcd    ", BLANK], None, id="narrow-run"),
    pytest.param(["界界", "\x1b[3G界"], ["界界    ", BLANK], None, id="aligned-wide"),
    pytest.param(["\x1b[31m界\x1b[32m界\x1b[1G\x1b[34m│"], ["│ 界    ", BLANK], ("blue", "red", "green"),
                 id="neighbour-styles"),
    pytest.param(["12345678界\x1b[2;1Hx"], ["12345678", "x       "], None, id="after-wrap"),
    pytest.param(["\x1b[2;1H12345678界\x1b[2;1Hx"], ["12345678", "x       "], None, id="after-scroll"),
    pytest.param(["\x1b[?7l123456界x"], ["123456 x", BLANK], None, id="autowrap-off"),
])
def test_incremental_repaint_erases_overwritten_wide_glyph(feed, expected, fg):
    screen = TerminalScreen(8, 2)
    stream = pyte.Stream(screen)
    for chunk in feed:
        stream.feed(chunk)
        # Every drained frame must remain observable, not just the final one.
        assert len(screen.display) == 2
    assert screen.display == expected
    if fg is not None:  # the neighbours keep their own colours; the repair resets none of them
        assert tuple(screen.buffer[0][column].fg for column in range(3)) == fg
