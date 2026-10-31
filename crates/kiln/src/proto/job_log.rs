//! U05: pure version1 producer-log wire validation. These data values grant no
//! execution or publication authority; the original admission and canonical owner do.

use sha2::{Digest as _, Sha256};

use super::{JobLogBindingV1, JobLogCaptureV1, JobLogChunkV1, JobLogEndV1, JobResult};

pub(crate) const RESULT_MAX: usize = 65_536;
pub(crate) const END_MAX: usize = 98_304;
pub(crate) const WHY_MAX: usize = 4096;

/// Borrow the stable channel::Frame::Item variant0 without allocating its body.
/// Neither sender low nor this view is canonical acceptance/publication authority.
pub(crate) struct JobLogItem<'a> {
    pub stream: &'a str,
    pub seq: u64,
    pub low: u64,
    pub body: &'a [u8],
}

pub(crate) fn borrow_item(bytes: &[u8]) -> anyhow::Result<JobLogItem<'_>> {
    // Postcard accepts redundant scalar varints. Compare each small scalar with
    // its canonical encoding while leaving the potentially large body borrowed.
    fn scalar(bytes: &[u8]) -> anyhow::Result<(u64, &[u8])> {
        let (value, tail) = postcard::take_from_bytes::<u64>(bytes)?;
        let mut encoded = [0u8; 10];
        let canonical = postcard::to_slice(&value, &mut encoded)?;
        anyhow::ensure!(bytes.get(..bytes.len() - tail.len()) == Some(&*canonical), "job-log Item has a noncanonical scalar varint");
        Ok((value, tail))
    }
    let (variant, tail) = scalar(bytes)?;
    anyhow::ensure!(variant == 0, "job-log lane requires canonical channel Item variant0");
    let (length, tail) = scalar(tail)?;
    let length = usize::try_from(length)?;
    anyhow::ensure!(length > 0 && length <= 128, "job-log Item has an invalid stream length");
    let stream = std::str::from_utf8(tail.get(..length).ok_or_else(|| anyhow::anyhow!("job-log Item stream is truncated"))?)?;
    let (seq, tail) = scalar(tail.get(length..).ok_or_else(|| anyhow::anyhow!("job-log Item stream is truncated"))?)?;
    let (low, tail) = scalar(tail)?;
    let (length, body) = scalar(tail)?;
    anyhow::ensure!(usize::try_from(length)? == body.len() && body.len() <= END_MAX, "job-log Item has trailing, truncated or oversized body bytes");
    anyhow::ensure!(seq > 0 && seq < u64::MAX && low < seq, "job-log Item has an invalid sequence window");
    Ok(JobLogItem { stream, seq, low, body })
}

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut digest = Sha256::new();
    for part in parts { digest.update(part); }
    digest.finalize().into()
}

pub(crate) fn validate_binding(binding: &JobLogBindingV1) -> anyhow::Result<()> {
    binding.owner.validate().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(binding.contract.version == 1, "unsupported job-log contract version");
    anyhow::ensure!(binding.contract.capture_limit <= i64::MAX as u64, "job-log capture limit exceeds canonical numeric range");
    Ok(())
}

/// Pure pre-effect policy, not an admission grant. The agent must additionally match
/// the original authenticated connection/stamp and actual prepared native Execution.
pub(crate) fn assignment_binding(assignment: &kiln_ci::wire::AssignmentV2, caps: u64) -> anyhow::Result<Option<JobLogBindingV1>> {
    let Some(contract) = &assignment.log_contract else { return Ok(None) };
    super::check_job_log_caps(caps).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(contract.capture_limit == assignment.limits.log_bytes, "required producer capture limit differs from its original assignment");
    anyhow::ensure!(!assignment.debug.keep_on_failure_minutes.is_some_and(|minutes| minutes > 0), "required producer logs do not yet support retained debug/rerun writers");
    anyhow::ensure!(assignment.parent_binding.is_none() && assignment.executor.slot.is_none()
        && matches!(assignment.executor.kind.as_str(), "host" | "docker"), "required producer logs need their original native unpooled execution");
    let owner = assignment.ordinary_owner.clone().ok_or_else(|| anyhow::anyhow!("required producer logs have no original ordinary execution owner"))?;
    anyhow::ensure!(owner.job == assignment.job_id && owner.attempt == assignment.attempt, "required producer logs changed original job/attempt");
    let binding = JobLogBindingV1 { contract: contract.clone(), owner };
    validate_binding(&binding)?;
    Ok(Some(binding))
}

pub(crate) fn binding_id(binding: &JobLogBindingV1) -> anyhow::Result<[u8; 32]> {
    validate_binding(binding)?;
    let owner = &binding.owner;
    // FIXED V1 tuple: later optional fields cannot silently change original identity.
    let bytes = postcard::to_allocvec(&(
        binding.contract.version, binding.contract.capture_limit,
        owner.version, &owner.grant, &owner.execution, &owner.allocation, &owner.job,
        owner.attempt, &owner.host, &owner.process_nonce, &owner.publication_epoch,
        &owner.connection, &owner.command.stream, owner.command.seq,
    ))?;
    Ok(hash(&[b"kiln-job-log-binding-v1\0", &bytes]))
}

