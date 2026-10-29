//! The `Forge` conformance suite (docs/seams.md, "Forge"): each case a clause, checked against what
//! the repository's abilities say, so one suite runs on every host.

use futures::future::BoxFuture;

use super::*;
use crate::conformance::{Case, Failure, ensure, fine, refused, same};

/// What a `Forge` conformance suite runs against: a fresh host per case, with the repository
/// `repo()` (its trunk [`TRUNK`], one commit, no changes, its queue off), and the host's git side
/// and controls (pushes, its queue's checks, its clock): the simulator the implementation reaches.
pub trait ForgeFixture: Send + Sync {
    fn forge(&self) -> &dyn Forge;
    fn host(&self) -> Host;
    fn repo(&self) -> &str;
    fn git(&self) -> &SimForge;
}

fn push<F: ForgeFixture>(f: &F, branch: &str, from: &str, label: &str) -> Result<String, Failure> {
    f.git().push(f.repo(), branch, from, label).map_err(|e| format!("push to {branch}: {e}"))
}

fn tip<F: ForgeFixture>(f: &F, branch: &str) -> Result<String, Failure> {
    f.git().tip(f.repo(), branch).ok_or_else(|| format!("no branch {branch}"))
}

async fn open<F: ForgeFixture>(f: &F, head: &str, base: &str, by: &str) -> Result<Change, Failure> {
    let o = Open { head: head.to_owned(), base: base.to_owned(), title: format!("change from {head}"), by: by.to_owned() };
    fine(&format!("open {head} into {base}"), f.forge().open(f.repo(), o).await)
}

/// A branch off the trunk with one commit, and a change from it into the trunk.
async fn change<F: ForgeFixture>(f: &F, branch: &str, by: &str) -> Result<Change, Failure> {
    push(f, branch, TRUNK, branch)?;
    open(f, branch, TRUNK, by).await
}

/// `n` changes by `by`, each on the branch of the one before (the first on the trunk).
async fn chain<F: ForgeFixture>(f: &F, prefix: &str, n: u32, by: &str) -> Result<Vec<Change>, Failure> {
    let mut out = Vec::new();
    let mut below = TRUNK.to_owned();
    for i in 1..=n {
        let branch = format!("{prefix}-{i}");
        push(f, &branch, &below, &branch)?;
        out.push(open(f, &branch, &below, by).await?);
        below = branch;
    }
    Ok(out)
}

fn numbers(cs: &[Change]) -> Vec<u64> {
    cs.iter().map(|c| c.number).collect()
}

async fn read<F: ForgeFixture>(f: &F, n: u64) -> Result<Change, Failure> {
    match fine(&format!("read change {n}"), f.forge().change(f.repo(), n, None).await)? {
        Fresh::Changed { value, .. } => Ok(value),
        Fresh::NotModified => Err(format!("change {n} read without an ETag answered NotModified")),
    }
}

async fn abilities<F: ForgeFixture>(f: &F) -> Result<Abilities, Failure> {
    fine("abilities", f.forge().abilities(f.repo()).await)
}

/// A merge's end: at once, or polled from the background.
async fn ended<F: ForgeFixture>(f: &F, n: u64, m: Merging) -> Result<MergeResult, Failure> {
    let id = match m {
        Merging::Merged { merged, commit } => return Ok(MergeResult::Merged { merged, commit }),
        Merging::Started { id } => id,
    };
    for _ in 0..20 {
        match fine("merge_result", f.forge().merge_result(f.repo(), n, &id).await)? {
            MergeResult::Pending => {}
            done => return Ok(done),
        }
    }
    Err(format!("the merge {id} of change {n} was still pending after 20 polls"))
}

/// Merges change `n` at its head, by the host's first method, to its end.
async fn merge<F: ForgeFixture>(f: &F, n: u64) -> Result<MergeResult, Failure> {
    let method = abilities(f).await?.methods.first().copied().ok_or("the host has no merge methods")?;
    let expected = read(f, n).await?.head_sha;
    let m = fine(&format!("merge change {n}"), f.forge().merge(f.repo(), n, Merge { method, expected }).await)?;
    ended(f, n, m).await
}

fn merged_list(r: &MergeResult) -> Result<Vec<u64>, Failure> {
    match r {
        MergeResult::Merged { merged, .. } => Ok(merged.clone()),
        other => Err(format!("the merge didn't end merged: {other:?}")),
    }
}

