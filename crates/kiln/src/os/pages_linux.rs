//! This binary's own pages in resident memory, on Linux (goal 11, plan 01a1a87b2e56 step 5): which of the
//! executable's mappings are read-only and backed by its file (`/proc/self/maps`), which of their pages are
//! resident and still the file's own (`/proc/self/pagemap`), and taking those out of the process's resident
//! set (`madvise(MADV_DONTNEED)`).
//!
//! Why: a fault on the binary maps the 64 KiB around it that the page cache holds (the kernel's fault-around,
//! `fault_around_bytes`), and the server reads its whole binary into the page cache as it starts (its build
//! hash, `crate::updates::own_binary`). So every 64 KiB window of code or read-only data the server touched
//! once stays in its resident memory: in the lab's realistic profile (train 4's musl build, 177 MiB of
//! `.text`), 102 MB of its code and 18 MB of its read-only data, about half of each, at the window's end.
//!
//! A page of a private, read-only mapping of the executable is either the file's own (a page-cache page: the
//! bytes the file holds) or, if something wrote it through ptrace (a debugger's breakpoint, a uprobe), an
//! anonymous copy. `MADV_DONTNEED` takes either out of the page tables. The file's own comes back from the
//! page cache on the next touch, unchanged, as it does after the kernel's own reclaim takes it under memory
//! pressure; an anonymous copy would come back as the file's bytes, losing what was written. So only runs of
//! the file's own pages are dropped, as pagemap says each page is, and nothing writable is ever touched.
//!
//! What it costs: the next touch of a dropped page is a minor fault (the page cache still holds it, and the
//! fault maps its 64 KiB window back), a few microseconds; a major fault, a read of the file, if the page
//! cache let the page go meanwhile, which it does sooner for pages no process maps.

use std::io;
use std::ops::Range;
use std::os::unix::fs::{FileExt as _, MetadataExt as _};

/// pagemap's bits for a page (Documentation/admin-guide/mm/pagemap.rst): present in RAM...
const PRESENT: u64 = 1 << 63;
/// ...swapped out...
const SWAPPED: u64 = 1 << 62;
/// ...and a page of a file (or shared anonymous memory), not a private anonymous copy.
const FILE: u64 = 1 << 61;
/// One entry of pagemap, a page's.
const ENTRY: usize = std::mem::size_of::<u64>();

/// Whether this platform can take pages out of resident memory.
pub const CAN: bool = true;

/// What a drop took out of resident memory.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dropped {
    /// The resident pages of the file's own it dropped, in bytes.
    pub bytes: u64,
    /// The runs of pages it dropped, a call each.
    pub runs: usize,
    /// Resident pages it left because they weren't the file's own (written through ptrace), in bytes.
    pub kept: u64,
}

impl Dropped {
    fn add(&mut self, other: Dropped) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.runs = self.runs.saturating_add(other.runs);
        self.kept = self.kept.saturating_add(other.kept);
    }
}

/// The size of a page.
pub fn page_size() -> usize {
    rustix::param::page_size().max(1)
}

/// One line of `/proc/self/maps`: a mapping, whether it may be written and whether it's private, and the
/// file behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub start: usize,
    pub end: usize,
    pub write: bool,
    pub private: bool,
    pub inode: u64,
    pub path: String,
}

/// `line` of `/proc/<pid>/maps` (`start-end perms offset dev inode path`), if it reads as one.
fn mapping(line: &str) -> Option<Mapping> {
    let mut fields = line.splitn(6, ' ');
    let (range, perms, _offset, _dev, inode) = (fields.next()?, fields.next()?, fields.next()?, fields.next()?, fields.next()?);
    let path = fields.next().map_or("", str::trim_start).to_owned();
    let (start, end) = range.split_once('-')?;
    let perms = perms.as_bytes();
    Some(Mapping {
        start: usize::from_str_radix(start, 16).ok()?,
        end: usize::from_str_radix(end, 16).ok()?,
        write: perms.get(1) == Some(&b'w'),
        private: perms.get(3) == Some(&b'p'),
        inode: inode.parse().ok()?,
        path,
    })
}

