//! Optional fixed-role prefix in the EXISTING measurement evidence. The loader
//! hashes every original byte before parsing this section; it never rewrites an
//! old evidence document or changes a legacy RuntimeCost identity.

use hex_api::Refusal;
use shards_api::{ImageIdentity, NativeImages};

use super::{exhausted, hash, invalid};

const NAME: &[u8] = b"hex-shards-native-images";
const HEADER: &[u8] = b"hex-shards-native-images 1";

fn image(row: &[u8], role: &str, optional: bool) -> Result<Option<ImageIdentity>, Refusal> {
    let text =
        std::str::from_utf8(row).map_err(|_| invalid("native-image section is not ASCII"))?;
    let mut words = text.split(' ');
    if words.next() != Some(role) {
        return Err(invalid(
            "native-image roles are missing, repeated or out of order",
        ));
    }
    let size = words
        .next()
        .ok_or_else(|| invalid("native-image size is missing"))?;
    if optional && size == "none" && words.next().is_none() {
        return Ok(None);
    }
    if size.is_empty()
        || !size.bytes().all(|byte| byte.is_ascii_digit())
        || (size.len() > 1 && size.starts_with('0'))
    {
        return Err(invalid(
            "native-image size is not a canonical unsigned integer",
        ));
    }
    let bytes = size
        .parse::<u64>()
        .map_err(|_| invalid("native-image size exceeds u64"))?;
    let digest = hash(
        words
            .next()
            .ok_or_else(|| invalid("native-image digest is missing"))?,
    )?;
    if words.next().is_some() {
        return Err(invalid("native-image row has extra fields"));
    }
    ImageIdentity::new(bytes, digest)
        .map(Some)
        .map_err(|_| invalid("native-image declaration requires a positive exact byte count"))
}

pub(super) fn parse(bytes: &[u8], evidence: [u8; 32]) -> Result<Option<NativeImages>, Refusal> {
    if bytes.len() > hex_core::bundle::MAX_CONFIG_BYTES {
        return Err(exhausted());
    }
    if !bytes.starts_with(NAME) {
        return Ok(None);
    }
    // Header + four fixed roles + empty separator + one opaque original body.
    // splitn never walks the measurement body's nested/user-scaled structure.
    let mut lines = bytes.splitn(7, |byte| *byte == b'\n');
    if lines.next() != Some(HEADER) {
        return Err(invalid(
            "native-image evidence header is malformed or unknown",
        ));
    }
    let mut next = |role, optional| {
        image(
            lines
                .next()
                .ok_or_else(|| invalid("native-image section is truncated"))?,
            role,
            optional,
        )
    };
    let cli = next("cli", false)?.ok_or_else(|| invalid("native CLI image is missing"))?;
    let kernel = next("kernel", false)?.ok_or_else(|| invalid("native kernel image is missing"))?;
    let cpu = next("cpu-vmm", true)?;
    let gpu = next("gpu-vmm", true)?;
    if lines.next() != Some(b"".as_slice()) || lines.next().is_none_or(|body| body.is_empty()) {
        return Err(invalid(
            "native-image section lacks the exact separator or measurement body",
        ));
    }
    NativeImages::new(evidence, cli, kernel, cpu, gpu)
        .map(Some)
        .map_err(|_| {
            invalid("native-image section has no selected VMM declaration or evidence identity")
        })
}

#[cfg(test)]
mod tests;
