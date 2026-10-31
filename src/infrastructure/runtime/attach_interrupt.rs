//! Attached-host stop capability, client side (Story 21.2 A4 / AC8).
//!
//! The daemon projects the live terminal turn's generation to the writer
//! ([`DaemonFrame::ActiveTurn`]); the attached host's Ctrl+C arm asks
//! [`AttachedInterrupt::interrupt_frame`] for the control to send and the
//! reader arm feeds every `ActiveTurn` / `InterruptResult` frame back through
//! [`AttachedInterrupt::apply`]. The generation is a stale-target fence only:
//! authorization is the daemon's, and a refusal is shown honestly as an
//! Advisory notice (the connection and the turn it names are untouched).

use crate::adapters::daemon::protocol::{
    ClientFrame, DaemonFrame, InterruptOutcome, InterruptRefusal, TurnGeneration,
};
use crate::domain::models::NoticeLevel;

/// One user-visible line about a stop request. Always [`NoticeLevel::Advisory`]:
/// it never ends or alters the turn it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterruptNotice {
    pub(crate) level: NoticeLevel,
    pub(crate) text: String,
}

impl InterruptNotice {
    fn advisory(text: impl Into<String>) -> Self {
        Self {
            level: NoticeLevel::Advisory,
            text: text.into(),
        }
    }
}

/// Copy for the Ctrl+C that has nothing to send: the host calls this when
/// [`AttachedInterrupt::interrupt_frame`] returned `None`.
pub(crate) fn nothing_to_stop_notice(read_only: bool) -> InterruptNotice {
    InterruptNotice::advisory(if read_only {
        InterruptRefusal::ReadOnly.to_string()
    } else {
        InterruptRefusal::NoActiveTurn.to_string()
    })
}

/// The projected live terminal-turn generation for one attachment.
#[derive(Debug, Default)]
pub(crate) struct AttachedInterrupt {
    read_only: bool,
    generation: Option<TurnGeneration>,
}

impl AttachedInterrupt {
    /// `read_only` attachments never offer a stop control (the daemon refuses
    /// one regardless; this only avoids sending it).
    pub(crate) fn new(read_only: bool) -> Self {
        Self {
            read_only,
            generation: None,
        }
    }

    /// The stop control for the currently projected terminal turn, if any.
    /// Pure: sending it twice is answered honestly by the daemon.
    pub(crate) fn interrupt_frame(&self) -> Option<ClientFrame> {
        if self.read_only {
            return None;
        }
        self.generation
            .map(|generation| ClientFrame::InterruptTurn { generation })
    }

    /// Feed one daemon frame. Returns the notice to show, if any. Frames that
    /// are not part of the interrupt capability are ignored.
    pub(crate) fn apply(&mut self, frame: &DaemonFrame) -> Option<InterruptNotice> {
        match frame {
            DaemonFrame::ActiveTurn { generation } => {
                self.generation = *generation;
                None
            }
            DaemonFrame::InterruptResult {
                generation,
                outcome: InterruptOutcome::Interrupted,
            } => {
                // The turn's terminal event (and `ActiveTurn { None }`) follow;
                // keep the generation so a repeat press is answered honestly.
                let _ = generation;
                None
            }
            DaemonFrame::InterruptResult {
                generation,
                outcome: InterruptOutcome::Refused { reason },
            } => {
                // A refusal about the generation we hold means that target is
                // not interruptible by us; one about an older press must not
                // clear a newer projection.
                if self.generation == Some(*generation)
                    && *reason != InterruptRefusal::AlreadyInterrupted
                {
                    self.generation = None;
                }
                Some(InterruptNotice::advisory(reason.to_string()))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projected(generation: u64) -> DaemonFrame {
        DaemonFrame::ActiveTurn {
            generation: Some(TurnGeneration(generation)),
        }
    }

    #[test]
    fn story21_2_interrupt_frame_needs_a_projected_generation() {
        let mut state = AttachedInterrupt::new(false);
        assert!(state.interrupt_frame().is_none(), "nothing projected yet");
        assert!(state.apply(&projected(4)).is_none());
        assert!(matches!(
            state.interrupt_frame(),
            Some(ClientFrame::InterruptTurn {
                generation: TurnGeneration(4)
            })
        ));
        assert!(
            state
                .apply(&DaemonFrame::ActiveTurn { generation: None })
                .is_none()
        );
        assert!(
            state.interrupt_frame().is_none(),
            "turn end clears the target"
        );
    }

    #[test]
    fn story21_2_read_only_attachment_never_offers_a_stop() {
        let mut state = AttachedInterrupt::new(true);
        state.apply(&projected(1));
        assert!(state.interrupt_frame().is_none());
        let notice = nothing_to_stop_notice(true);
        assert_eq!(notice.level, NoticeLevel::Advisory);
        assert_eq!(notice.text, InterruptRefusal::ReadOnly.to_string());
    }

    #[test]
    fn story21_2_refusal_is_an_advisory_notice_and_clears_a_dead_target() {
        for reason in [
            InterruptRefusal::Stale,
            InterruptRefusal::NoActiveTurn,
            InterruptRefusal::NotWriter,
            InterruptRefusal::ReadOnly,
            InterruptRefusal::PeerTier,
            InterruptRefusal::WrongOrigin,
        ] {
            let mut state = AttachedInterrupt::new(false);
            state.apply(&projected(2));
            let notice = state
                .apply(&DaemonFrame::InterruptResult {
                    generation: TurnGeneration(2),
                    outcome: InterruptOutcome::Refused { reason },
                })
                .expect("a refusal is shown, never swallowed");
            assert_eq!(notice.level, NoticeLevel::Advisory);
            assert_eq!(notice.text, reason.to_string());
            assert!(!notice.level.is_turn_fatal(), "the turn keeps running");
            assert!(state.interrupt_frame().is_none(), "{reason:?} clears");
        }
    }

    #[test]
    fn story21_2_duplicate_refusal_keeps_the_settling_target() {
        let mut state = AttachedInterrupt::new(false);
        state.apply(&projected(3));
        let notice = state
            .apply(&DaemonFrame::InterruptResult {
                generation: TurnGeneration(3),
                outcome: InterruptOutcome::Refused {
                    reason: InterruptRefusal::AlreadyInterrupted,
                },
            })
            .expect("duplicate stop is acknowledged");
        assert_eq!(
            notice.text,
            InterruptRefusal::AlreadyInterrupted.to_string()
        );
        assert!(state.interrupt_frame().is_some());
    }

    #[test]
    fn story21_2_late_refusal_for_an_older_turn_keeps_the_newer_target() {
        let mut state = AttachedInterrupt::new(false);
        state.apply(&projected(5));
        state.apply(&projected(6));
        state.apply(&DaemonFrame::InterruptResult {
            generation: TurnGeneration(5),
            outcome: InterruptOutcome::Refused {
                reason: InterruptRefusal::Stale,
            },
        });
        assert!(matches!(
            state.interrupt_frame(),
            Some(ClientFrame::InterruptTurn {
                generation: TurnGeneration(6)
            })
        ));
    }

    #[test]
    fn story21_2_successful_stop_is_silent_and_keeps_the_target_until_turn_end() {
        let mut state = AttachedInterrupt::new(false);
        state.apply(&projected(8));
        assert!(
            state
                .apply(&DaemonFrame::InterruptResult {
                    generation: TurnGeneration(8),
                    outcome: InterruptOutcome::Interrupted,
                })
                .is_none()
        );
        assert!(state.interrupt_frame().is_some());
        state.apply(&DaemonFrame::ActiveTurn { generation: None });
        assert!(state.interrupt_frame().is_none());
    }
}