fn unsupported<T: std::fmt::Debug>(what: &str, v: Result<T, Refusal>) -> Result<(), Failure> {
    match refused(what, v)? {
        Refusal::Unsupported { kiln_does, .. } => ensure(!kiln_does.trim().is_empty(), || format!("{what}: refused Unsupported without naming what kiln does")),
        other => Err(format!("{what}: refused {other:?}, not Unsupported")),
    }
}

fn is_moved<T: std::fmt::Debug>(what: &str, v: Result<T, Refusal>) -> Result<(), Failure> {
    match refused(what, v)? {
        Refusal::Moved { .. } => Ok(()),
        other => Err(format!("{what}: refused {other:?}, not Moved")),
    }
}

fn is_invalid<T: std::fmt::Debug>(what: &str, v: Result<T, Refusal>) -> Result<(), Failure> {
    match refused(what, v)? {
        Refusal::Invalid { .. } => Ok(()),
        other => Err(format!("{what}: refused {other:?}, not Invalid")),
    }
}

fn is_closed<T: std::fmt::Debug>(what: &str, v: Result<T, Refusal>) -> Result<(), Failure> {
    match refused(what, v)? {
        Refusal::Closed { .. } => Ok(()),
        other => Err(format!("{what}: refused {other:?}, not Closed")),
    }
}

fn is_stack_limit<T: std::fmt::Debug>(what: &str, v: Result<T, Refusal>) -> Result<String, Failure> {
    match refused(what, v)? {
        Refusal::StackLimit { why } => Ok(why),
        other => Err(format!("{what}: refused {other:?}, not StackLimit")),
    }
}

fn native(a: &Abilities) -> Option<(Option<u32>, bool, StackOrder)> {
    match &a.stacks {
        Stacks::Native { most, one_author, order } => Some((*most, *one_author, *order)),
        Stacks::Kiln { .. } => None,
    }
}

fn c1<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = abilities(f).await?;
        same("its host", a.host, f.host())?;
        let (stacks, update, review, merge_async) = match f.host() {
            Host::Github => (Some((None, false, StackOrder::BottomUp)), Some(HostUpdate::UpdateBranch), Batched::PendingReview, true),
            Host::Gitlab => (Some((Some(20), true, StackOrder::Retarget)), Some(HostUpdate::Rebase), Batched::DraftNotes, false),
            Host::Bitbucket => (None, None, Batched::Held, false),
        };
        same("its stacks", native(&a), stacks)?;
        same("its own way of updating", a.update, update)?;
        same("its batched reviews", a.review, review)?;
        same("whether it merges in the background", a.merge_async, merge_async)?;
        ensure(a.methods.contains(&Method::Merge), || format!("it can't merge by a merge commit: {:?}", a.methods))?;
        same("its queue, while off", a.queue, None)?;
        f.git().set_queue(f.repo(), true);
        let want = match f.host() {
            Host::Github => Some(QueueKind::MergeQueue),
            Host::Gitlab => Some(QueueKind::MergeTrain),
            Host::Bitbucket => None,
        };
        same("its queue, once on", abilities(f).await?.queue, want)
    })
}

fn c2<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let opened = change(f, "feature", "alice").await?;
        let got = read(f, opened.number).await?;
        same("the change read back", got.clone(), opened)?;
        same("its author", got.author.as_str(), "alice")?;
        same("its branches", (got.head_ref.as_str(), got.base_ref.as_str()), ("feature", TRUNK))?;
        same("its head", got.head_sha, tip(f, "feature")?)?;
        same("its state", got.state, State::Open)?;
        match refused("a change never opened", f.forge().change(f.repo(), 9999, None).await)? {
            Refusal::NotFound { .. } => Ok(()),
            other => Err(format!("a change never opened: refused {other:?}, not NotFound")),
        }
    })
}

fn c3<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = change(f, "feature", "alice").await?;
        let (_, tag) = fine("read", f.forge().change(f.repo(), c.number, None).await)?.changed().ok_or("a first read answered NotModified")?;
        let (_, list_tag) = fine("list", f.forge().changes(f.repo(), None).await)?.changed().ok_or("a first list answered NotModified")?;
        let before = fine("rate", f.forge().rate(f.repo()).await)?.remaining;
        same("a read with an unchanged change's ETag", fine("read again", f.forge().change(f.repo(), c.number, Some(&tag)).await)?, Fresh::NotModified)?;
        same("a list with the unchanged list's ETag", fine("list again", f.forge().changes(f.repo(), Some(&list_tag)).await)?, Fresh::NotModified)?;
        same("requests left after NotModified reads", fine("rate", f.forge().rate(f.repo()).await)?.remaining, before)?;
        let head = push(f, "feature", "feature", "more")?;
        let (now, new_tag) = fine("read after a push", f.forge().change(f.repo(), c.number, Some(&tag)).await)?.changed().ok_or("a change pushed to read NotModified")?;
        same("its head after the push", now.head_sha, head)?;
        ensure(new_tag != tag, || "its ETag didn't change".into())?;
        let (list, _) = fine("list after a push", f.forge().changes(f.repo(), Some(&list_tag)).await)?.changed().ok_or("the open changes read NotModified after a push")?;
        same("the open changes", numbers(&list), vec![c.number])
    })
}

