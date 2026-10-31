# The B-link tree's root split: holding the split page until the level above has its link

Source notes for decision 3 of `docs/design/engine.md` (the B-tree's latches), on one question: when
a writer splits a page, how long it holds that page, and what happens when the page was the root.
CLAUDE.md §5: the decision cites these notes; each says what its source holds and what it implies
here.

## What happened in Amphora (2026-10-24)

`many_writers_and_readers_at_once_lose_nothing` (32 writers putting interleaved keys, 4 readers,
`crates/engine/storage/versioned/blink.rs`) failed while the whole engine lib suite ran on fedora,
and passed alone. Each writer released the latch of the page it split, then posted the split to the
level above, and a split of the root made its new root only in that post. In the window between,
another writer could move right from the old root into the root's new page, fill it and split it:
its path ended at or below the old root, the root was still the old page, so `grow` declined (the
root wasn't its page) and `parent_of` found no page a level above the split page: Corrupt
"versioned storage: no level above a split page". The window lasts one descheduling of the root's
splitter, so it opens under load. A test that pauses the root's splitter right after it releases the
root's latch (`a_root_split_is_whole_before_another_writer_reaches_its_new_page`) failed that way
on the code before the fix, at key 277 for a root of level 0, every run.

## Lehman and Yao (ACM TODS 6(4), 1981)

"Efficient Locking for Concurrent Operations on B-Trees", pp. 650–670, doi:10.1145/319628.319663.
The insertion procedure (p. 662) splits a full node, writes both halves, then pops the parent from
its stack, locks it and moves right in it, and only then releases the node it split
(`unlock(oldnode);`): "at most three nodes are ever locked simultaneously", "the original half of
the split node, and two nodes in the level above the split node, while the insertion is moving to
the right" (p. 662). The split page is never unlocked before the level above is locked, so no
other writer splits its new half before the link to it can go in. The paper doesn't treat a split
of the root (PostgreSQL's README below says so).

**Implies here.** Releasing the split page before the parent is had is a departure from the paper;
it is safe for any page whose level above exists (moving right on that level finds the place), and
not for the root, whose level above doesn't exist until the new root is made.

## PostgreSQL's nbtree (src/backend/access/nbtree/README, master as of 2026-10-24)

- Root splits, "Differences to the Lehman & Yao algorithm": "Lehman and Yao fail to discuss what
  must happen when the root page becomes full and must be split. Our implementation is to split the
  root in the same way that any other page would be split, then construct a new root page holding
  pointers to both of the resulting pages". In the WAL section: "For a root split, the follow-on
  WAL entry is a "new root" entry rather than an "insertion" entry".
- How long a split page stays locked, "WAL Considerations": "The child page is kept locked until the
  insertion in the parent is finished and the flag in the child cleared, but can be released
  immediately after that". Why, "Scans during Recovery": "Holding on to an extra write lock on the
  primary is only necessary so that a second writer cannot observe the incomplete split flag before
  the first writer finishes the split."
- The failure itself, "WAL Considerations": "if the page without a downlink gets split again, the
  insertion algorithm will fail to find the location in the parent level to insert the downlink."
  PostgreSQL's answer for crashes is to "create any missing downlinks on-the-fly, when searching
  the tree for a new insertion" (the incomplete-split flag).
- A stale descent stack, "Differences to the Lehman & Yao algorithm": "we find the correct place by
  re-descending the tree until we reach the level one above the level we need to insert a link to,
  and then moving right as necessary" (Amphora's `parent_of`).
- Coupling, same section: "We more or less stick with Lehman and Yao's approach of conservatively
  coupling parent and child locks when ascending the tree, since it's far simpler."

**Implies here.** The new root must exist before any writer can reach the root's new page. Amphora's
fix makes the new root while the split root's latch is still held, at the leaf level (`modify`) and
at inner levels (`post`); splits of other pages keep releasing their latch first, since the level
above them exists. `grow` then runs only under the latch of the page that split, so the root it
compares with can't change meanwhile (only `grow`, under the root's latch, replaces it), and the
mutex that serialized root replacement went. Not taken: a wait or retry when no level is above
(CLAUDE.md §4 rules out retries, and the wait has no bound); PostgreSQL's incomplete-split flag,
finished by whoever meets it (state on every page, for a window the latch closes); coupling every
split to its parent (a longer hold on every split for a case only the root has).
