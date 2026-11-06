//! S1-R3 attached negative: real owning tab, host painter and status pixels.
use super::*;
use crate::domain::models::{InvocationStatus, Turn, TurnPart};

#[test]
fn story21_5_narrow_cue_attached_tool_never_advertises_local_inspection() {
    for (width, height) in [(60, 16), (80, 24)] {
        for read_only in [false, true] {
            let mut rig = attach_keys::tests::Rig::new(read_only);
            rig.state.terminal_width = width;
            rig.state.terminal_height = height;
            rig.state.focus = FocusState::Chat;
            rig.state.session_count = 1;
            rig.state.theme.timing.status_hint_fade_sessions = 5;
            let mut turn = Turn::new("fixture-model".into(), 0);
            turn.push_part(|id| TurnPart::ToolInvocation {
                id,
                tool: "Bash".into(),
                args: serde_json::json!({"command":"echo attached"}),
                status: InvocationStatus::Success,
                started_at: 0,
                ended_at: Some(1),
            });
            rig.domain_tab.conversation.turns.push(turn);
            rig.domain_tab.conversation.rebuild_messages_mirror();
            let terminal = rig.paint();
            let buffer = terminal.backend().buffer();
            let layout = crate::adapters::tui::layout::compute_layout(
                buffer.area,
                &rig.state.theme,
                &rig.state.input_buffer,
                1,
                false,
                DensityMode::Focus,
            )
            .unwrap();
            let row = (0..width)
                .map(|x| buffer[(x, layout.status_bar.y)].symbol())
                .collect::<String>();
            let screen = buffer
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(screen.contains("Bash"), "actual attached tool: {screen}");
            assert!(
                attach_keys::status_hint(&rig.state).is_some(),
                "hints enabled"
            );
            assert!(
                !row.contains("Inspect"),
                "{width} readonly={read_only}: {row}"
            );
            assert!(rig.frames().is_empty(), "painting never sends a command");
        }
    }
}