fn c4<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = change(f, "feature", "alice").await?;
        let main = push(f, TRUNK, TRUNK, "trunk moves")?;
        let a = abilities(f).await?;
        let up = |expected: &str| Update { expected: expected.to_owned(), replayed: None };
        if a.update.is_none() {
            return unsupported("updating on a host with no way of its own", f.forge().update(f.repo(), c.number, up(&c.head_sha)).await);
        }
        is_moved("an update at another head", f.forge().update(f.repo(), c.number, up(&main)).await)?;
        same("its head after a refused update", read(f, c.number).await?.head_sha, c.head_sha.clone())?;
        if fine("update", f.forge().update(f.repo(), c.number, up(&c.head_sha)).await)? == Updated::Started {
            let mut done = false;
            for _ in 0..20 {
                if !read(f, c.number).await?.updating {
                    done = true;
                    break;
                }
            }
            ensure(done, || "the host's update was still under way after 20 reads".into())?;
        }
        ensure(f.git().contains(f.repo(), "feature", &main), || "the updated change doesn't contain its base's head".into())?;
        same("its head as read", read(f, c.number).await?.head_sha, tip(f, "feature")?)
    })
}

fn c5<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = change(f, "feature", "alice").await?;
        push(f, TRUNK, TRUNK, "trunk moves")?;
        let replayed = push(f, "kiln/replay/feature", TRUNK, "feature, replayed")?;
        let lease = |expected: &str| Update { expected: expected.to_owned(), replayed: Some(replayed.clone()) };
        is_moved("a push with a stale lease", f.forge().update(f.repo(), c.number, lease(&replayed)).await)?;
        same("its branch after a stale lease", tip(f, "feature")?, c.head_sha.clone())?;
        same("a push with the lease", fine("update", f.forge().update(f.repo(), c.number, lease(&c.head_sha)).await)?, Updated::Done { head: replayed.clone() })?;
        same("its head", read(f, c.number).await?.head_sha, replayed)
    })
}

fn c6<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        if native(&abilities(f).await?).is_none() {
            return Ok(());
        }
        let cs = chain(f, "s", 3, "alice").await?;
        let s = fine("stack", f.forge().stack(f.repo(), numbers(&cs)).await)?;
        same("the stack's changes", s.changes.clone(), numbers(&cs))?;
        let second = cs.get(1).map(|c| c.number).ok_or("no second change")?;
        same("the second's place", read(f, second).await?.stack, Some(Place { stack: s.number, position: 2, size: 3 }))?;
        same("the stack the third is in", fine("stack_of", f.forge().stack_of(f.repo(), cs.get(2).map(|c| c.number).unwrap_or(0)).await)?, Some(s.clone()))?;
        let loose = change(f, "loose", "alice").await?;
        is_invalid("adding a change that doesn't chain", f.forge().stack_add(f.repo(), s.number, vec![loose.number]).await)?;
        is_invalid("stacking a change in a stack already", f.forge().stack(f.repo(), vec![loose.number, second]).await)?;
        same("the stack after refusals", fine("stack_of", f.forge().stack_of(f.repo(), second).await)?.map(|s| s.changes), Some(numbers(&cs)))?;
        push(f, "s-4", "s-3", "s-4")?;
        let fourth = open(f, "s-4", "s-3", "alice").await?;
        same("the stack added to", fine("stack_add", f.forge().stack_add(f.repo(), s.number, vec![fourth.number]).await)?.changes.len(), 4)
    })
}

