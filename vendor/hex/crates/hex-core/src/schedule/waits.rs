//! AC-T7.4.1/decision 9: scheduler answers learn one wait per actual operation, so a fast status
//! never supplies the patience for creating or deleting a local box. This is wait classification
//! only; the peer core remains the sole parser/authority for placement messages.

use super::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum WaitKind {
    Describe,
    Advertise,
    Submit,
    HearCreate,
    HearDelete,
    HearControl,
    LinkUp,
    Retry,
    Delete,
    Status,
    /// The person's release of a capacity holder (joint plan step 13, CV6): its records' removal.
    Forget,
}

impl WaitKind {
    pub const COUNT: usize = Self::Forget as usize + 1;

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn classify(command: &Command) -> Self {
        match command {
            Command::Describe { .. } => Self::Describe,
            Command::Advertise { .. } => Self::Advertise,
            Command::Submit { .. } => Self::Submit,
            Command::Hear { packet, .. } => Self::peer(&packet.message),
            Command::LinkUp { .. } => Self::LinkUp,
            Command::Retry => Self::Retry,
            Command::Delete { .. } => Self::Delete,
            Command::Status { .. } => Self::Status,
            Command::Forget { .. } => Self::Forget,
        }
    }

    fn peer(message: &str) -> Self {
        // Exact version and tag tokens from peer's p1 codec; a malformed/unknown message keeps
        // the control wait and is still refused by the actual protocol parser later.
        let mut words = message.split(' ');
        match (words.next(), words.next()) {
            (Some("p1"), Some("start")) => Self::HearCreate,
            (Some("p1"), Some("delete")) => Self::HearDelete,
            _ => Self::HearControl,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::Key;

    /// AC-T7.4.1: a local deletion, retry and status keep separate learned response histories.
    #[test]
    fn local_control_operations_do_not_share_creation_or_deletion_waits() {
        let key = Key {
            owner: 0,
            epoch: 1,
            seq: 0,
        };
        assert_eq!(
            WaitKind::classify(&Command::Status { key }),
            WaitKind::Status
        );
        assert_eq!(
            WaitKind::classify(&Command::Delete { key }),
            WaitKind::Delete
        );
        assert_eq!(WaitKind::classify(&Command::Retry), WaitKind::Retry);
        assert_eq!(
            WaitKind::classify(&Command::LinkUp { peer: 1 }),
            WaitKind::LinkUp
        );
        let payload = crate::schedule::Payload {
            spec: hex_api::BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: hex_api::BoxId::new("wait-classification").unwrap(),
                bounds: hex_api::Bounds::default(),
                class: hex_api::ClassSpec::Process,
                owner: None,
                band: 0,
            },
            start: false,
        };
        assert_eq!(
            WaitKind::classify(&Command::Submit {
                demand: "class:oci".into(),
                payload
            }),
            WaitKind::Submit
        );
    }

    /// AC-T7.4.1: only exact p1 start/delete tags select the physical runtime-operation waits.
    #[test]
    fn peer_create_and_delete_are_distinguished_from_answers_and_other_versions() {
        assert_eq!(
            WaitKind::peer("p1 start 0 1 2 d1 plain oci"),
            WaitKind::HearCreate
        );
        assert_eq!(
            WaitKind::peer("p1 delete 0 1 2 d1 plain oci"),
            WaitKind::HearDelete
        );
        for text in [
            "p1 started 0 1 2",
            "p1 ended 0 1 2",
            "p1 held 0 1 2 1 0 1",
            "p1 holds 0 1 2",
            "p1 forget 0 1 2",
            "p1 refused 0 1 2",
            "p2 start 0 1 2",
            "start 0 1 2",
            "p1  start 0 1 2",
            "",
        ] {
            assert_eq!(WaitKind::peer(text), WaitKind::HearControl, "{text:?}");
        }
    }
}
