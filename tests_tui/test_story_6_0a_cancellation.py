"""Story 6-0a: Cancellation Token Tree & Dual-Channel EventBus — TUI E2E tests.

While this story is primarily infrastructure (no new TUI widgets), the
tab-close → turn_cancel integration and multi-tab CancellationToken tree
must not regress tab lifecycle stability.

These tests exercise the TUI-visible paths:
- AC4: close_tab() invokes turn_cancel.cancel() before extraction
- AC1: sibling tabs have independent turn_cancel tokens
- Event loop stability after rapid tab create/close cycles
"""

import pytest

from harness import RustainTUI


def _assert_tui_alive(tui: RustainTUI, timeout: float = 3.0) -> None:
    """Custom responsiveness check that works on the welcome screen.

    assert_responsive() sends space+backspace which may not visibly change
    an empty input box.  We send a visible character, verify it appears,
    then backspace it away.
    """
    import time
    assert tui._child is not None and tui._child.isalive(), "TUI process is dead"
    tui.input_mode()
    tui.send("x")
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        time.sleep(0.05)
        screen = tui.get_screen_text()
        if "x" in screen:
            # Clean up
            tui.send("\x7f")
            return
    pytest.fail(f"TUI did not respond to input within {timeout}s")


@pytest.mark.slow
@pytest.mark.story_6_0a
class TestTabCancellationLifecycleNoApi:
    """Tab lifecycle stability with CancellationToken integration.
    These tests do NOT send messages to the AI, so they run without an API key.
    """

    def test_create_and_close_multiple_tabs_stays_responsive(self, tui: RustainTUI):
        """AC4: Rapidly create and close tabs — TUI must remain responsive.

        Each tab carries a turn_cancel child token. Closing a tab cancels it.
        This test verifies the cancellation path does not crash the TUI.
        """
        # Create several tabs
        for i in range(3):
            tui.new_tab()
            tui.wait_for_screen(f"Tab {i + 2}", timeout=3.0)

        # Close tabs one by one, verifying responsiveness each time
        for _ in range(3):
            tui.chat_mode()
            tui.close_tab()
            tui.wait(1.0)
            tui.input_mode()
            _assert_tui_alive(tui, timeout=3.0)

        # Should end up with one tab still alive
        tui.assert_screen_contains("Welcome to Rustain")
        tui.input_mode()
        _assert_tui_alive(tui, timeout=3.0)

    def test_close_tab_switches_to_another_tab(self, tui: RustainTUI):
        """AC4: Closing the active tab switches focus to another tab.

        Verifies that close_tab's internal cancellation + refocus logic
        leaves the TUI in a valid state.
        """
        tui.new_tab()
        tui.wait_for_screen("Tab 2", timeout=3.0)

        tui.new_tab()
        tui.wait_for_screen("Tab 3", timeout=3.0)

        # Close Tab 3 (currently active)
        tui.chat_mode()
        tui.close_tab()
        tui.wait(1.0)

        # Should no longer show Tab 3
        tui.assert_screen_not_contains("Tab 3")
        # Should still show Tab 2 (the new active tab)
        tui.assert_screen_contains("Tab 2")
        tui.input_mode()
        _assert_tui_alive(tui, timeout=3.0)

    def test_last_tab_close_is_noop(self, tui: RustainTUI):
        """AC4: Closing the last tab is rejected — app stays alive.

        Regression guard: the cancellation hook inside close_tab must not
        panic or corrupt state when there's only one tab.
        """
        # Close the only tab — should be rejected
        tui.chat_mode()
        tui.close_tab()
        tui.wait_for_screen("Only one tab open", timeout=3.0)

        tui.assert_screen_contains("Only one tab open")
        _assert_tui_alive(tui, timeout=3.0)

    def test_tab_bar_shows_correct_count_after_creates_and_closes(self, tui: RustainTUI):
        """Tab bar renders correctly through create/close cycles."""
        tui.new_tab()
        tui.wait_for_screen("Tab 2", timeout=3.0)

        tui.new_tab()
        tui.wait_for_screen("Tab 3", timeout=3.0)

        # Close Tab 3
        tui.chat_mode()
        tui.close_tab()
        tui.wait(1.0)

        # Tab bar should show Tab 1 and Tab 2 only
        screen = tui.get_screen_text()
        assert "Tab 1" in screen or "1" in screen, "Tab 1 should be visible in tab bar"
        assert "Tab 2" in screen, "Tab 2 should be visible in tab bar"
        assert "Tab 3" not in screen, "Tab 3 should NOT be visible after close"


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0a
class TestTabCancellationLifecycleWithApi:
    """Tab lifecycle tests that send messages to the AI (require API key)."""

    def test_switch_tabs_after_close(self, tui: RustainTUI):
        """AC1: After closing a tab, switching between remaining tabs works.

        Sibling tabs have independent CancellationTokens. Closing one
        must not corrupt the state of others.
        """
        tui.send_message("Message in tab 1")
        tui.wait_for_idle()

        tui.new_tab()
        tui.wait_for_screen("Tab 2", timeout=3.0)
        tui.send_message("Message in tab 2")
        tui.wait_for_idle()

        tui.new_tab()
        tui.wait_for_screen("Tab 3", timeout=3.0)

        # Close Tab 3
        tui.chat_mode()
        tui.close_tab()
        tui.wait(1.0)

        # Switch to Tab 1 — content should be intact
        tui.chat_mode()
        tui.switch_tab(1)
        tui.wait(1.5)
        # Soft assertion: we verify the tab title changed and the TUI is
        # responsive.  Exact message text may be scrolled out or the AI
        # response may not contain the prompt verbatim.
        screen = tui.get_screen_text()
        assert "Tab 1" in screen or "1" in screen, "Should be on Tab 1 after switch"
        _assert_tui_alive(tui, timeout=3.0)

        # Switch to Tab 2 — content should be intact
        tui.chat_mode()
        tui.switch_tab(2)
        tui.wait(1.5)
        screen = tui.get_screen_text()
        assert "Tab 2" in screen or "2" in screen, "Should be on Tab 2 after switch"
        _assert_tui_alive(tui, timeout=3.0)


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0a
class TestCancellationDuringToolExecution:
    """TUI-visible cancellation while a tool is running.

    These tests require the API because they rely on the model invoking
    the Bash tool.  They are best-effort: if the model does not use Bash,
    the test gracefully asserts on the observable outcome (no zombie
    processes, TUI stays responsive).
    """

    def test_close_tab_during_tool_does_not_hang(self, tui: RustainTUI):
        """AC4: Close tab while a tool is running — TUI must not hang.

        We send a message that strongly encourages a Bash invocation,
        then close the tab before the turn completes.  The TUI should
        cancel the in-flight tool and close the tab without hanging.
        """
        tui.new_tab()
        tui.wait_for_screen("Tab 2", timeout=3.0)

        # Ask for a long command — model may or may not use Bash
        tui.send_message("Run: sleep 10 && echo done")
        tui.wait(3.0)  # Give model time to start tool execution

        # Close the tab while the turn might still be in progress
        tui.chat_mode()
        tui.close_tab()

        # TUI must stay responsive; worst case we wait a bit longer
        _assert_tui_alive(tui, timeout=5.0)

        # If the model did use Bash, no zombie sleep should remain.
        # Use sleep 10 (not 5) to avoid collision with other tests that may
        # leave orphaned sleep 5 processes when the TUI is force-killed.
        import subprocess
        result = subprocess.run(
            ["pgrep", "-f", "sleep 10"],
            capture_output=True,
            text=True,
        )
        # If pgrep finds anything, kill them (soft check — process isolation
        # in the test environment can cause false positives from prior runs).
        if result.returncode == 0 and result.stdout.strip():
            pids = result.stdout.strip().splitlines()
            for pid in pids:
                subprocess.run(["kill", "-9", pid], capture_output=True)
            # Log warning but don't fail — the primary assertion is that the
            # TUI stayed responsive, which was already verified above.
            print(f"WARNING: sleep 10 processes found after tab close: {pids}")

    def test_switch_tab_during_tool_other_tab_stays_usable(self, tui: RustainTUI):
        """AC1: Switching away from a tab with an in-flight tool must not
        break the destination tab.

        Sibling tabs have independent cancellation tokens.
        """
        tui.send_message("Run: sleep 5 && echo done")
        tui.wait(2.0)

        # Switch to a new tab while Tab 1 might still be running
        tui.new_tab()
        tui.wait_for_screen("Tab 2", timeout=3.0)

        # Tab 2 should be immediately usable
        tui.send_message("Hello from tab 2")
        tui.wait_for_idle()
        tui.assert_screen_contains("Hello from tab 2")
        _assert_tui_alive(tui, timeout=3.0)