fn c7<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let Some((most, one_author, _)) = native(&abilities(f).await?) else { return Ok(()) };
        if one_author {
            let mine = change(f, "mine", "alice").await?;
            push(f, "theirs", "mine", "theirs")?;
            let theirs = open(f, "theirs", "mine", "bob").await?;
            let why = is_stack_limit("a stack with another author's change", f.forge().stack(f.repo(), vec![mine.number, theirs.number]).await)?;
            ensure(why.contains("bob"), || format!("the refusal doesn't say whose: {why}"))?;
            same("the first's stack after the refusal", read(f, mine.number).await?.stack, None)?;
        }
        if let Some(most) = most {
            let cs = chain(f, "long", most.saturating_add(1), "alice").await?;
            let (fit, past) = cs.split_at(usize::try_from(most).unwrap_or(usize::MAX).min(cs.len()));
            is_stack_limit("a stack past the host's most", f.forge().stack(f.repo(), numbers(&cs)).await)?;
            let s = fine("a stack of the host's most", f.forge().stack(f.repo(), numbers(fit)).await)?;
            same("its size", s.changes.len(), fit.len())?;
            is_stack_limit("one more on a full stack", f.forge().stack_add(f.repo(), s.number, numbers(past)).await)?;
        }
        Ok(())
    })
}

fn c8<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        if native(&abilities(f).await?).is_some() {
            return Ok(());
        }
        let cs = chain(f, "s", 2, "alice").await?;
        unsupported("a stack", f.forge().stack(f.repo(), numbers(&cs)).await)?;
        unsupported("adding to a stack", f.forge().stack_add(f.repo(), 1, numbers(&cs)).await)?;
        unsupported("unstacking", f.forge().unstack(f.repo(), 1).await)?;
        same("the first's stack", read(f, cs.first().map(|c| c.number).unwrap_or(0)).await?.stack, None)
    })
}

fn c9<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        if !matches!(native(&abilities(f).await?), Some((_, _, StackOrder::BottomUp))) {
            return Ok(());
        }
        let root = tip(f, TRUNK)?;
        let cs = chain(f, "s", 3, "alice").await?;
        let &[one, two, three] = numbers(&cs).as_slice() else { return Err("three changes".into()) };
        fine("stack", f.forge().stack(f.repo(), vec![one, two, three]).await)?;
        same("merging the bottom", merged_list(&merge(f, one).await?)?, vec![one])?;
        let top_head = read(f, three).await?.head_sha;
        let second_head = read(f, two).await?.head_sha;
        same("merging the second, after the bottom", merged_list(&merge(f, two).await?)?, vec![two])?;
        same("the trunk", tip(f, TRUNK)?, second_head)?;
        ensure(f.git().linear(f.repo(), &root, TRUNK), || "the trunk's history isn't linear".into())?;
        let third = read(f, three).await?;
        same("the third, above the merged", (third.state.clone(), third.base_ref.as_str(), third.stack.map(|p| (p.position, p.size))), (State::Open, TRUNK, Some((1, 1))))?;
        same("the third's head", third.head_sha, top_head)?;
        let more = chain(f, "t", 3, "alice").await?;
        let &[a, b, c] = numbers(&more).as_slice() else { return Err("three changes".into()) };
        fine("stack", f.forge().stack(f.repo(), vec![a, b, c]).await)?;
        same("merging a stack's top", merged_list(&merge(f, c).await?)?, vec![a, b, c])?;
        for n in [a, b, c] {
            ensure(matches!(read(f, n).await?.state, State::Merged { .. }), || format!("change {n} isn't merged"))?;
        }
        ensure(f.git().linear(f.repo(), &root, TRUNK), || "the trunk's history isn't linear".into())
    })
}

fn c10<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        if !matches!(native(&abilities(f).await?), Some((_, _, StackOrder::Retarget))) {
            return Ok(());
        }
        let cs = chain(f, "s", 3, "alice").await?;
        let &[one, two, three] = numbers(&cs).as_slice() else { return Err("three changes".into()) };
        fine("stack", f.forge().stack(f.repo(), vec![one, two, three]).await)?;
        let head = read(f, one).await?.head_sha;
        same("merging the bottom", merged_list(&merge(f, one).await?)?, vec![one])?;
        ensure(f.git().contains(f.repo(), TRUNK, &head), || "the trunk doesn't have the bottom's head".into())?;
        let next = read(f, two).await?;
        same("the next, retargeted", (next.base_ref.as_str(), next.stack.map(|p| (p.position, p.size))), (TRUNK, Some((1, 2))))?;
        same("the top's base", read(f, three).await?.base_ref, "s-2".to_owned())
    })
}

