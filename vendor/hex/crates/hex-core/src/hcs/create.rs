//! AC-T7.4.1/T-T7.1.2: durable native-operation ownership, separate from physical claims.
//! HCS operations cannot be reopened after creator death. The existing keeper owns dispatch and
//! writes a terminal receipt before releasing the request's native owner. Absence is no such receipt.
use super::{BoxRecord, InitRecord, NativeCreate, record_bound, record_json, write_record};
use hex_api::Refusal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub record: BoxRecord,
    pub document: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Created,
    Failed { code: u32 },
}

/// What the operation itself proves about native ownership. A terminal error after submission
/// without a returned handle is uncertainty, not absence or authority over a textual name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemOwner {
    #[default]
    Unknown,
    NotSubmitted,
    Returned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Queued,
    Dispatched { owner: InitRecord },
    Terminal { outcome: Outcome },
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub record: BoxRecord,
    pub phase: Phase,
    /// SHA-256 of exactly the UTF-8 native configuration submitted by the node (FIPS180-4).
    /// This commits queued input through dispatch/terminal recovery without another config copy.
    pub document_digest: [u8; 32],
    #[serde(default)]
    pub system_owner: SystemOwner,
}

fn invalid() -> Refusal {
    Refusal::Invalid {
        what: "native create receipt",
        why: "invalid state or immutable cleanup identity disagrees".to_owned(),
    }
}

pub fn bound() -> Result<usize, Refusal> {
    // One already-bounded marker plus one already-bounded native configuration. JSON escaping
    // multiplies a UTF-8 input byte by at most six (control byte -> \u00XX).
    record_bound()?
        .checked_add(
            crate::bundle::MAX_CONFIG_BYTES
                .checked_mul(6)
                .ok_or_else(invalid)?,
        )
        .and_then(|size| size.checked_add(b"{\"record\":,\"document\":\"\"}".len()))
        .ok_or_else(invalid)
}

