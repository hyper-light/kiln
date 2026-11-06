//! The calling thread's own stack in resident memory, on Linux (goal 11, plan 01a1a87b2e56 step 5).
//!
//! A thread's stack is a mapping its C library made as the thread started: kiln's are 16 MiB (`main`'s
//! `THREAD_STACK`, for the runtime's deep futures), reserved, not resident; the kernel makes a page resident as
//! the thread first touches it, and it stays so for the thread's life, however shallow the thread runs after.
//! In the lab's realistic profile (alloc2, musl-jemalloc, smaps at the window's end) the server's 91 threads'
//! stacks held 5.8 MB: the four shards 0.40 to 0.75 MB each, 16 threads 88 KB, the rest 12 to 20 KB. Its size is
//! no lever on what's resident: a thread holds the pages it reached, whatever its reservation. Two things are:
//!
//! - **Huge pages.** Where the kernel gives transparent huge pages to every anonymous mapping (`always`, as on
//!   adacachy, production's host), a fault in a 2 MiB-aligned range that lies wholly in a stack's mapping takes a
//!   whole huge page, and khugepaged collapses such a range once 103 of its 512 pages are in use (adacachy's
//!   `max_ptes_none`, 409): a shard's 0.4 to 0.75 MB becomes 2 MB. Linux keeps huge pages off a mapping made with
//!   `MAP_STACK` (glibc's stacks), but musl's `pthread_create`, production's, maps without it. So a runtime
//!   thread marks its own stack `MADV_NOHUGEPAGE` as it starts ([`no_huge_pages`]). Where the kernel gives them
//!   only when asked (`madvise`, as on the lab's fedora) it changes nothing.
//! - **Pages a worker no longer reaches.** A pool's worker that ran a deep job keeps every page the job touched
//!   while it waits. A worker gone cold (`hyper_rt::idle`) gives back what lies below its own frame
//!   ([`give_back_unused`]): its next deep job touches them again, each a minor fault on a zero page.

use std::io;
use std::ops::Range;

/// The calling thread's stack: the addresses it may grow through, from its lowest (above its guard) to its top,
/// as its C library has them (pthread_getattr_np(3)); none where it doesn't say.
#[allow(unsafe_code)]
pub fn bounds() -> Option<Range<usize>> {
    use nix::libc;
    let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
    // SAFETY: pthread_getattr_np initializes `attr`, this frame's, with the calling thread's attributes and answers
    // 0, or answers an error and leaves it as it was.
    if unsafe { libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) } != 0 {
        return None;
    }
    let mut low: *mut libc::c_void = std::ptr::null_mut();
    let mut size: libc::size_t = 0;
    // SAFETY: `attr` is initialized (the call above answered 0); pthread_attr_getstack writes the stack's lowest
    // address and its size to `low` and `size`, both this frame's.
    let got = unsafe { libc::pthread_attr_getstack(attr.as_ptr(), &raw mut low, &raw mut size) };
    // SAFETY: `attr` is initialized and destroyed once, here; nothing reads it after.
    let _ = unsafe { libc::pthread_attr_destroy(attr.as_mut_ptr()) };
    let low = low.addr();
    (got == 0 && low != 0 && size > 0).then(|| low.checked_add(size).map(|top| low..top)).flatten()
}

/// The size of a page.
fn page_size() -> usize {
    rustix::param::page_size().max(1)
}

/// The whole pages of the calling thread's stack, as [`bounds`] says it: from its lowest page up to the page its
/// top lies in, which may hold the thread's own control block (musl's and glibc's lie just above it).
fn whole(stack: &Range<usize>, page: usize) -> Option<Range<usize>> {
    let start = stack.start.checked_next_multiple_of(page)?;
    let end = stack.end / page * page;
    (start < end).then_some(start..end)
}

/// Marks the calling thread's stack as wanting no transparent huge pages (`MADV_NOHUGEPAGE`, madvise(2)): what a
/// runtime thread does as it starts, so that its stack, where the kernel gives huge pages to every anonymous
/// mapping, holds the pages it touches rather than a 2 MiB page around them. An error where the kernel has no
/// huge pages to say this of (`EINVAL`), or the C library doesn't say where the stack is.
#[allow(unsafe_code)]
pub fn no_huge_pages() -> io::Result<()> {
    let stack = bounds().ok_or_else(|| io::Error::other("the C library didn't say where this thread's stack is"))?;
    let pages = whole(&stack, page_size()).ok_or_else(|| io::Error::other("a stack of less than a page"))?;
    // SAFETY: `pages` lie in the calling thread's own stack (its C library's word), whole pages of it. Advising them
    // `MADV_NOHUGEPAGE` sets a flag on their mapping (the kernel splits the mapping at `pages`' ends if they don't
    // match it) and changes no byte of them: no reference into the stack sees a change.
    unsafe { rustix::mm::madvise(std::ptr::without_provenance_mut(pages.start), pages.end - pages.start, rustix::mm::Advice::LinuxNoHugepage) }.map_err(io::Error::from)
}

