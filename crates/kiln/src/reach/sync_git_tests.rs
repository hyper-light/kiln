//! U08 behavioral oracle: a selected repository remains Git, without unrelated physical data.
use super::*;

fn git(root: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let o = std::process::Command::new("git").arg("-C").arg(root).args(args)
        .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_AUTHOR_NAME", "U08").env("GIT_AUTHOR_EMAIL", "u08@example.test")
        .env("GIT_COMMITTER_NAME", "U08").env("GIT_COMMITTER_EMAIL", "u08@example.test").output()?;
    anyhow::ensure!(o.status.success(), "Git oracle {args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
    Ok(o.stdout)
}

fn transfer(tree: &Tree, dst: &Path) -> anyhow::Result<Vec<String>> {
    let mut mirror = Mirror::new(dst.to_owned())?;
    mirror.plan(tree.entries.clone())?;
    let needed = need(dst, &tree.entries);
    for name in &needed {
        let mut offset = 0;
        loop {
            let (data, last) = read_piece(&tree.source(name), offset)?;
            mirror.write(name, offset, &data, last)?;
            offset += data.len() as u64;
            if last { break; }
        }
    }
    mirror.commit()?;
    Ok(needed)
}

fn fixture() -> anyhow::Result<tempfile::TempDir> {
    let cache = std::env::var_os("HOME").context("test HOME")?;
    let path = PathBuf::from(cache).join(".cache/kiln-unify");
    std::fs::create_dir_all(&path)?;
    Ok(tempfile::tempdir_in(path)?)
}

#[test]
fn u08_selected_history_dirty_index_refs_and_garbage_oracle() -> anyhow::Result<()> {
    let temp = fixture()?;
    let main = temp.path().join("main");
    let wt = temp.path().join("feature");
    let dst = temp.path().join("copy");
    std::fs::create_dir(&main)?;
    git(&main, &["init", "-q", "-b", "main"])?;
    std::fs::write(main.join("a"), "base\n")?;
    git(&main, &["add", "."])?;
    git(&main, &["commit", "-qm", "base"])?;
    git(&main, &["tag", "-a", "v1", "-m", "version one"])?;
    git(&main, &["checkout", "-qb", "unrelated"])?;
    let noise: Vec<u8> = (0u32..524_288).flat_map(|i| i.wrapping_mul(2_654_435_761).to_le_bytes()).collect();
    std::fs::write(main.join("huge"), noise)?;
    git(&main, &["add", "."])?;
    git(&main, &["commit", "-qm", "unrelated large history"])?;
    let unrelated = git(&main, &["rev-parse", "HEAD"])?;
    git(&main, &["checkout", "-q", "main"])?;
    git(&main, &["worktree", "add", "-qb", "feature", wt.to_str().context("worktree path")?])?;
    std::fs::write(wt.join("a"), "feature\n")?;
    git(&wt, &["commit", "-qam", "feature"])?;
    std::fs::write(main.join("upstream"), "divergent upstream\n")?;
    git(&main, &["add", "."])?;
    git(&main, &["commit", "-qm", "upstream advancement"])?;
    // Divergent upstream must retain its full history and ahead/behind semantics.
    git(&wt, &["update-ref", "refs/remotes/origin/main", "main"])?;
    git(&wt, &["config", "remote.origin.url", "https://private:secret@example.test/repo"])?;
    git(&wt, &["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"])?;
    git(&wt, &["branch", "--set-upstream-to", "origin/main"])?;
    std::fs::write(wt.join("a"), "staged\n")?;
    std::fs::write(wt.join("new"), "staged addition\n")?;
    git(&wt, &["add", "."])?;
    git(&wt, &["update-index", "--split-index"])?;
    std::fs::write(wt.join("a"), "unstaged\n")?;
    std::fs::write(wt.join("untracked"), "working tree\n")?;
    let own = git_path(&wt, "--absolute-git-dir").context("own metadata")?;
    let before = std::fs::read(own.join("index"))?;
    let pack = main.join(".git/objects/pack");
    std::fs::create_dir_all(&pack)?;
    let garbage = std::fs::File::create(pack.join("tmp_pack_garbage"))?;
    garbage.set_len(512 * 1024 * 1024)?;
    // An unreachable loose object also must stay home.
    std::fs::write(temp.path().join("unreachable"), "unreachable object\n")?;
    let unreachable = git(&main, &["hash-object", "-w", temp.path().join("unreachable").to_str().context("object path")?])?;
    let shared_before: Vec<_> = std::fs::read_dir(&own)?.filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("sharedindex."))
        .map(|e| Ok((e.path(), e.metadata()?.modified()?))).collect::<anyhow::Result<_>>()?;
    let tree = scan(&wt)?;
    for (path, modified) in shared_before { assert_eq!(std::fs::metadata(path)?.modified()?, modified, "shared source index untouched"); }
    assert!(tree.entries.iter().map(|e| e.size).sum::<u64>() < 200_000, "only selected history travels");
    assert!(!tree.entries.iter().any(|e| e.path.contains("tmp_pack") || e.path.contains("worktrees") || e.path.contains("unrelated")));
    transfer(&tree, &dst)?;
    let view = std::fs::read_to_string(dst.join(".git/kiln-reach-view"))?;
    assert!(view.contains("Other branches, reflogs and unrelated history are intentionally unavailable"));
    let missing = std::process::Command::new("git").arg("-C").arg(&dst)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/unrelated"]).output()?;
    assert_eq!(missing.status.code(), Some(1), "unrelated branch is absent, not a phantom ref to a missing object");
    assert_eq!(std::fs::read(own.join("index"))?, before, "source split index untouched");
    for args in [
        vec!["status", "--porcelain=v2", "--branch"], vec!["diff", "--no-ext-diff"],
        vec!["diff", "--cached", "--no-ext-diff"], vec!["diff", "HEAD", "--no-ext-diff"],
        vec!["log", "--format=%H %s"], vec!["log", "--format=%H", "@{upstream}"],
        vec!["rev-parse", "HEAD", "main", "v1", "@{upstream}"], vec!["merge-base", "HEAD", "main"],
        vec!["describe", "--tags"], vec!["ls-files", "--stage"],
    ] {
        assert_eq!(git(&wt, &args)?, git(&dst, &args)?, "oracle {args:?}");
    }
    assert!(!String::from_utf8(git(&dst, &["config", "--list", "--local"])?)?.contains("secret"));
    for oid in [unrelated, unreachable] {
        let status = std::process::Command::new("git").arg("-C").arg(&dst).args(["cat-file", "-e", std::str::from_utf8(&oid)?.trim()]).output()?.status;
        assert!(!status.success(), "unrelated object unavailable");
    }
    let second = scan(&wt)?;
    let first_bytes = std::fs::read(tree.source(".git/index"))?;
    let second_bytes = std::fs::read(second.source(".git/index"))?;
    assert_eq!(first_bytes, second_bytes, "exported index bytes stable");
    let again = transfer(&second, &dst)?;
    assert!(again.iter().all(|p| p == ".git/index"), "unchanged packs and metadata are reused: {again:?}");
    assert!(transfer(&scan(&wt)?, &dst)?.is_empty(), "no Git command refreshed the destination index this time");
    // New commit of the same length must update refs/config metadata as well as the pack.
    git(&wt, &["commit", "-qm", "staged work"])?;
    transfer(&scan(&wt)?, &dst)?;
    assert_eq!(git(&wt, &["log", "--format=%H"] )?, git(&dst, &["log", "--format=%H"])?);
    Ok(())
}