/// The mappings of `maps` (`/proc/self/maps`) of the file at `exe` (`/proc/self/exe`'s target, with the
/// " (deleted)" maps says of it too) whose inode is `inode`, private and read-only: the binary's code and
/// read-only data. Never one that may be written (its data, or its relocations before they're made read-only),
/// whose pages a drop would lose. A path alone could name a newer file put in its place, and an inode alone
/// another file system's file; the device isn't compared, as a btrfs subvolume's file says another in `stat`
/// than in maps.
fn own_readonly(maps: &str, exe: &str, inode: u64) -> Vec<Mapping> {
    maps.lines().filter_map(mapping).filter(|m| m.private && !m.write && m.inode == inode && m.path == exe && m.end > m.start).collect()
}

/// This binary's own private, read-only mappings, from `/proc/self/maps`.
pub fn own_mappings() -> io::Result<Vec<Mapping>> {
    let exe = std::fs::read_link("/proc/self/exe")?;
    let inode = std::fs::metadata("/proc/self/exe")?.ino();
    let maps = std::fs::read_to_string("/proc/self/maps")?;
    Ok(own_readonly(&maps, &exe.to_string_lossy(), inode))
}

/// The runs of `entries` (pagemap's, for the pages from `start` on, `page` bytes each) that a drop may take:
/// pages resident and of the file, with the pages not resident between them (nothing to drop there, and one
/// call instead of two). A page that's resident but not the file's (an anonymous copy), or swapped, ends a
/// run and is kept. Each run starts and ends on a resident page of the file's. With the runs, the bytes they
/// hold resident and the bytes kept.
fn runs(entries: &[u64], start: usize, page: usize) -> (Vec<Range<usize>>, u64, u64) {
    let (mut out, mut held, mut kept) = (Vec::new(), 0u64, 0u64);
    let mut run: Option<Range<usize>> = None;
    let page64 = u64::try_from(page).unwrap_or(u64::MAX);
    for (i, e) in entries.iter().enumerate() {
        let at = start.saturating_add(i.saturating_mul(page));
        let resident = e & PRESENT != 0;
        if e & SWAPPED != 0 || (resident && e & FILE == 0) {
            kept = kept.saturating_add(if resident { page64 } else { 0 });
            out.extend(run.take());
        } else if resident {
            held = held.saturating_add(page64);
            let end = at.saturating_add(page);
            run = Some(run.map_or(at..end, |r| r.start..end));
        }
    }
    out.extend(run);
    (out, held, kept)
}

/// The pages of `ranges` (addresses) that lie in `mappings` (each private and read-only, of a file), out of
/// resident memory where pagemap says they're the file's own. pagemap is read a page of its entries at a time
/// (the entries a page table holds), and each run found in them dropped.
fn drop_within(ranges: &[Range<usize>], mappings: &[Mapping]) -> io::Result<Dropped> {
    let page = page_size();
    let pagemap = std::fs::File::open("/proc/self/pagemap")?;
    let per_read = (page / ENTRY).max(1);
    let mut buf = vec![0u8; per_read.saturating_mul(ENTRY)];
    let mut done = Dropped::default();
    for range in ranges {
        for m in mappings {
            // The range's whole pages within the mapping.
            let from = range.start.max(m.start).checked_next_multiple_of(page).unwrap_or(usize::MAX);
            let to = range.end.min(m.end) / page * page;
            let mut at = from;
            while at < to {
                let pages = ((to - at) / page).min(per_read);
                let bytes = buf.get_mut(..pages.saturating_mul(ENTRY)).ok_or_else(|| io::Error::other("pagemap's buffer"))?;
                pagemap.read_exact_at(bytes, u64::try_from(at / page * ENTRY).map_err(io::Error::other)?)?;
                let entries: Vec<u64> = bytes.as_chunks::<ENTRY>().0.iter().map(|c| u64::from_ne_bytes(*c)).collect();
                let (found, held, kept) = runs(&entries, at, page);
                for run in &found {
                    dontneed(run, m)?;
                }
                done.add(Dropped { bytes: held, runs: found.len(), kept });
                at = at.saturating_add(pages.saturating_mul(page));
            }
        }
    }
    Ok(done)
}

