//! Pure decision state for the GUI's local node recovery loop.
//!
//! This module does not start, stop, or contact a process. It turns observed
//! local socket and child-process state into one decision, which keeps the
//! durability policy deterministic and unit-testable.

pub(crate) const WEDGED_RESTART_ROUNDS: u32 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SocketState {
    Ready,
    /// The first probe failed, but the node bound its socket during the
    /// existing startup grace window.
    ReadyAfterGrace,
    Unresponsive,
}

impl SocketState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::ReadyAfterGrace => "ready_after_grace",
            Self::Unresponsive => "unresponsive",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ownership {
    GuiOwnedAlive,
    /// The GUI either reused a service-owned node or its own child is dead.
    NoLiveGuiChild,
}

impl Ownership {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::GuiOwnedAlive => "gui_owned_alive",
            Self::NoLiveGuiChild => "no_live_gui_child",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Observation {
    pub(crate) socket: SocketState,
    pub(crate) ownership: Ownership,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Decision {
    Healthy,
    StartupCompleted,
    WaitForOwnedNode,
    RestartOwnedNode,
    EnsureNode,
}

impl Decision {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::StartupCompleted => "startup_completed",
            Self::WaitForOwnedNode => "wait_for_owned_node",
            Self::RestartOwnedNode => "restart_owned_node",
            Self::EnsureNode => "ensure_node",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Evidence {
    pub(crate) observation: Observation,
    pub(crate) decision: Decision,
    pub(crate) prior_wedged_rounds: u32,
    pub(crate) wedged_rounds: u32,
}

#[derive(Debug, Default)]
pub(crate) struct BackendRecovery {
    wedged_rounds: u32,
}

impl BackendRecovery {
    pub(crate) fn observe(&mut self, observation: Observation) -> Evidence {
        let prior_wedged_rounds = self.wedged_rounds;
        let decision = match (observation.socket, observation.ownership) {
            (SocketState::Ready, _) => {
                self.wedged_rounds = 0;
                Decision::Healthy
            }
            (SocketState::ReadyAfterGrace, _) => {
                self.wedged_rounds = 0;
                Decision::StartupCompleted
            }
            (SocketState::Unresponsive, Ownership::NoLiveGuiChild) => {
                self.wedged_rounds = 0;
                Decision::EnsureNode
            }
            (SocketState::Unresponsive, Ownership::GuiOwnedAlive) => {
                self.wedged_rounds = self.wedged_rounds.saturating_add(1);
                if self.wedged_rounds >= WEDGED_RESTART_ROUNDS {
                    self.wedged_rounds = 0;
                    Decision::RestartOwnedNode
                } else {
                    Decision::WaitForOwnedNode
                }
            }
        };
        Evidence {
            observation,
            decision,
            prior_wedged_rounds,
            wedged_rounds: self.wedged_rounds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(socket: SocketState, ownership: Ownership, generation: u64) -> Observation {
        Observation {
            socket,
            ownership,
            generation,
        }
    }

    #[test]
    fn live_socket_clears_wedge_history() {
        let mut recovery = BackendRecovery::default();
        let _ = recovery.observe(observation(
            SocketState::Unresponsive,
            Ownership::GuiOwnedAlive,
            4,
        ));

        let evidence =
            recovery.observe(observation(SocketState::Ready, Ownership::GuiOwnedAlive, 4));
        assert_eq!(evidence.decision, Decision::Healthy);
        assert_eq!(evidence.prior_wedged_rounds, 1);
        assert_eq!(evidence.wedged_rounds, 0);
        assert_eq!(evidence.observation.generation, 4);
    }

    #[test]
    fn starting_node_that_binds_during_grace_is_not_restarted() {
        let mut recovery = BackendRecovery::default();
        let evidence = recovery.observe(observation(
            SocketState::ReadyAfterGrace,
            Ownership::GuiOwnedAlive,
            7,
        ));
        assert_eq!(evidence.decision, Decision::StartupCompleted);
        assert_eq!(evidence.wedged_rounds, 0);
    }

    #[test]
    fn dead_or_external_node_is_ensured_without_owner_kill() {
        let mut recovery = BackendRecovery::default();
        let evidence = recovery.observe(observation(
            SocketState::Unresponsive,
            Ownership::NoLiveGuiChild,
            0,
        ));
        assert_eq!(evidence.decision, Decision::EnsureNode);
        assert_eq!(evidence.wedged_rounds, 0);
    }

    #[test]
    fn live_owned_but_wedged_node_restarts_on_existing_third_round() {
        let mut recovery = BackendRecovery::default();
        for expected in 1..WEDGED_RESTART_ROUNDS {
            let evidence = recovery.observe(observation(
                SocketState::Unresponsive,
                Ownership::GuiOwnedAlive,
                9,
            ));
            assert_eq!(evidence.decision, Decision::WaitForOwnedNode);
            assert_eq!(evidence.wedged_rounds, expected);
        }
        let evidence = recovery.observe(observation(
            SocketState::Unresponsive,
            Ownership::GuiOwnedAlive,
            9,
        ));
        assert_eq!(evidence.decision, Decision::RestartOwnedNode);
        assert_eq!(evidence.prior_wedged_rounds, 2);
        assert_eq!(evidence.wedged_rounds, 0);
    }
}
