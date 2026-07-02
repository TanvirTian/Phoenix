//! VM lifecycle state machine (§2):
//! `Created -> Booting -> Running -> Paused -> Stopped`.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    Created,
    Booting,
    Running,
    Paused,
    Stopped,
}

impl VmState {
    pub fn as_str(&self) -> &'static str {
        match self {
            VmState::Created => "Created",
            VmState::Booting => "Booting",
            VmState::Running => "Running",
            VmState::Paused => "Paused",
            VmState::Stopped => "Stopped",
        }
    }

    /// Whether `next` is a legal transition from `self`. Illegal transitions are
    /// rejected by the manager so the GUI can't drive the VM into nonsense.
    pub fn can_transition_to(self, next: VmState) -> bool {
        use VmState::*;
        matches!(
            (self, next),
            (Created, Booting)
                | (Booting, Running)
                | (Booting, Stopped)
                | (Running, Paused)
                | (Running, Stopped)
                | (Paused, Running)
                | (Paused, Stopped)
        )
    }
}

impl fmt::Display for VmState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(thiserror::Error, Debug)]
#[error("illegal VM state transition: {from} -> {to}")]
pub struct IllegalTransition {
    pub from: VmState,
    pub to: VmState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_boot_path() {
        assert!(VmState::Created.can_transition_to(VmState::Booting));
        assert!(VmState::Booting.can_transition_to(VmState::Running));
        assert!(VmState::Running.can_transition_to(VmState::Paused));
        assert!(VmState::Paused.can_transition_to(VmState::Running));
        assert!(VmState::Running.can_transition_to(VmState::Stopped));
    }

    #[test]
    fn illegal_transitions_rejected() {
        assert!(!VmState::Created.can_transition_to(VmState::Running));
        assert!(!VmState::Stopped.can_transition_to(VmState::Running));
        assert!(!VmState::Paused.can_transition_to(VmState::Booting));
    }
}