/// `run` out of this process's page tables.
#[allow(unsafe_code)]
fn dontneed(run: &Range<usize>, within: &Mapping) -> io::Result<()> {
    if run.start < within.start || run.end > within.end || run.start >= run.end || within.write || !within.private {
        return Err(io::Error::other("a run outside a private, read-only mapping"));
    }
    // SAFETY: `run` lies in `within`, a private mapping of a file that this process can't write through (no
    // write permission, as `/proc/self/maps` said; nothing in kiln maps, unmaps or changes the protection of
    // its own binary once the loader is done, so it still is), and pagemap said a moment ago that each of its
    // resident pages is the file's own (a page-cache page), none an anonymous copy. For such pages MADV_DONTNEED only
    // takes this process's page-table entries away: the next access, by any thread, reading or running them,
    // faults the same bytes back from the page cache (or the file), as after the kernel's own reclaim. So no
    // reference or code into them sees a change. The one race is a page written through ptrace (a uprobe, a
    // debugger's breakpoint) between that read and this call: its copy would be dropped, and the breakpoint
    // with it.
    unsafe { rustix::mm::madvise(std::ptr::without_provenance_mut(run.start), run.end - run.start, rustix::mm::Advice::LinuxDontNeed) }.map_err(io::Error::from)
}

/// The pages of `ranges` (addresses) that lie in this binary's own read-only mappings, out of resident memory
/// where they're still the file's own.
fn drop_own(ranges: &[Range<usize>]) -> io::Result<Dropped> {
    drop_within(ranges, &own_mappings()?)
}

