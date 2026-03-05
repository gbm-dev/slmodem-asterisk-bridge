use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Init,
    Originating,
    ConnectingMedia,
    MediaActive,
    Terminating,
    Terminated,
}

#[derive(Debug)]
pub struct Session {
    dial: String,
    state: SessionState,
}

impl Session {
    pub fn new(dial: String) -> Self {
        Self {
            dial,
            state: SessionState::Init,
        }
    }

    pub fn dial(&self) -> &str {
        &self.dial
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn transition(&mut self, next: SessionState) -> Result<()> {
        if !is_valid_transition(self.state, next) {
            bail!(
                "invalid session state transition: {:?} -> {:?}",
                self.state,
                next
            );
        }
        self.state = next;
        Ok(())
    }
}

fn is_valid_transition(from: SessionState, to: SessionState) -> bool {
    matches!(
        (from, to),
        (SessionState::Init, SessionState::Originating)
            | (SessionState::Originating, SessionState::ConnectingMedia)
            | (SessionState::ConnectingMedia, SessionState::MediaActive)
            | (SessionState::ConnectingMedia, SessionState::Terminating)
            | (SessionState::Originating, SessionState::Terminating)
            | (SessionState::MediaActive, SessionState::Terminating)
            | (SessionState::Terminating, SessionState::Terminated)
    )
}

#[cfg(test)]
mod tests {
    use super::{Session, SessionState};

    #[test]
    fn accepts_valid_transition_sequence() {
        let mut session = Session::new("15551234567".to_string());

        session.transition(SessionState::Originating).unwrap();
        session.transition(SessionState::ConnectingMedia).unwrap();
        session.transition(SessionState::MediaActive).unwrap();
        session.transition(SessionState::Terminating).unwrap();
        session.transition(SessionState::Terminated).unwrap();

        assert_eq!(session.state(), SessionState::Terminated);
    }

    #[test]
    fn rejects_invalid_transition() {
        let mut session = Session::new("15551234567".to_string());
        let err = session
            .transition(SessionState::MediaActive)
            .expect_err("must reject skipping connecting state");

        assert!(err.to_string().contains("invalid session state transition"));
        assert_eq!(session.state(), SessionState::Init);
    }
}
