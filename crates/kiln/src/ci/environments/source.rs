//! U10/U19: absence is evidence from a successfully read immutable tree, or a native
//! repository proven to have no refs yet. Git and YAML failures cannot publish a policy.

use super::{FileEnv,FileTemplate,FromFile};
use super::policy::SourceState;
use std::collections::BTreeMap;
use crate::hub::Hub;
use crate::store::Repo;

pub(super) fn decode(name:&str,text:&[u8])->FromFile {
    let parsed=(||->Result<FromFile,String>{
        let mut mapping:serde_yaml::Mapping=serde_yaml::from_slice(text).map_err(|e|format!("{name}: {e}"))?;
        let previews=mapping.remove("previews");
        let environments:BTreeMap<String,FileEnv>=serde_yaml::from_value(serde_yaml::Value::Mapping(mapping)).map_err(|e|format!("{name}: {e}"))?;
        let previews:Option<BTreeMap<String,FileTemplate>>=previews.map(serde_yaml::from_value).transpose().map_err(|e|format!("{name}: previews: {e}"))?;
        let mut out=FromFile{state:SourceState::Present,envs:BTreeMap::new(),previews:BTreeMap::new()};
        for (environment,f) in environments {
            if environment.trim()!=environment||environment.is_empty(){return Err(format!("{name}: invalid environment name `{environment}`"));}
            if out.envs.contains_key(&environment.to_ascii_lowercase()){return Err(format!("{name}: duplicate environment identity `{environment}`"));}
            f.protection.check().map_err(|e|format!("{name}: {environment}: {e}"))?;
            if f.concurrency==Some(0){return Err(format!("{name}: {environment}: concurrency is 1 or more"));}
            let (kind,targets)=f.kind("",&environment).map_err(|e|format!("{name}: {environment}: {e}"))?;
            out.envs.insert(environment.to_ascii_lowercase(),super::EnvDef{name:environment,protection:f.protection,url:f.url,kind,targets,source:"file".into(),agents_approve:f.agents.approve,concurrency:f.concurrency,preview:None});
        }
        for (name_in_file,f) in previews.unwrap_or_default() {
            if name_in_file.contains('/')||name_in_file.trim()!=name_in_file||name_in_file.is_empty(){return Err(format!("{name}: previews: invalid environment name `{name_in_file}`"));}
            if out.previews.contains_key(&name_in_file.to_ascii_lowercase()){return Err(format!("{name}: duplicate preview identity `{name_in_file}`"));}
            let template=f.template("",&name_in_file).map_err(|e|format!("{name}: previews.{name_in_file}: {e}"))?;
            out.previews.insert(name_in_file.to_ascii_lowercase(),template);
        }
        Ok(out)
    })();
    parsed.unwrap_or_else(|why|FromFile::failed(SourceState::Invalid(why)))
}