/// Gives back the pages of the calling thread's stack below its current frame (`MADV_DONTNEED`, madvise(2)): what
/// the thread touched at its deepest and doesn't reach now. Its next touch of one is a minor fault on a zero page,
/// as its first was. What a pool's worker does once it's cold (`hyper_rt::idle`), from its idle loop, the
/// shallowest it runs. The bytes it gave back, resident or not (none where the C library doesn't say where the
/// stack is, or nothing lies below the frame).
///
/// It keeps the page this frame's marker lies in and the one below it: the call into the kernel runs below this
/// frame (rustix's two functions, a few hundred bytes of a debug build's stack at the most, and x86-64's 128-byte
/// red zone), all of it within that page. Never inlined, so the marker lies in this function's own frame, below
/// every frame of its caller.
#[inline(never)]
#[allow(unsafe_code)]
pub fn give_back_unused() -> usize {
    let marker = std::hint::black_box(0_u8);
    let here = (&raw const marker).addr();
    let page = page_size();
    let Some(stack) = bounds() else {
        return 0;
    };
    let Some(start) = stack.start.checked_next_multiple_of(page) else {
        return 0;
    };
    let end = (here / page * page).saturating_sub(page);
    if !stack.contains(&here) || end <= start {
        return 0;
    }
    // SAFETY: `start..end` are whole pages of the calling thread's own stack (its C library's word, and `here`, a
    // local of this frame, lies in it) below this frame's page and the page below that. A thread's frames lie at
    // and above its stack pointer, and this frame's callees (the call into the kernel) lie within the page kept
    // below it, so no frame of this thread is there, and no other thread refers to a thread's stack below its
    // frames. For such private anonymous memory MADV_DONTNEED drops the pages: a later touch, by this thread's later
    // calls, finds zero pages, as on its first touch.
    let given = unsafe { rustix::mm::madvise(std::ptr::without_provenance_mut(start), end - start, rustix::mm::Advice::LinuxDontNeed) };
    std::hint::black_box(marker);
    if given.is_ok() { end - start } else { 0 }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileExt as _;

    use super::*;

    /// One entry of `/proc/self/pagemap`, a page's, and its present bit (Documentation/admin-guide/mm/pagemap.rst).
    const ENTRY: usize = std::mem::size_of::<u64>();
    const PRESENT: u64 = 1 << 63;

    /// The resident pages of `range`, as pagemap says.
    fn resident(range: &Range<usize>) -> io::Result<usize> {
        let page = page_size();
        let pages = range.end.saturating_sub(range.start) / page;
        let mut buf = vec![0_u8; pages * ENTRY];
        let pagemap = std::fs::File::open("/proc/self/pagemap")?;
        pagemap.read_exact_at(&mut buf, u64::try_from(range.start / page * ENTRY).map_err(io::Error::other)?)?;
        Ok(buf.as_chunks::<ENTRY>().0.iter().filter(|c| u64::from_ne_bytes(**c) & PRESENT != 0).count())
    }

    /// Touches `depth` bytes of the stack below the caller, a page at a time, in frames of 16 KiB.
    #[inline(never)]
    fn reach(depth: usize) -> usize {
        let mut frame = [0_u8; 16 << 10];
        for at in (0..frame.len()).step_by(page_size()) {
            if let Some(b) = frame.get_mut(at) {
                *b = 1;
            }
        }
        std::hint::black_box(&mut frame);
        let below = if depth > frame.len() { reach(depth - frame.len()) } else { 0 };
        below + usize::from(std::hint::black_box(frame).first().copied().unwrap_or(0))
    }

    /// A local's address: within the caller's frame.
    #[inline(never)]
    fn here() -> usize {
        let marker = std::hint::black_box(0_u8);
        (&raw const marker).addr()
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: the C library says where the calling thread's stack is, and it holds the
    /// thread's frames and at least the room it was asked for.
    #[test]
    fn g11_s5_a_threads_stack_is_where_its_frames_are() -> anyhow::Result<()> {
        const ASKED: usize = 4 << 20;
        let (stack, at) = std::thread::Builder::new().stack_size(ASKED).spawn(|| (bounds(), here()))?.join().map_err(|_| anyhow::anyhow!("the thread panicked"))?;
        let stack = stack.ok_or_else(|| anyhow::anyhow!("no stack said"))?;
        anyhow::ensure!(stack.contains(&at), "{at:#x} in {stack:x?}");
        anyhow::ensure!(stack.end - stack.start >= ASKED - (64 << 10), "{stack:x?} for {ASKED} asked");
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a thread that reached deep and came back gives back the pages below its
    /// frame, and runs on (its next deep call finds zero pages, as its first did).
    #[test]
    fn g11_s5_a_cold_threads_stack_gives_back_the_pages_below_its_frame() -> anyhow::Result<()> {
        const DEEP: usize = 1 << 20;
        let measured = std::thread::Builder::new()
            .stack_size(4 << 20)
            .spawn(|| -> io::Result<(usize, usize, usize, usize)> {
                let stack = bounds().ok_or_else(|| io::Error::other("no stack said"))?;
                // Below 64 KiB under this frame: room for the frames of this test's own calls (a debug build's),
                // which touch pages again as they run.
                let below = stack.start..here().saturating_sub(64 << 10) / page_size() * page_size();
                let _ = reach(DEEP);
                let before = resident(&below)?;
                let given = give_back_unused();
                let after = resident(&below)?;
                let again = reach(DEEP);
                Ok((before, given, after, again))
            })?
            .join()
            .map_err(|_| anyhow::anyhow!("the thread panicked"))??;
        let (before, given, after, again) = measured;
        let pages = (DEEP - (128 << 10)) / page_size();
        anyhow::ensure!(before >= pages, "{before} pages resident below the frame after reaching {DEEP} bytes deep");
        anyhow::ensure!(given >= DEEP - (128 << 10), "gave back {given} bytes");
        anyhow::ensure!(after == 0, "{after} pages resident below the frame once given back");
        anyhow::ensure!(again == DEEP.div_ceil(16 << 10), "the thread ran on, reaching as deep again: {again}");
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a thread's stack marked so takes no transparent huge page (`nh` in its
    /// mapping's flags, proc(5)'s smaps), where the kernel has them to say this of.
    #[test]
    fn g11_s5_a_runtime_threads_stack_wants_no_huge_pages() -> anyhow::Result<()> {
        let flags = std::thread::Builder::new()
            .stack_size(4 << 20)
            .spawn(|| -> io::Result<Option<String>> {
                if no_huge_pages().is_err() {
                    // No huge pages in this kernel (EINVAL): nothing to keep off the stack.
                    return Ok(None);
                }
                // The stack's middle: in its whole pages, which the advice covers (its top page may hold this
                // frame and the thread's control block, a mapping split off it).
                let stack = bounds().ok_or_else(|| io::Error::other("no stack said"))?;
                let at = stack.start + (stack.end - stack.start) / 2;
                let smaps = std::fs::read_to_string("/proc/self/smaps")?;
                let mut within = false;
                for line in smaps.lines() {
                    if let Some((range, _)) = line.split_once(' ')
                        && let Some((a, b)) = range.split_once('-')
                        && let (Ok(a), Ok(b)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16))
                    {
                        within = (a..b).contains(&at);
                    } else if within && let Some(flags) = line.strip_prefix("VmFlags:") {
                        return Ok(Some(flags.to_owned()));
                    }
                }
                Err(io::Error::other("this thread's stack isn't in smaps"))
            })?
            .join()
            .map_err(|_| anyhow::anyhow!("the thread panicked"))??;
        if let Some(flags) = flags {
            anyhow::ensure!(flags.split_whitespace().any(|f| f == "nh"), "the stack's mapping flags: {flags}");
        }
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a stack's whole pages, its top's partial page left out.
    #[test]
    fn g11_s5_a_stacks_whole_pages() {
        assert_eq!(whole(&(4096..16384), 4096), Some(4096..16384));
        assert_eq!(whole(&(4096..16000), 4096), Some(4096..12288));
        assert_eq!(whole(&(4000..8192), 4096), Some(4096..8192));
        assert_eq!(whole(&(4096..8000), 4096), None);
    }
}