#[test]
fn u08_unborn_and_detached_repositories_keep_git_semantics() -> anyhow::Result<()> {
    let temp = fixture()?;
    let src = temp.path().join("src");
    let dst = temp.path().join("dst");
    std::fs::create_dir(&src)?;
    git(&src, &["init", "-q", "-b", "main"])?;
    std::fs::write(src.join("a"), "staged before first commit\n")?;
    git(&src, &["add", "."])?;
    transfer(&scan(&src)?, &dst)?;
    assert_eq!(git(&src, &["status", "--porcelain=v2", "--branch"])?, git(&dst, &["status", "--porcelain=v2", "--branch"])?);
    assert_eq!(git(&src, &["diff", "--cached"])?, git(&dst, &["diff", "--cached"])?);
    git(&src, &["commit", "-qm", "one"])?;
    git(&src, &["checkout", "-q", "--detach"])?;
    transfer(&scan(&src)?, &dst)?;
    assert_eq!(git(&src, &["status", "--porcelain=v2", "--branch"])?, git(&dst, &["status", "--porcelain=v2", "--branch"])?);
    assert_eq!(git(&src, &["show-ref"])?, git(&dst, &["show-ref"])?);
    Ok(())
}

#[test]
fn u08_conflict_stages_gitlinks_and_shallow_boundaries() -> anyhow::Result<()> {
    let temp = fixture()?;
    let src = temp.path().join("src");
    let dst = temp.path().join("dst");
    std::fs::create_dir(&src)?;
    git(&src, &["init", "-q", "-b", "main"])?;
    // Keep merge, the isolated oracle and the export on the same explicit policy, even
    // when the host's Git installation supplies a different system-wide default.
    git(&src, &["config", "core.autocrlf", "true"])?;
    git(&src, &["config", "core.eol", "crlf"])?;
    std::fs::write(src.join("a"), "base\n")?;
    git(&src, &["add", "."])?;
    git(&src, &["commit", "-qm", "base"])?;
    git(&src, &["checkout", "-qb", "side"])?;
    std::fs::write(src.join("a"), "side\n")?;
    git(&src, &["commit", "-qam", "side"])?;
    git(&src, &["checkout", "-q", "main"])?;
    std::fs::write(src.join("a"), "main\n")?;
    git(&src, &["commit", "-qam", "main"])?;
    let merge = std::process::Command::new("git").arg("-C").arg(&src).args(["-c", "user.name=U08", "-c", "user.email=u08@example.test", "merge", "side"]).output()?;
    assert!(!merge.status.success());
    transfer(&scan(&src)?, &dst)?;
    for args in [vec!["status", "--porcelain=v2"], vec!["status", "--short", "--branch"], vec!["diff", "--cc"], vec!["ls-files", "--stage"]] {
        assert_eq!(git(&src, &args)?, git(&dst, &args)?, "conflict oracle {args:?}");
    }
    // Per-repository attributes can override CRLF conversion without changing the
    // conflicted bytes or staged objects; preserve that view as well.
    std::fs::write(src.join(".git/info/attributes"), "a -text\n")?;
    transfer(&scan(&src)?, &dst)?;
    assert_eq!(git(&src, &["diff", "--cc"])?, git(&dst, &["diff", "--cc"])?, "conflict attributes oracle");
    git(&src, &["merge", "--abort"])?;
    // A gitlink's commit belongs to its submodule, and need not exist in this object DB.
    git(&src, &["update-index", "--add", "--cacheinfo", "160000,1111111111111111111111111111111111111111,module"])?;
    transfer(&scan(&src)?, &dst)?;
    assert_eq!(git(&src, &["diff", "--cached"])?, git(&dst, &["diff", "--cached"])?);
    let shallow = temp.path().join("shallow");
    git(&src, &["clone", "--no-local", "--depth=2", src.to_str().context("source path")?, shallow.to_str().context("shallow path")?])?;
    transfer(&scan(&shallow)?, &dst)?;
    assert_eq!(git(&shallow, &["log", "--format=%H"])?, git(&dst, &["log", "--format=%H"])?);
    assert_eq!(git(&shallow, &["rev-parse", "--is-shallow-repository"])?, git(&dst, &["rev-parse", "--is-shallow-repository"])?);
    Ok(())
}

#[test]
fn u08_invalid_git_metadata_never_falls_back_to_copying_objects() -> anyhow::Result<()> {
    let temp = fixture()?;
    std::fs::create_dir_all(temp.path().join(".git/objects/pack"))?;
    std::fs::write(temp.path().join(".git/objects/pack/tmp_pack_garbage"), "not a repository")?;
    assert!(scan(temp.path()).is_err());
    Ok(())
}