fn c11<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        if !matches!(native(&abilities(f).await?), Some((_, _, StackOrder::BottomUp))) {
            return Ok(());
        }
        let cs = chain(f, "s", 2, "alice").await?;
        let &[one, two] = numbers(&cs).as_slice() else { return Err("two changes".into()) };
        fine("stack", f.forge().stack(f.repo(), vec![one, two]).await)?;
        push(f, TRUNK, TRUNK, "trunk moves")?;
        let expected = read(f, two).await?.head_sha;
        let method = Method::Merge;
        match f.forge().merge(f.repo(), two, Merge { method, expected }).await {
            Err(Refusal::NotLinear { .. }) => {}
            Ok(m) => match ended(f, two, m).await? {
                MergeResult::Failed { .. } => {}
                other => return Err(format!("a stack that isn't linear merged: {other:?}")),
            },
            Err(other) => return Err(format!("a stack that isn't linear: refused {other:?}, not NotLinear")),
        }
        same("the bottom after", read(f, one).await?.state, State::Open)?;
        let r1 = push(f, "kiln/replay/s-1", TRUNK, "s-1 replayed")?;
        let old1 = read(f, one).await?.head_sha;
        fine("update the bottom", f.forge().update(f.repo(), one, Update { expected: old1, replayed: Some(r1) }).await)?;
        let r2 = push(f, "kiln/replay/s-2", "s-1", "s-2 replayed")?;
        let old2 = read(f, two).await?.head_sha;
        fine("update the top", f.forge().update(f.repo(), two, Update { expected: old2, replayed: Some(r2) }).await)?;
        same("merging once updated", merged_list(&merge(f, two).await?)?, vec![one, two])
    })
}

fn c12<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = change(f, "feature", "alice").await?;
        let trunk = tip(f, TRUNK)?;
        push(f, "feature", "feature", "pushed again")?;
        let stale = Merge { method: Method::Merge, expected: c.head_sha.clone() };
        match f.forge().merge(f.repo(), c.number, stale).await {
            Err(Refusal::Moved { .. }) => {}
            Ok(m) => match ended(f, c.number, m).await? {
                MergeResult::Failed { .. } => {}
                other => return Err(format!("a merge at a head that moved: {other:?}")),
            },
            Err(other) => return Err(format!("a merge at a head that moved: refused {other:?}, not Moved")),
        }
        same("the change after", read(f, c.number).await?.state, State::Open)?;
        same("the trunk after", tip(f, TRUNK)?, trunk)?;
        let done = merge(f, c.number).await?;
        let MergeResult::Merged { merged, commit } = done else { return Err(format!("a merge at its head: {done:?}")) };
        same("merged", merged, vec![c.number])?;
        same("the trunk", tip(f, TRUNK)?, commit.clone())?;
        same("its state", read(f, c.number).await?.state, State::Merged { commit })
    })
}

fn c13<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = change(f, "a", "alice").await?;
        let b = change(f, "b", "bob").await?;
        unsupported("queueing while the repository has no queue", f.forge().enqueue(f.repo(), a.number, &a.head_sha).await)?;
        f.git().set_queue(f.repo(), true);
        if abilities(f).await?.queue.is_none() {
            return unsupported("queueing on a host with no queue", f.forge().enqueue(f.repo(), a.number, &a.head_sha).await);
        }
        is_moved("queueing at another head", f.forge().enqueue(f.repo(), a.number, &b.head_sha).await)?;
        fine("enqueue a", f.forge().enqueue(f.repo(), a.number, &a.head_sha).await)?;
        same("the queue", fine("enqueue b", f.forge().enqueue(f.repo(), b.number, &b.head_sha).await)?, vec![a.number, b.number])?;
        same("queued", fine("queued", f.forge().queued(f.repo()).await)?, vec![a.number, b.number])?;
        same("landed", fine("land", f.git().land_queue(f.repo()))?, vec![a.number, b.number])?;
        for n in [a.number, b.number] {
            ensure(matches!(read(f, n).await?.state, State::Merged { .. }), || format!("change {n} isn't merged"))?;
        }
        same("queued after landing", fine("queued", f.forge().queued(f.repo()).await)?, Vec::<u64>::new())
    })
}

