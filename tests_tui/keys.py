"""Keyboard constants for rustain TUI interaction.

Derived from src/adapters/tui/app.rs key mappings.
Organized by focus state so tests read like user actions.
"""

# ── Special keys (ANSI escape sequences) ─────────────────────────────────────

ESC = "\x1b"
ENTER = "\r"
TAB = "\t"
BACKSPACE = "\x7f"
CTRL_C = "\x03"
CTRL_F = "\x06"
CTRL_H = "\x08"
CTRL_P = "\x10"
CTRL_R = "\x12"
CTRL_T = "\x14"
CTRL_U = "\x15"
CTRL_X = "\x18"

# Shift+Enter / Alt+Enter — terminal-dependent; these are common encodings.
SHIFT_ENTER = "\x1b[13;2u"
ALT_ENTER = "\x1b\r"
ALT_M = "\x1bm"
ALT_V = "\x1bv"

# Arrow keys
UP = "\x1b[A"
DOWN = "\x1b[B"
RIGHT = "\x1b[C"
LEFT = "\x1b[D"
HOME = "\x1b[H"
END = "\x1b[F"
DELETE = "\x1b[3~"
SHIFT_TAB = "\x1b[Z"


# ── Chat focus ───────────────────────────────────────────────────────────────

class Chat:
    """Keys active in Chat focus (vim-like navigation)."""
    FOCUS_INPUT = "i"
    QUIT = "q"
    SCROLL_DOWN = "j"
    SCROLL_UP = "k"
    JUMP_TOP = "g"
    JUMP_BOTTOM = "G"
    NEXT_BLOCK = "J"
    PREV_BLOCK = "K"
    PREV_USER_MSG = "{"
    NEXT_USER_MSG = "}"
    HELP = "?"
    COPY = "c"
    PEEK = "p"
    FORK = "f"
    REWIND = "R"        # uppercase
    BOOKMARK_TOGGLE = "m"
    BOOKMARK_LIST = "'"
    TOGGLE_TOOL_BLOCK = ENTER


# ── Overlay: Confirmation ────────────────────────────────────────────────────

class Confirm:
    """Keys for confirmation overlays (Rewind, Fork, Delete, Export)."""
    YES = "y"
    NO = "n"
    # Rewind-specific
    FORK_INSTEAD = "f"


# ── Overlay: Search ──────────────────────────────────────────────────────────

class Search:
    """Keys for within-conversation search (Ctrl+F)."""
    OPEN = CTRL_F
    NEXT = "n"          # in Navigating substate
    PREV = "N"          # in Navigating substate
    COMMIT = ENTER
    CLEAR = CTRL_U
    CLOSE = ESC


# ── Overlay: Permission / Question ───────────────────────────────────────────

class Permission:
    """Keys for tool permission prompts."""
    ALLOW = "y"
    DENY = "n"
    ALWAYS_ALLOW = "a"


# ── Sidebar ──────────────────────────────────────────────────────────────────

class Sidebar:
    """Keys for sidebar history panel."""
    TOGGLE = CTRL_H
    DOWN = "j"
    UP = "k"
    DELETE = "d"
    CROSS_SEARCH = "/"
    OPEN = ENTER
    FOCUS_INPUT = TAB
    FOCUS_CHAT = ESC
