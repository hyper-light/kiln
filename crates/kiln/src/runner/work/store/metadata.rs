//! U05/U07: scoped holdings and restartable journal imports in the host's Amphora database.
use std::io::{BufRead as _, Read as _, Seek as _};
use std::path::Path;
use anyhow::Context as _;
use sha2::{Digest as _, Sha256};
use crate::sql::{OptionalExtension as _, params};
use super::{Index, Line, Scope, is_digest};

pub(super) struct Metadata { store: crate::store::Store, owner: String }
impl Metadata {
    pub(super) fn open(work: &Path, root: &Path, store: crate::store::Store, host_key:&str) -> anyhow::Result<(Self,Index)> {
        store.import_runner_work(&root.join("ledger.db"),host_key)?;
        let owner=serde_json::to_string(&(host_key,work.canonicalize()?.to_string_lossy()))?;
        let metadata=Self { store,owner };
        metadata.import_journal(&root.join("index.log"))?;
        let mut index=Index::default();
        let mut after: Option<(String,String,String)>=None;
        loop {
            let (owner,cursor)=(metadata.owner.clone(),after);
            let rows: Vec<(String,String,String)>=metadata.store.now(move |c| {
                let (sql,args)=match cursor {
                    Some((repo,domain,digest))=>("SELECT repo,domain,digest FROM runner_work_holds WHERE owner=?1 AND (repo,domain,digest)>(?2,?3,?4) ORDER BY repo,domain,digest LIMIT 1000",params![owner,repo,domain,digest]),
                    None=>("SELECT repo,domain,digest FROM runner_work_holds WHERE owner=?1 ORDER BY repo,domain,digest LIMIT 1000",params![owner]),
                };
                let mut q=c.prepare(sql)?;
                Ok(q.query_map(args,|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<crate::sql::Result<_>>()?)
            })?;
            if rows.is_empty() { break; }
            after=rows.last().cloned();
            for (repo,domain,digest) in rows { index.apply(Line::Hold { scope:Scope {repo,domain},digest }); }
            anyhow::ensure!(index.lines()<=1_000_000,"runner scoped holdings exceed one million entries");
        }
        Ok((metadata,index))
    }

    pub(super) fn ledger(&self)->&crate::store::Store { &self.store }

    pub(super) fn hold(&self,scope:&Scope,digest:&str)->anyhow::Result<bool> {
        let (owner,scope,digest)=(self.owner.clone(),scope.clone(),digest.to_owned());
        self.store.now(move|c| Self::put(c,owner,scope,digest))
    }

    pub(super) async fn hold_async(&self,scope:&Scope,digest:&str)->anyhow::Result<bool> {
        let (owner,scope,digest)=(self.owner.clone(),scope.clone(),digest.to_owned());
        self.store.call(move|c| Self::put(c,owner,scope,digest)).await
    }

    pub(super) async fn hold_async_guarded(&self, scope: &Scope, digest: &str, guard: crate::runner::cache::CacheGuard) -> anyhow::Result<bool> {
        let (owner, scope, digest) = (self.owner.clone(), scope.clone(), digest.to_owned());
        // U07: actor result owns the guard through the durable acknowledgement as well.
        self.store.call(move |c| Ok((Self::put(c, owner, scope, digest)?, guard))).await.map(|(added, _guard)| added)
    }

    fn put(c:&crate::sql::Connection<'_>,owner:String,scope:Scope,digest:String)->anyhow::Result<bool> {
        Ok(c.execute("INSERT INTO runner_work_holds(owner,repo,domain,digest) VALUES(?1,?2,?3,?4) ON CONFLICT(owner,repo,domain,digest) DO NOTHING",params![owner,scope.repo,scope.domain,digest])?==1)
    }

    fn import_journal(&self,path:&Path)->anyhow::Result<()> {
        let source=format!("runner-holds:{}",self.owner);
        let key=source.clone();
        let previous=self.store.now(move|c|Ok(c.query_row("SELECT identity,cursor,done FROM runner_work_imports WHERE source=?1",[key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,bool>(2)?))).optional()?))?;
        if previous.as_ref().is_some_and(|(_,_,done)|*done) { return Ok(()); }
        if !path.is_file() {
            anyhow::ensure!(previous.is_none(),"runner holdings import is incomplete and its source {} is unavailable; restore the pinned source before accepting work",path.display());
            return Ok(());
        }
        // A descriptor pins the old journal across rename-based compaction. Ignore later append
        // bytes consistently by hashing and reading the same captured length.
        let mut file=std::fs::File::open(path)?;
        let length=file.metadata()?.len();
        let mut snapshot=file.by_ref().take(length);
        let mut digest=Sha256::new();
        let mut buffer=[0u8;65536];
        loop { let n=snapshot.read(&mut buffer)?; if n==0 { break; } digest.update(buffer.get(..n).context("journal read exceeded buffer")?); }
        let identity=hex::encode(digest.finalize());
        let mut offset=if let Some((known,cursor,_))=previous {
            anyhow::ensure!(identity==known,"the legacy runner holdings journal changed during import; restore its original snapshot");
            postcard::from_bytes::<u64>(&cursor)?
        } else {
            let (key,identity)=(source.clone(),identity.clone());
            self.store.now(move|c| { c.execute("INSERT INTO runner_work_imports(source,identity,cursor,done) VALUES(?1,?2,?3,0)",params![key,identity,postcard::to_allocvec(&0u64)?])?; Ok(()) })?;
            0
        };
        anyhow::ensure!(offset<=length,"runner holdings cursor is past the source snapshot");
        file.seek(std::io::SeekFrom::Start(offset))?;
        let mut input=std::io::BufReader::new(file.take(length-offset));
        loop {
            let start=offset;
            let mut lines=Vec::new();
            let mut done=false;
            for _ in 0..128 {
                let mut bytes=Vec::new();
                let n=input.by_ref().take((1<<20)+1).read_until(b'\n',&mut bytes)?;
                anyhow::ensure!(n<=1<<20,"legacy runner holdings line exceeds one MiB");
                if n==0 { done=true; break; }
                offset=offset.checked_add(u64::try_from(n)?).context("journal cursor overflow")?;
                match serde_json::from_slice::<Line>(&bytes) {
                    Ok(line) => {
                        let Line::Hold{scope,digest}=&line;
                        anyhow::ensure!(is_digest(digest)&&!scope.repo.is_empty()&&super::super::token::known_domain(&scope.domain),"invalid legacy runner holding");
                        lines.push(line);
                    }
                    Err(_) if offset==length&&!bytes.ends_with(b"\n") => { done=true; break; }
                    Err(error)=>return Err(error).context("corrupt legacy runner holdings journal"),
                }
            }
            let (owner,source,identity)=(self.owner.clone(),source.clone(),identity.clone());
            self.store.now(move|c| {
                let state:(String,Vec<u8>,bool)=c.query_row("SELECT identity,cursor,done FROM runner_work_imports WHERE source=?1",[&source],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
                anyhow::ensure!(state.0==identity&&state.1==postcard::to_allocvec(&start)?&&!state.2,"runner holdings source or cursor changed");
                for Line::Hold{scope,digest} in lines { c.execute("INSERT INTO runner_work_holds(owner,repo,domain,digest) VALUES(?1,?2,?3,?4) ON CONFLICT(owner,repo,domain,digest) DO NOTHING",params![owner,scope.repo,scope.domain,digest])?; }
                c.execute("UPDATE runner_work_imports SET cursor=?2,done=?3 WHERE source=?1",params![source,postcard::to_allocvec(&offset)?,done])?;
                Ok(())
            })?;
            if done { return Ok(()); }
        }
    }
}