fn c14<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        f.git().set_queue(f.repo(), true);
        let a = abilities(f).await?;
        if a.queue.is_none() {
            return unsupported("taking a change out of a host with no queue", f.forge().dequeue(f.repo(), 1).await);
        }
        let other = change(f, "other", "bob").await?;
        fine("enqueue", f.forge().enqueue(f.repo(), other.number, &other.head_sha).await)?;
        if matches!(native(&a), Some((_, _, StackOrder::BottomUp))) {
            let cs = chain(f, "s", 3, "alice").await?;
            let &[one, two, three] = numbers(&cs).as_slice() else { return Err("three changes".into()) };
            fine("stack", f.forge().stack(f.repo(), vec![one, two, three]).await)?;
            let head = read(f, three).await?.head_sha;
            same("the queue with the stack", fine("enqueue the top", f.forge().enqueue(f.repo(), three, &head).await)?, vec![other.number, one, two, three])?;
            same("taken out with the second", fine("dequeue", f.forge().dequeue(f.repo(), two).await)?, vec![two, three])?;
            return same("left queued", fine("queued", f.forge().queued(f.repo()).await)?, vec![other.number, one]);
        }
        let mine = change(f, "mine", "alice").await?;
        fine("enqueue", f.forge().enqueue(f.repo(), mine.number, &mine.head_sha).await)?;
        same("taken out", fine("dequeue", f.forge().dequeue(f.repo(), mine.number).await)?, vec![mine.number])?;
        same("left queued", fine("queued", f.forge().queued(f.repo()).await)?, vec![other.number])
    })
}

fn c15<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = change(f, "feature", "carol").await?;
        let n = c.number;
        let d = |body: &str, line: Option<u32>| Draft { body: body.to_owned(), line: line.map(|line| Line { path: "src/lib.rs".into(), line }) };
        fine("draft", f.forge().draft(f.repo(), n, "alice", d("first", Some(3))).await)?;
        fine("draft", f.forge().draft(f.repo(), n, "alice", d("second", None)).await)?;
        fine("bob's draft", f.forge().draft(f.repo(), n, "bob", d("bob's", None)).await)?;
        same("alice's drafts", fine("drafts", f.forge().drafts(f.repo(), n, "alice").await)?.len(), 2)?;
        same("drafts carol sees", fine("drafts", f.forge().drafts(f.repo(), n, "carol").await)?, Vec::new())?;
        same("published before", fine("comments", f.forge().comments(f.repo(), n).await)?, Vec::new())?;
        let review = fine("publish", f.forge().publish(f.repo(), n, "alice", Verdict::RequestChanges).await)?;
        same("the review's verdict and author", (review.verdict, review.author.as_str()), (Verdict::RequestChanges, "alice"))?;
        same("its comments", review.comments.iter().map(|c| c.body.as_str()).collect::<Vec<_>>(), vec!["first", "second"])?;
        ensure(review.comments.iter().all(|c| c.review == Some(review.id)), || "a comment isn't marked with its review".into())?;
        same("the first's line", review.comments.first().and_then(|c| c.line.clone()).map(|l| l.line), Some(3))?;
        let said = |cs: &[Comment]| cs.iter().map(|c| (c.author.clone(), c.body.clone(), c.line.clone())).collect::<Vec<_>>();
        same("published", said(&fine("comments", f.forge().comments(f.repo(), n).await)?), said(&review.comments))?;
        same("alice's drafts after", fine("drafts", f.forge().drafts(f.repo(), n, "alice").await)?, Vec::new())?;
        same("bob's drafts after", fine("drafts", f.forge().drafts(f.repo(), n, "bob").await)?.len(), 1)
    })
}

fn c16<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        push(f, "x", TRUNK, "x")?;
        let r = fine("rate", f.forge().rate(f.repo()).await)?;
        ensure(r.remaining > 0 && r.remaining <= r.limit, || format!("the rate as it stands: {r:?}"))?;
        let (mut answered, mut resets_at) = (0u32, None);
        for _ in 0..=r.remaining {
            match f.forge().abilities(f.repo()).await {
                Ok(_) => answered = answered.saturating_add(1),
                Err(Refusal::RateLimited { resets_at: at }) => {
                    resets_at = Some(at);
                    break;
                }
                Err(other) => return Err(format!("a request within the limit: {other}")),
            }
        }
        let resets_at = resets_at.ok_or_else(|| format!("{} requests with {} left were never refused", r.remaining.saturating_add(1), r.remaining))?;
        ensure((1..=r.remaining).contains(&answered), || format!("{answered} requests answered with {} left", r.remaining))?;
        ensure(resets_at > f.git().now(), || format!("it resets at {resets_at}, not after now ({})", f.git().now()))?;
        match f.forge().open(f.repo(), Open { head: "x".into(), base: TRUNK.into(), title: "x".into(), by: "alice".into() }).await {
            Err(Refusal::RateLimited { .. }) => {}
            other => return Err(format!("a write past the limit: {other:?}")),
        }
        f.git().advance(resets_at.saturating_sub(f.git().now()));
        let (list, _) = fine("list once it reset", f.forge().changes(f.repo(), None).await)?.changed().ok_or("a first list answered NotModified")?;
        same("open changes after the refused write", list, Vec::new())
    })
}

