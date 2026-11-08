//! What a page of the binary costs a thread when it's dropped from resident memory (goal 11's audit, row B14). kiln
//! dropped its binary's read-only pages once the server was up, and the UI's embedded files once packed, behind a
//! switch (`KILN_KEEP_PAGES`); the A/B decided they're kept, so the drop and its switch are gone. What's left is the
//! measurement that decided it: a page of the binary dropped faults on whichever thread next reads it, a shard of
//! kiln's runtime among them (a minor fault while the page cache holds the file, a read of the disk once the host
//! has reclaimed it), and a page kept doesn't. Linux, tests alone (`crate::os`).

/// Read-only data of this test binary's own, larger than four of the largest windows Linux maps around a fault
/// (64 KiB), for a shard to read. Constant bytes, so `.rodata`: never written, the file's own pages.
static SHARDS_OWN: [u8; 320 << 10] = [0xA5; 320 << 10];

/// The faults the calling thread has been charged so far, minor and major (`getrusage(RUSAGE_THREAD)`).
fn faults_here() -> anyhow::Result<u64> {
    let usage = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_THREAD)?;
    Ok(u64::try_from(usage.minor_page_faults())?.saturating_add(u64::try_from(usage.major_page_faults())?))
}

/// The whole pages within `data` out of the process's resident memory (`madvise(MADV_DONTNEED)`): what the drop did.
/// The bytes it dropped.
#[allow(unsafe_code)]
fn drop_pages(data: &[u8]) -> anyhow::Result<usize> {
    let page = rustix::param::page_size().max(1);
    let (start, end) = (data.as_ptr().addr(), data.as_ptr().addr().saturating_add(data.len()));
    let first = start.checked_next_multiple_of(page).ok_or_else(|| anyhow::anyhow!("no page within"))?;
    let last = end / page * page;
    anyhow::ensure!(last > first, "no whole page within {} bytes", data.len());
    // SAFETY: `first..last` are whole pages within `SHARDS_OWN`, a private read-only mapping of this executable's
    // constant data that nothing writes (no relocation reaches `.rodata`, no ptrace here): its pages are the file's
    // own, which `MADV_DONTNEED` takes out of the page tables and the next read maps back from the page cache with the
    // same bytes. No reference into the range is used across the call but `data`, which is only read after it.
    unsafe { rustix::mm::madvise(std::ptr::without_provenance_mut(first), last - first, rustix::mm::Advice::LinuxDontNeed) }?;
    Ok(last - first)
}

/// Goal 11's audit, row B14: a page of the binary that was dropped faults on the thread that next reads it, a shard of
/// kiln's runtime here, and a page that was kept doesn't: why the binary's pages are kept. On one shard, in one poll:
/// the binary's own read-only data read whole (so resident), then read again (kept: no fault), then dropped as the
/// drop did and read again (faults, a window at a time, and the same bytes). The faults counted are the shard's own
/// (`getrusage(RUSAGE_THREAD)`), and nothing else runs on it in between.
#[test]
fn g11_b14_a_dropped_page_faults_on_the_shard_that_touches_it_and_a_kept_one_does_not() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        anyhow::ensure!(crate::rt::on_shard(), "read on a shard");
        let data: &[u8] = std::hint::black_box(&SHARDS_OWN);
        let sum = |d: &[u8]| d.iter().map(|b| u64::from(*b)).sum::<u64>();
        let want = sum(data);
        let before = faults_here()?;
        let kept = sum(std::hint::black_box(data));
        let kept_faults = faults_here()?.saturating_sub(before);
        let dropped = drop_pages(data)?;
        let before = faults_here()?;
        let again = sum(std::hint::black_box(data));
        let dropped_faults = faults_here()?.saturating_sub(before);
        anyhow::ensure!(kept == want && again == want, "the same bytes either way");
        anyhow::ensure!(kept_faults == 0, "kept: {kept_faults} faults on the shard re-reading it");
        anyhow::ensure!(dropped_faults > 0, "{dropped} bytes dropped: {dropped_faults} faults on the shard re-reading them");
        Ok(())
    })
}