impl Request {
    pub fn digest(&self) -> [u8; 32] {
        Sha256::digest(self.document.as_bytes()).into()
    }
    pub fn encode(&self) -> Result<Vec<u8>, Refusal> {
        write_record(&self.record)?;
        if self.record.creation != NativeCreate::Pending
            || self.record.system.is_none()
            || self.record.devices.is_empty()
            || self.document.is_empty()
            || self.document.contains('\0')
            || self.document.len() > crate::bundle::MAX_CONFIG_BYTES
        {
            return Err(invalid());
        }
        record_json(self, bound()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refusal> {
        if bytes.len() > bound()? {
            return Err(invalid());
        }
        let request: Self = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        request.encode()?;
        Ok(request)
    }
}

impl Receipt {
    pub fn check_request(&self, request: &Request) -> Result<(), Refusal> {
        if self.record != request.record || self.document_digest != request.digest() {
            return Err(invalid());
        }
        Ok(())
    }
    pub fn cancel_queued(&mut self, code: u32) -> Result<(), Refusal> {
        if self.phase != Phase::Queued {
            return Err(invalid());
        }
        self.phase = Phase::Terminal {
            outcome: Outcome::Failed { code },
        };
        self.system_owner = SystemOwner::NotSubmitted;
        Ok(())
    }
    pub fn dispatch(&mut self, owner: InitRecord) -> Result<(), Refusal> {
        if self.phase != Phase::Queued || owner.pid == 0 || owner.created == 0 {
            return Err(invalid());
        }
        self.phase = Phase::Dispatched { owner };
        Ok(())
    }

    pub fn complete(&mut self, outcome: Outcome) -> Result<(), Refusal> {
        if !matches!(self.phase, Phase::Dispatched { .. })
            || (outcome == Outcome::Created && self.system_owner != SystemOwner::Returned)
        {
            return Err(invalid());
        }
        self.phase = Phase::Terminal { outcome };
        Ok(())
    }

    /// Startup may resolve only a terminal result for the same immutable system/storage/bindings.
    /// A changed init/start field is ordinary lifecycle progress, never permission to change ownership.
    pub fn outcome_for(&self, record: &BoxRecord) -> Result<Option<Outcome>, Refusal> {
        if self.record.folder != record.folder
            || self.record.made != record.made
            || self.record.storage != record.storage
            || self.record.outlives != record.outlives
            || self.record.system != record.system
            || self.record.devices != record.devices
        {
            return Err(invalid());
        }
        Ok(match self.phase {
            Phase::Terminal { outcome } => Some(outcome),
            _ => None,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, Refusal> {
        write_record(&self.record)?;
        if self.record.creation != NativeCreate::Pending
            || self.record.system.is_none()
            || self.record.devices.is_empty()
        {
            return Err(invalid());
        }
        if let Phase::Dispatched { owner } = self.phase
            && (owner.pid == 0 || owner.created == 0)
        {
            return Err(invalid());
        }
        if matches!(
            self.phase,
            Phase::Terminal {
                outcome: Outcome::Created
            }
        ) && self.system_owner != SystemOwner::Returned
        {
            return Err(invalid());
        }
        record_json(self, bound()?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refusal> {
        if bytes.len() > bound()? {
            return Err(invalid());
        }
        let receipt: Self = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        receipt.encode()?;
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hcs::{DeviceBinding, RecordStorage};
    use hex_api::{BoxId, DeviceId, DeviceKind};
    fn record() -> BoxRecord {
        BoxRecord {
            folder: "owned".to_owned(),
            made: Vec::new(),
            outlives: true,
            system: Some(BoxId::new("exact").unwrap()),
            started: false,
            init: None,
            devices: vec![DeviceBinding {
                id: DeviceId {
                    kind: DeviceKind::Gpu,
                    key: 1,
                },
                location: "private-location".to_owned(),
            }],
            storage: RecordStorage::BorrowedVolume,
            creation: NativeCreate::Pending,
        }
    }
    #[test]
    fn pending_absence_and_owner_death_are_not_terminal_receipts() {
        let r = record();
        let mut receipt = Receipt {
            record: r.clone(),
            phase: Phase::Queued,
            document_digest: Sha256::digest(b"{}").into(),
            system_owner: SystemOwner::NotSubmitted,
        };
        assert_eq!(receipt.outcome_for(&r).unwrap(), None);
        assert!(receipt.complete(Outcome::Created).is_err());
        receipt.dispatch(InitRecord { pid: 3, created: 7 }).unwrap();
        assert_eq!(receipt.outcome_for(&r).unwrap(), None);
        assert!(receipt.dispatch(InitRecord { pid: 3, created: 8 }).is_err());
        receipt
            .complete(Outcome::Failed { code: 0x80004004 })
            .unwrap();
        assert_eq!(
            Receipt::decode(&receipt.encode().unwrap())
                .unwrap()
                .outcome_for(&r)
                .unwrap(),
            Some(Outcome::Failed { code: 0x80004004 })
        );
    }
    #[test]
    fn a_recycled_identity_or_changed_binding_cannot_consume_an_old_terminal_receipt() {
        let r = record();
        let receipt = Receipt {
            record: r.clone(),
            phase: Phase::Terminal {
                outcome: Outcome::Created,
            },
            document_digest: Sha256::digest(b"{}").into(),
            system_owner: SystemOwner::Returned,
        };
        let mut changed = r.clone();
        changed.devices[0].id.key = 2;
        assert!(receipt.outcome_for(&changed).is_err());
        changed = r.clone();
        changed.folder = "borrowed-sibling".to_owned();
        assert!(receipt.outcome_for(&changed).is_err());
        changed = r;
        changed.system = Some(BoxId::new("sibling").unwrap());
        assert!(receipt.outcome_for(&changed).is_err());
    }
}