fn c17<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let c = change(f, "feature", "alice").await?;
        merge_list_ok(f, c.number).await?;
        let head = read(f, c.number).await?.head_sha;
        is_closed("updating a merged change", f.forge().update(f.repo(), c.number, Update { expected: head.clone(), replayed: None }).await)?;
        is_closed("merging a merged change", f.forge().merge(f.repo(), c.number, Merge { method: Method::Merge, expected: head.clone() }).await)?;
        is_closed("a draft on a merged change", f.forge().draft(f.repo(), c.number, "alice", Draft { body: "late".into(), line: None }).await)?;
        f.git().set_queue(f.repo(), true);
        let a = abilities(f).await?;
        if a.queue.is_some() {
            is_closed("queueing a merged change", f.forge().enqueue(f.repo(), c.number, &head).await)?;
        }
        if native(&a).is_some() {
            is_closed("stacking a merged change", f.forge().stack(f.repo(), vec![c.number]).await)?;
        }
        Ok(())
    })
}

async fn merge_list_ok<F: ForgeFixture>(f: &F, n: u64) -> Result<(), Failure> {
    same("merged", merged_list(&merge(f, n).await?)?, vec![n])
}

fn c18<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let o = |head: &str, base: &str, title: &str| Open { head: head.to_owned(), base: base.to_owned(), title: title.to_owned(), by: "alice".into() };
        is_invalid("a change from a branch that isn't there", f.forge().open(f.repo(), o("nowhere", TRUNK, "t")).await)?;
        push(f, "feature", TRUNK, "feature")?;
        is_invalid("a change into a branch that isn't there", f.forge().open(f.repo(), o("feature", "nowhere", "t")).await)?;
        is_invalid("a change into itself", f.forge().open(f.repo(), o("feature", "feature", "t")).await)?;
        is_invalid("a blank title", f.forge().open(f.repo(), o("feature", TRUNK, " ")).await)?;
        let (list, _) = fine("list", f.forge().changes(f.repo(), None).await)?.changed().ok_or("a first list answered NotModified")?;
        same("open changes after the refusals", list, Vec::new())?;
        let c = open(f, "feature", TRUNK, "alice").await?;
        is_invalid("a second change from the same branch", f.forge().open(f.repo(), o("feature", TRUNK, "again")).await)?;
        is_invalid("a blank comment", f.forge().draft(f.repo(), c.number, "alice", Draft { body: "  ".into(), line: None }).await)?;
        is_invalid("a long comment", f.forge().draft(f.repo(), c.number, "alice", Draft { body: "x".repeat(BODY_MAX.saturating_add(1)), line: None }).await)?;
        same("drafts after the refusals", fine("drafts", f.forge().drafts(f.repo(), c.number, "alice").await)?, Vec::new())?;
        if native(&abilities(f).await?).is_some() {
            is_invalid("an empty stack", f.forge().stack(f.repo(), Vec::new()).await)?;
        }
        Ok(())
    })
}

fn c19<F: ForgeFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let a = abilities(f).await?;
        if !matches!(native(&a), Some((_, _, StackOrder::BottomUp))) {
            let cs = chain(f, "s", 2, "alice").await?;
            if native(&a).is_some() {
                fine("stack", f.forge().stack(f.repo(), numbers(&cs)).await)?;
            }
            return unsupported("unstacking on a host that can't", f.forge().unstack(f.repo(), 1).await);
        }
        let cs = chain(f, "s", 3, "alice").await?;
        let &[one, two, three] = numbers(&cs).as_slice() else { return Err("three changes".into()) };
        let s = fine("stack", f.forge().stack(f.repo(), vec![one, two, three]).await)?;
        same("unstacked, nothing queued", fine("unstack", f.forge().unstack(f.repo(), s.number).await)?, None)?;
        same("the stack after", fine("stack_of", f.forge().stack_of(f.repo(), two).await)?, None)?;
        let more = chain(f, "t", 2, "alice").await?;
        let &[x, y] = numbers(&more).as_slice() else { return Err("two changes".into()) };
        let s = fine("stack", f.forge().stack(f.repo(), vec![x, y]).await)?;
        f.git().set_queue(f.repo(), true);
        let head = read(f, x).await?.head_sha;
        fine("enqueue the bottom", f.forge().enqueue(f.repo(), x, &head).await)?;
        let left = fine("unstack", f.forge().unstack(f.repo(), s.number).await)?;
        same("left in the stack, queued", left.map(|s| s.changes), Some(vec![x]))?;
        same("the unqueued one's stack", read(f, y).await?.stack, None)
    })
}