pub(crate) fn initial_root(binding_id: &[u8; 32]) -> [u8; 32] {
    hash(&[b"kiln-job-log-root-v1\0", binding_id])
}

pub(crate) fn stream(binding: &JobLogBindingV1) -> anyhow::Result<String> {
    Ok(format!("jlog:{}", hex::encode(binding_id(binding)?)))
}

pub(crate) fn stable_operation(binding_id: &[u8; 32], item_seq: u64) -> anyhow::Result<String> {
    anyhow::ensure!(item_seq > 0 && item_seq < u64::MAX, "job-log item sequence is exhausted or zero");
    Ok(hex::encode(hash(&[b"kiln-job-log-op-v1\0", binding_id, &item_seq.to_le_bytes()])))
}

pub(crate) fn validate_chunk(chunk: &JobLogChunkV1) -> anyhow::Result<()> {
    validate_binding(&chunk.binding)?;
    anyhow::ensure!(chunk.seq > 0 && chunk.seq < u64::MAX - 1, "job-log chunk sequence leaves no original End slot");
    anyhow::ensure!(!chunk.data.is_empty() && chunk.data.len() <= super::LOG_CHUNK, "job-log chunk exceeds its nonempty payload bound");
    let end = chunk.offset.checked_add(u64::try_from(chunk.data.len())?).ok_or_else(|| anyhow::anyhow!("job-log step offset overflow"))?;
    anyhow::ensure!(end <= chunk.binding.contract.capture_limit, "job-log step prefix exceeds original capture limit");
    Ok(())
}

pub(crate) fn chunk_root(chunk: &JobLogChunkV1) -> anyhow::Result<[u8; 32]> {
    validate_chunk(chunk)?;
    let payload: [u8; 32] = Sha256::digest(&chunk.data).into();
    Ok(hash(&[
        b"kiln-job-log-chunk-v1\0", &chunk.previous_root,
        &chunk.seq.to_le_bytes(), &chunk.step.to_le_bytes(), &chunk.offset.to_le_bytes(),
        &u64::try_from(chunk.data.len())?.to_le_bytes(), &payload,
    ]))
}

fn decode_result(bytes: &[u8]) -> anyhow::Result<JobResult> {
    anyhow::ensure!(bytes.len() <= RESULT_MAX, "original job result exceeds retained log End bound");
    let result: JobResult = super::read_whole(bytes).map_err(|error| anyhow::anyhow!("original job result is unreadable: {}", error.why("JobResult")))?;
    anyhow::ensure!(result.status.is_terminal(), "job-log End requires a terminal original result");
    anyhow::ensure!(result.cpu_seconds.is_none_or(|value| value.is_finite() && value >= 0.0), "original job result has invalid CPU seconds");
    anyhow::ensure!(super::encode(&result)? == bytes, "original job result is not canonical version1 encoding");
    Ok(result)
}

pub(crate) fn result_hash(bytes: &[u8]) -> anyhow::Result<[u8; 32]> {
    decode_result(bytes)?;
    Ok(hash(&[b"kiln-job-log-result-v1\0", bytes]))
}

pub(crate) fn end_item_seq(end: &JobLogEndV1) -> anyhow::Result<u64> {
    end.final_seq.checked_add(1).filter(|seq| *seq < u64::MAX).ok_or_else(|| anyhow::anyhow!("job-log End sequence exhausted"))
}

pub(crate) fn validate_end(end: &JobLogEndV1) -> anyhow::Result<JobResult> {
    validate_binding(&end.binding)?;
    end_item_seq(end)?;
    anyhow::ensure!(end.captured_bytes <= end.binding.contract.capture_limit, "job-log End exceeds original capture limit");
    anyhow::ensure!(super::encode(&super::AgentMsg::JobLogEndV1(end.clone()))?.len() <= END_MAX, "job-log End exceeds retained descriptor bound");
    let result = decode_result(&end.result)?;
    anyhow::ensure!(result.job == end.binding.owner.job, "job-log End result belongs to another original job");
    anyhow::ensure!(hash(&[b"kiln-job-log-result-v1\0", &end.result]) == end.result_hash, "job-log End result hash differs from original bytes");
    if end.final_seq == 0 {
        anyhow::ensure!(end.captured_bytes == 0 && end.root == initial_root(&binding_id(&end.binding)?), "zero-input End does not match its registered original empty producer");
    } else {
        anyhow::ensure!(end.captured_bytes >= end.final_seq, "nonempty chunk sequence cannot have fewer captured bytes");
    }
    match &end.capture {
        JobLogCaptureV1::Complete => {},
        JobLogCaptureV1::Truncated { limit, discarded_bytes } => {
            anyhow::ensure!(*limit == end.binding.contract.capture_limit && end.captured_bytes == *limit && *discarded_bytes > 0, "job-log truncation is not the original configured cap boundary");
        }
        JobLogCaptureV1::Incomplete { why, .. } => {
            anyhow::ensure!(!why.is_empty() && why.len() <= WHY_MAX, "incomplete job-log capture requires a bounded diagnostic");
        }
    }
    Ok(result)
}