/// The whole pages of `spans` (each an address and a length), spans less than a page apart taken as one:
/// the bytes between two such spans share a page with them, and a drop leaves such bytes as it found them
/// (they're the same file's, read-only, and come back as they were if touched). In address order.
fn whole_pages(spans: &[(usize, usize)], page: usize) -> Vec<Range<usize>> {
    let mut sorted: Vec<Range<usize>> = spans.iter().filter(|(_, len)| *len > 0).map(|(at, len)| *at..at.saturating_add(*len)).collect();
    sorted.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for r in sorted {
        match merged.last_mut() {
            Some(last) if r.start.saturating_sub(last.end) < page => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    merged.into_iter().filter_map(|r| Some(r.start.checked_next_multiple_of(page)?..r.end / page * page)).filter(|r| r.start < r.end).collect()
}

/// The whole pages of `spans` (each an address and a length, in this binary's own read-only data: its
/// embedded files, say) out of resident memory where they're still the file's own; see [`whole_pages`].
pub fn drop_spans(spans: &[(usize, usize)]) -> io::Result<Dropped> {
    drop_own(&whole_pages(spans, page_size()))
}

/// Every page of this binary's own read-only mappings (its code and read-only data) out of resident memory
/// where it's still the file's own.
pub fn drop_all_own() -> io::Result<Dropped> {
    let mut done = Dropped::default();
    for m in own_mappings()? {
        done.add(drop_within(std::slice::from_ref(&(m.start..m.end)), std::slice::from_ref(&m))?);
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 11, plan 01a1a87b2e56 step 5: a maps line reads as its mapping, a deleted binary's included.
    #[test]
    fn g11_s5_a_maps_line_reads_as_its_mapping() {
        let m = mapping("04632000-0f747000 r-xp 03631000 00:23 482401212                          /home/a/kiln");
        assert_eq!(m, Some(Mapping { start: 0x0463_2000, end: 0x0f74_7000, write: false, private: true, inode: 482_401_212, path: "/home/a/kiln".into() }));
        let d = mapping("7f00-8f00 r--p 00000000 103:02 77   /usr/bin/kiln (deleted)").map(|m| m.path);
        assert_eq!(d.as_deref(), Some("/usr/bin/kiln (deleted)"));
        let anon = mapping("7f00-8f00 rw-p 00000000 00:00 0 ").map(|m| (m.write, m.inode, m.path));
        assert_eq!(anon, Some((true, 0, String::new())));
        assert_eq!(mapping("not a mapping"), None);
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: only the binary's own private, read-only mappings are taken: never
    /// its data (writable), another file of its inode or its path, or a shared mapping.
    #[test]
    fn g11_s5_only_the_binarys_own_read_only_mappings_are_taken() {
        let maps = "\
01000000-04632000 r--p 00000000 00:23 9 /k/kiln
04632000-0f747000 r-xp 03631000 00:23 9 /k/kiln
0f747000-0fa2e000 rw-p 0e745000 00:23 9 /k/kiln
0fa2e000-0fe91000 rw-p 0ea2b000 00:23 9 /k/kiln
10000000-10001000 r--s 00000000 00:23 9 /k/kiln
20000000-20001000 r--p 00000000 00:24 9 /lib/other
30000000-30001000 r-xp 00000000 00:23 8 /k/kiln
7efc84858000-7efc84860000 rw-s 00000000 00:23 5 /k/data/kiln.db-shm
";
        let own: Vec<(usize, usize)> = own_readonly(maps, "/k/kiln", 9).iter().map(|m| (m.start, m.end)).collect();
        assert_eq!(own, [(0x0100_0000, 0x0463_2000), (0x0463_2000, 0x0f74_7000)]);
        let gone = "01000000-04632000 r-xp 00000000 00:23 9 /k/kiln (deleted)\n";
        assert_eq!(own_readonly(gone, "/k/kiln (deleted)", 9).len(), 1, "an upgraded binary, still running, is still its own");
        assert!(own_readonly(gone, "/k/kiln", 9).is_empty(), "a newer file at its path is another binary");
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: runs are made of the file's own resident pages, with the pages not
    /// resident between them; a page written through ptrace (anonymous), or swapped, is never in one.
    #[test]
    fn g11_s5_runs_hold_the_files_own_pages_and_never_a_written_one() {
        let (file, none, anon, swapped) = (PRESENT | FILE, 0, PRESENT, SWAPPED);
        let p = 4096;
        // Pages 0..10: none, file, none, file, anon, file, file, swapped, none, none.
        let entries = [none, file, none, file, anon, file, file, swapped, none, none];
        let (found, held, kept) = runs(&entries, 0x10000, p);
        assert_eq!(found, [0x10000 + p..0x10000 + 4 * p, 0x10000 + 5 * p..0x10000 + 7 * p]);
        assert_eq!((held, kept), (4 * 4096, 4096), "four of the file's pages dropped, the written one kept");
        let (found, held, _) = runs(&[none, none], 0, p);
        assert!(found.is_empty() && held == 0, "nothing resident, nothing to drop");
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: spans become the whole pages they cover, spans less than a page apart
    /// as one, so two files that share a page lose it, and a span with no whole page of its own loses none.
    #[test]
    fn g11_s5_spans_become_the_whole_pages_they_cover() {
        let p = 4096;
        // Three files back to back from 0x1010 (the second ends at 0x4010), one at 0x9000 three pages long, a
        // small one alone at 0x20010, and an empty one.
        let spans = [(0x1010, 0x100), (0x1110, 0x2f00), (0x9000, 3 * p), (0x4010 + 0x10, 0x20), (0x20010, 0x100), (0x30000, 0)];
        assert_eq!(whole_pages(&spans, p), [0x2000..0x4000, 0x9000..0xc000]);
        assert!(whole_pages(&[], p).is_empty());
        assert!(whole_pages(&[(usize::MAX - 10, 100)], p).is_empty(), "a span past the address space is cut, not wrapped");
    }

    /// A private mapping of the first `len` bytes of a file, unmapped when dropped.
    struct Mapped {
        ptr: *mut u8,
        len: usize,
    }

    impl Mapped {
        #[allow(unsafe_code)]
        fn new(file: &std::fs::File, len: usize, write: bool) -> io::Result<Self> {
            use std::os::fd::AsFd as _;
            let prot = if write { rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE } else { rustix::mm::ProtFlags::READ };
            // SAFETY: a fresh mapping the kernel places (no address asked for), of `len` bytes of a file this
            // test made and holds open; nothing refers to it but `Mapped`.
            let ptr = unsafe { rustix::mm::mmap(std::ptr::null_mut(), len, prot, rustix::mm::MapFlags::PRIVATE, file.as_fd(), 0) }?;
            Ok(Self { ptr: ptr.cast(), len })
        }

        fn at(&self) -> usize {
            self.ptr.addr()
        }

        /// Its bytes, each read by a volatile read so that the compiler keeps every one.
        #[allow(unsafe_code)]
        fn read(&self) -> Vec<u8> {
            (0..self.len)
                .map(|i| {
                    // SAFETY: `i` is within the mapping, which is readable and lives as long as `self`.
                    unsafe { std::ptr::read_volatile(self.ptr.add(i)) }
                })
                .collect()
        }

        #[allow(unsafe_code)]
        fn write(&self, i: usize, b: u8) {
            if i < self.len {
                // SAFETY: `i` is within the mapping, made writable by `new(.., true)` before any `protect`.
                unsafe { std::ptr::write_volatile(self.ptr.add(i), b) }
            }
        }

        /// Made read-only, as the loader makes a binary's relocations once it has written them.
        #[allow(unsafe_code)]
        fn protect(&self) -> io::Result<()> {
            // SAFETY: the whole of this mapping, which this test owns; nothing holds a reference into it.
            unsafe { rustix::mm::mprotect(self.ptr.cast(), self.len, rustix::mm::MprotectFlags::READ) }.map_err(io::Error::from)
        }

        fn mapping(&self) -> Mapping {
            Mapping { start: self.at(), end: self.at() + self.len, write: false, private: true, inode: 0, path: String::new() }
        }

        /// pagemap's present bit for each of its pages.
        fn resident(&self) -> io::Result<Vec<bool>> {
            let page = page_size();
            let pagemap = std::fs::File::open("/proc/self/pagemap")?;
            let mut buf = vec![0u8; self.len / page * ENTRY];
            pagemap.read_exact_at(&mut buf, u64::try_from(self.at() / page * ENTRY).map_err(io::Error::other)?)?;
            Ok(buf.as_chunks::<ENTRY>().0.iter().map(|c| u64::from_ne_bytes(*c) & PRESENT != 0).collect())
        }
    }

    impl Drop for Mapped {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: the mapping `new` made, unmapped once; nothing refers to it after `self`.
            let _ = unsafe { rustix::mm::munmap(self.ptr.cast(), self.len) };
        }
    }

    /// A file of `pages` pages, each its own bytes.
    fn file_of(pages: usize) -> io::Result<(tempfile::NamedTempFile, Vec<u8>)> {
        use std::io::Write as _;
        let page = page_size();
        let bytes: Vec<u8> = (0..pages * page).map(|i| u8::try_from((i / page * 31 + i % 251) % 256).unwrap_or(0)).collect();
        let mut f = tempfile::NamedTempFile::new()?;
        f.write_all(&bytes)?;
        f.flush()?;
        Ok((f, bytes))
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a read-only mapping's pages, read, leave resident memory when dropped
    /// and come back unchanged on the next read.
    #[test]
    fn g11_s5_dropped_pages_leave_resident_memory_and_come_back_unchanged() -> io::Result<()> {
        let pages = 16;
        let (f, bytes) = file_of(pages)?;
        let m = Mapped::new(f.as_file(), bytes.len(), false)?;
        assert_eq!(m.read(), bytes);
        assert!(m.resident()?.iter().all(|r| *r), "every page read is resident");
        let done = drop_within(std::slice::from_ref(&(m.at()..m.at() + m.len)), &[m.mapping()])?;
        assert_eq!(done.bytes, u64::try_from(bytes.len()).unwrap_or(0));
        assert!(m.resident()?.iter().all(|r| !r), "none resident after the drop");
        assert_eq!(m.read(), bytes, "the same bytes, faulted back from the file");
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a page written before its mapping was made read-only (a relocation,
    /// a breakpoint) is an anonymous copy, and a drop keeps it and what was written in it.
    #[test]
    fn g11_s5_a_written_page_is_kept_with_what_was_written() -> io::Result<()> {
        let pages = 8;
        let page = page_size();
        let (f, bytes) = file_of(pages)?;
        let m = Mapped::new(f.as_file(), bytes.len(), true)?;
        let _ = m.read();
        m.write(3 * page + 5, 0xAB);
        m.protect()?;
        let done = drop_within(std::slice::from_ref(&(m.at()..m.at() + m.len)), &[m.mapping()])?;
        assert_eq!(done.kept, u64::try_from(page).unwrap_or(0), "the written page is kept");
        assert_eq!(done.bytes, u64::try_from((pages - 1) * page).unwrap_or(0));
        let resident = m.resident()?;
        assert_eq!(resident.iter().filter(|r| **r).count(), 1);
        assert_eq!(resident.get(3), Some(&true));
        let mut want = bytes.clone();
        if let Some(b) = want.get_mut(3 * page + 5) {
            *b = 0xAB;
        }
        assert_eq!(m.read(), want, "what was written is still there, the rest the file's");
        Ok(())
    }

    /// Read-only data of this test binary's own, larger than four of the largest pages Linux takes (64 KiB).
    static OWN: [u8; 320 << 10] = [0x5A; 320 << 10];

    /// Goal 11, plan 01a1a87b2e56 step 5: this binary's own mappings are found (the code that runs and its
    /// read-only data among them), and its read-only data dropped through them comes back unchanged.
    #[test]
    fn g11_s5_the_binarys_own_pages_drop_and_come_back() -> io::Result<()> {
        let own = own_mappings()?;
        let code = g11_s5_the_binarys_own_pages_drop_and_come_back as fn() -> io::Result<()> as usize;
        assert!(own.iter().any(|m| (m.start..m.end).contains(&code)), "the code that runs is in one: {own:?}");
        let data = std::hint::black_box(&OWN);
        let at = data.as_ptr().addr();
        assert!(own.iter().any(|m| (m.start..m.end).contains(&at)), "its read-only data is in one: {own:?}");
        let sum = |d: &[u8]| d.iter().map(|b| u64::from(*b)).sum::<u64>();
        let want = u64::try_from(OWN.len()).unwrap_or(0) * 0x5A;
        assert_eq!(sum(data), want);
        let done = drop_own(std::slice::from_ref(&(at..at + OWN.len())))?;
        // Its whole pages, every one read just now: dropped, but for one a test beside it may have faulted
        // back in the meantime (the kernel maps the 64 KiB around a fault).
        let page = u64::try_from(page_size()).unwrap_or(u64::MAX);
        let first = at.checked_next_multiple_of(page_size()).unwrap_or(usize::MAX);
        let whole = u64::try_from(((at + OWN.len()) / page_size() * page_size()).saturating_sub(first)).unwrap_or(0);
        assert!(done.bytes > 0 && done.bytes <= whole && done.bytes % page == 0, "{done:?} of {whole}");
        assert_eq!(sum(std::hint::black_box(&OWN)), want, "and read back the same");
        Ok(())
    }
}