/// The `Forge` contract, clause by clause (docs/seams.md, "Forge").
pub fn cases<F: ForgeFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "FORGE-1", says: "abilities say what the repository has of stacks, a queue, updating, batched reviews and merging, as its host has them, its queue only while it's on", check: c1::<F> },
        Case { id: "FORGE-2", says: "a change opened is read back with its author, branches and head; one never opened is refused NotFound", check: c2::<F> },
        Case { id: "FORGE-3", says: "a read with the ETag of a change, or of the open changes, that hasn't changed is NotModified and costs no request; once it changes the read gives it, with a new ETag", check: c3::<F> },
        Case { id: "FORGE-4", says: "a stale change updated the host's own way at its expected head comes to have its base's head, at once or once the host is done; at another head it's refused Moved and nothing moves; a host with no way of its own refuses Unsupported", check: c4::<F> },
        Case { id: "FORGE-5", says: "a replayed commit is pushed to a change's branch only with a lease on its expected head: at another head it's refused Moved and the branch stays", check: c5::<F> },
        Case { id: "FORGE-6", says: "a host with stacks makes one of changes bottom to top, each based on the head of the one below, each saying its place; a change that doesn't chain, or is in a stack already, is refused Invalid", check: c6::<F> },
        Case { id: "FORGE-7", says: "a host that bounds its stacks takes a stack of its most, and refuses StackLimit a change past it or by another author than the stack's", check: c7::<F> },
        Case { id: "FORGE-8", says: "a host without stacks refuses them Unsupported, naming what kiln does instead", check: c8::<F> },
        Case { id: "FORGE-9", says: "a host that merges stacks bottom-up merges a change with every unmerged change below it, bottom first, onto the trunk in a linear history, and leaves those above it open on the trunk", check: c9::<F> },
        Case { id: "FORGE-10", says: "a host that retargets merges a stack's bottom change into the trunk and retargets the next onto the trunk", check: c10::<F> },
        Case { id: "FORGE-11", says: "a bottom-up stack whose history isn't linear isn't merged (refused NotLinear, or failing once polled) until it's updated", check: c11::<F> },
        Case { id: "FORGE-12", says: "a merge of a change whose head isn't the one expected is refused Moved, or fails once polled, and nothing merges; at its head it ends merged, at once or once polled", check: c12::<F> },
        Case { id: "FORGE-13", says: "a repository's queue on its host lands changes in the order they were queued, each at its expected head; where it has none, queueing is refused Unsupported, naming kiln's queue", check: c13::<F> },
        Case { id: "FORGE-14", says: "taking a change out of its host's queue takes out the changes stacked above it too, and leaves the rest queued", check: c14::<F> },
        Case { id: "FORGE-15", says: "a batched review's comments are seen only by their author until published, then all appear together, with its verdict", check: c15::<F> },
        Case { id: "FORGE-16", says: "every request but a read answered NotModified counts against the host's rate limit; past it requests are refused RateLimited, saying when it resets, nothing is done, and they go again once it has", check: c16::<F> },
        Case { id: "FORGE-17", says: "a merged change refuses updates, merges, queueing, stacking and drafts: Closed", check: c17::<F> },
        Case { id: "FORGE-18", says: "bad requests are refused Invalid and change nothing: a branch that isn't there, a change into itself or from a branch with one open, a blank title, a blank or long comment, an empty stack", check: c18::<F> },
        Case { id: "FORGE-19", says: "unstacking takes a stack's unmerged changes out of it, leaving those queued in place, and dissolves it once none are left; a host that can't refuses Unsupported", check: c19::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    fn suite(host: Host) -> Result<(), String> {
        futures::executor::block_on(run("Forge", &cases::<SimForgeFixture>(), || async move { SimForgeFixture::new(host) })).into_result().map(|_| ())
    }

    /// W26-AC09: the forge seam's suite passes against GitHub's simulator.
    #[test]
    fn w26_ac09_the_github_simulator_passes_the_forge_suite() -> Result<(), String> {
        suite(Host::Github)
    }

    /// W26-AC09: the forge seam's suite passes against GitLab's simulator.
    #[test]
    fn w26_ac09_the_gitlab_simulator_passes_the_forge_suite() -> Result<(), String> {
        suite(Host::Gitlab)
    }

    /// W26-AC09: the forge seam's suite passes against Bitbucket's simulator.
    #[test]
    fn w26_ac09_the_bitbucket_simulator_passes_the_forge_suite() -> Result<(), String> {
        suite(Host::Bitbucket)
    }
}