pub(super) async fn read(hub:&Hub,repo:&Repo)->FromFile {
    let native=super::super::hosting::native_path(hub,repo);
    let mirror=native.clone().unwrap_or_else(||crate::gitops::mirror_path(hub,&repo.name)).to_string_lossy().to_string();
    let fetched=(|| async {
        let branch=crate::gitops::run_git(hub,&["-C",&mirror,"symbolic-ref","--short","HEAD"],crate::gitops::LS_REMOTE_TIMEOUT).await?;
        let branch=String::from_utf8(branch)?.trim().to_owned();
        anyhow::ensure!(!branch.is_empty(),"repository default branch is unavailable");
        let tree=crate::gitops::run_git(hub,&["-C",&mirror,"rev-parse","--verify",&format!("{branch}^{{tree}}")],crate::gitops::LS_REMOTE_TIMEOUT).await;
        // U10/W15-R08: only native repositories whose refs are positively absent can
        // have no source before their first push. A failed tree read alone proves nothing.
        if tree.is_err()&&native.is_some()&&empty_native(hub,&mirror).await? {
            return Ok::<_,anyhow::Error>(FromFile::failed(SourceState::Absent));
        }
        let tree=String::from_utf8(tree?)?.trim().to_owned();
        anyhow::ensure!(!tree.is_empty(),"repository default tree is unavailable");
        let paths=crate::gitops::run_git(hub,&["-C",&mirror,"ls-tree","-r","--name-only","-z",&tree,"--",".kiln/environments.yaml",".kiln/environments.yml"],crate::gitops::LS_REMOTE_TIMEOUT).await?;
        let names=[".kiln/environments.yaml",".kiln/environments.yml"];
        let name=names.into_iter().find(|name|paths.split(|b|*b==0).any(|path|path==name.as_bytes()));
        let Some(name)=name else{return Ok::<_,anyhow::Error>(FromFile::failed(SourceState::Absent));};
        let text=crate::gitops::run_git(hub,&["-C",&mirror,"cat-file","blob",&format!("{tree}:{name}")],crate::gitops::LS_REMOTE_TIMEOUT).await?;
        Ok(decode(name,&text))
    })().await;
    fetched.unwrap_or_else(|error|FromFile::failed(SourceState::Unavailable(format!("{}: environment configuration source is unavailable: {error:#}",repo.name))))
}

/// show-ref reports no refs as status 1 with no output. Damaged refs, objects and
/// repositories are errors; they must not remove a saved policy. --quiet bounds output.
async fn empty_native(hub:&Hub,mirror:&str)->anyhow::Result<bool> {
    let mut command=crate::gitops::git(hub);
    command.args(["-C",mirror,"show-ref","--quiet","--head"]);
    let refs=crate::proc::output(command,crate::gitops::LS_REMOTE_TIMEOUT).await?;
    if refs.status.code()==Some(1)&&refs.stdout.is_empty()&&refs.stderr.is_empty() {return Ok(true);}
    anyhow::ensure!(refs.status.success(),"git show-ref: {}",crate::util::truncate(String::from_utf8_lossy(&refs.stderr).trim(),600));
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// U10/W15-R08: native repositories accept API environments before their first push.
    /// Missing or damaged data and unfetched mirrors must still fail closed.
    #[test]
    fn u10_unborn_native_source_is_absent_but_unavailable_sources_stay_failures()->anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub=crate::hub::tests::scratch_hub_with(|cfg|cfg.ci.enabled=false).await?;
            let mut repo=Repo {
                id:crate::util::new_id(),name:"acme/unborn".into(),clone_url:String::new(),
                branches:Vec::new(),poll_seconds:30,enabled:true,created_at:0,
                last_polled_at:None,last_error:None,mode:"native".into(),
                default_branch:Some("main".into()),mirror:None,
            };
            let native=crate::ci::hosting::create(&hub,&repo.name,"main",None).await?;
            assert_eq!(read(&hub,&repo).await.state,SourceState::Absent);
            std::fs::write(native.join("refs/heads/main"),"not a ref\n")?;
            assert!(matches!(read(&hub,&repo).await.state,SourceState::Unavailable(_)),"a broken ref isn't an absent policy");
            std::fs::remove_file(native.join("refs/heads/main"))?;
            std::fs::rename(native.join("HEAD"),native.join("HEAD-missing"))?;
            assert!(matches!(read(&hub,&repo).await.state,SourceState::Unavailable(_)),"a missing repository isn't an absent policy");
            repo.mode="upstream".into();
            let mirror=crate::gitops::mirror_path(&hub,&repo.name).to_string_lossy().to_string();
            crate::gitops::run_git(&hub,&["init","--quiet","--bare","--initial-branch=main",&mirror],crate::gitops::LS_REMOTE_TIMEOUT).await?;
            assert!(matches!(read(&hub,&repo).await.state,SourceState::Unavailable(_)),"an empty mirror proves nothing about its upstream");
            Ok(())
        })
    }
}
