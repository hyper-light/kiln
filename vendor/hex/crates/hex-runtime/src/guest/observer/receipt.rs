//! Two borrowed views of real VM receipts. This owns no record, native handle,
//! grant, observer process, device selection or retained state of its own.

use std::path::Path;

use hex_api::Refusal;
use hex_core::record::NativeScope;

use super::{Attempt, Evidence};
use crate::guest::{pool, selected};

pub(super) enum Receipt<'a> {
    Pool(&'a mut pool::Receipt),
    Selected(&'a mut selected::Record),
}

impl Receipt<'_> {
    pub fn scope(&self) -> Option<NativeScope> {
        match self {
            Self::Pool(receipt) => Some(receipt.original.scope()),
            Self::Selected(receipt) => receipt.scope,
        }
    }

    pub fn count(&self) -> usize {
        match self {
            Self::Pool(receipt) => receipt.physical.len(),
            Self::Selected(receipt) => receipt.physical.len(),
        }
    }

    pub fn attempt(&self) -> Option<&Attempt> {
        match self {
            Self::Pool(receipt) => receipt.attempt.as_ref(),
            Self::Selected(receipt) => receipt.attempt.as_ref(),
        }
    }

    pub fn attempt_mut(&mut self) -> Option<&mut Attempt> {
        match self {
            Self::Pool(receipt) => receipt.attempt.as_mut(),
            Self::Selected(receipt) => receipt.attempt.as_mut(),
        }
    }

    pub fn set_attempt(&mut self, attempt: Attempt) {
        match self {
            Self::Pool(receipt) => receipt.attempt = Some(attempt),
            Self::Selected(receipt) => receipt.attempt = Some(attempt),
        }
    }

    pub fn readiness(&self) -> Option<&Evidence> {
        match self {
            Self::Pool(receipt) => receipt.readiness.as_ref(),
            Self::Selected(receipt) => receipt.readiness.as_ref(),
        }
    }

    pub fn set_readiness(&mut self, readiness: Evidence) {
        match self {
            Self::Pool(receipt) => receipt.readiness = Some(readiness),
            Self::Selected(receipt) => receipt.readiness = Some(readiness),
        }
    }

    pub fn encode(&self) -> Result<String, Refusal> {
        match self {
            Self::Pool(receipt) => receipt.encode(),
            Self::Selected(receipt) => receipt.encode(),
        }
    }

    pub fn bound(&self) -> usize {
        match self {
            Self::Pool(_) => hex_core::record::MAX_RECORD_BYTES,
            Self::Selected(_) => hex_core::bundle::MAX_CONFIG_BYTES,
        }
    }

    pub fn write(&self, path: &Path) -> Result<(), Refusal> {
        match self {
            Self::Pool(receipt) => pool::write(path, receipt),
            Self::Selected(receipt) => selected::write(path, receipt),
        }
    }

    pub fn checked_readiness(&self) -> Result<(), Refusal> {
        match self {
            Self::Pool(receipt) => receipt.checked_work_ready().map(|_| ()),
            Self::Selected(receipt) => receipt.checked_work_ready().map(|_| ()),
        }
    }
}
