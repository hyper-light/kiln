//! U10: canonical environment policies, on the common Records namespace. Legacy SQL and CD
//! records are import inputs only; absent-row CAS makes interrupted imports restartable and
//! prevents an import from overwriting a policy edited after the first successful copy.

use kiln_ci::env::{EnvironmentId, EnvironmentPolicy};
use kiln_seams::records::{Cond, Consistency, Records, Refusal, Tx, Write};

/// U10/U19: only explicit absence can restore an overridden saved definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SourceState { Present, Absent, Invalid(String), Unavailable(String) }

impl SourceState {
    pub(super) fn check(&self)->anyhow::Result<()> {
        match self {
            Self::Present|Self::Absent=>Ok(()),
            Self::Invalid(why)=>anyhow::bail!("invalid environment configuration: {why}"),
            Self::Unavailable(why)=>anyhow::bail!("environment configuration unavailable: {why}"),
        }
    }
}

const FAMILY: &str = "environment.policy";

fn refused(error: Refusal) -> anyhow::Error { anyhow::anyhow!("environment policy: {error}") }

pub async fn get(records: &dyn Records, id: &EnvironmentId) -> anyhow::Result<Option<EnvironmentPolicy>> {
    records.get(FAMILY, &id.key()?, Consistency::Linearizable).await.map_err(refused)?
        .map(|row| serde_json::from_slice(&row.value).map_err(anyhow::Error::from)).transpose()
}

pub async fn import(records: &dyn Records, id: &EnvironmentId, legacy: &EnvironmentPolicy) -> anyhow::Result<EnvironmentPolicy> {
    for _ in 0..8 {
        if let Some(policy) = get(records, id).await? { return Ok(policy); }
        let key = id.key()?;
        match records.commit(Tx {
            conds: vec![Cond::Absent { family: FAMILY.into(), key: key.clone() }],
            writes: vec![Write::Put { family: FAMILY.into(), key, value: serde_json::to_vec(legacy)? }],
        }).await {
            Ok(_) => return Ok(legacy.clone()),
            Err(Refusal::Conflict { .. }) => {}
            Err(error) => return Err(refused(error)),
        }
    }
    anyhow::bail!("environment policy import kept racing for {}", id.name)
}

/// Resolve declared configuration over the saved definition without destroying the saved
/// restrictions. Only the selected definition is evaluated. A removed file restores its
/// snapshot, while a restarted importer keeps every subsequently saved edit.
pub(super) async fn resolve_source(records: &dyn Records, id: &EnvironmentId, mut input: EnvironmentPolicy, fallback: Option<EnvironmentPolicy>, state: SourceState) -> anyhow::Result<EnvironmentPolicy> {
    state.check()?;
    if state==SourceState::Present {
        input.fallback = match get(records, id).await? {
            Some(previous) if previous.source == input.source => previous.fallback,
            Some(previous) => Some(Box::new(previous)),
            None => fallback.map(Box::new),
        };
        save(records, id, &input).await?;
        return Ok(input);
    }
    let imported = import(records, id, &input).await?;
    if imported.source == "file" {
        let restored = imported.fallback.map_or(input, |saved| *saved);
        save(records, id, &restored).await?;
        return Ok(restored);
    }
    Ok(imported)
}

pub fn write(id: &EnvironmentId, policy: &EnvironmentPolicy) -> anyhow::Result<Write> {
    Ok(Write::Put { family: FAMILY.into(), key: id.key()?, value: serde_json::to_vec(policy)? })
}

pub async fn save(records: &dyn Records, id: &EnvironmentId, policy: &EnvironmentPolicy) -> anyhow::Result<()> {
    policy.protection.check().map_err(anyhow::Error::msg)?;
    records.commit(Tx { conds: Vec::new(), writes: vec![write(id, policy)?] }).await.map_err(refused)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_ci::env::{Approvals, EnvironmentScope, Protection};

    #[test]
    fn u10_invalid_or_unavailable_source_never_publishes_default_or_removes_saved_restrictions()->anyhow::Result<()> {
        crate::rt::block_on(async{
            let records=kiln_seams::records::SimRecords::new();
            let id=EnvironmentId{scope:EnvironmentScope::Repository("source-regression".into()),name:"production".into()};
            let saved=EnvironmentPolicy{source:"file".into(),protection:Protection{reviewers:vec!["alice".into()],branches:Some(vec!["main".into()]),freeze:Some("incident".into()),..Default::default()},..Default::default()};
            save(&records,&id,&saved).await?;
            let decoded=super::super::source::decode(".kiln/environments.yaml",b"production:
  reviewers: [alice]
  branches: [main]
  freeze: incident
  windows: [{allow: bad-cron, duration: 1h}]
");
            assert!(matches!(&decoded.state,SourceState::Invalid(why) if why.contains("window")));
            for state in [decoded.state,SourceState::Unavailable("Git tree could not be read".into())] {
                let refused=resolve_source(&records,&id,EnvironmentPolicy::default(),None,state).await;
                assert!(refused.is_err(),"a broken source is not a deliberate removal");
                assert_eq!(get(&records,&id).await?,Some(saved.clone()));
            }
            assert!(matches!(super::super::source::decode(".kiln/environments.yaml",b"production: [broken").state,SourceState::Invalid(_)));
            Ok(())
        })
    }

    #[test]
    fn u10_removing_config_restores_the_overridden_saved_restrictions() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let records = kiln_seams::records::SimRecords::new();
            let id = EnvironmentId { scope: EnvironmentScope::Repository("repo".into()), name: "production".into() };
            let saved = EnvironmentPolicy { source: "kiln".into(), protection: Protection { reviewers: vec!["alice".into()], ..Default::default() }, ..Default::default() };
            let file = EnvironmentPolicy { source: "file".into(), protection: Protection { wait_timer: 5, ..Default::default() }, ..Default::default() };
            let chosen = resolve_source(&records, &id, file.clone(), Some(saved.clone()), SourceState::Present).await?;
            assert_eq!(chosen.protection.wait_timer, 5);
            // A second config refresh must not replace the saved snapshot with the last file.
            resolve_source(&records, &id, file, None, SourceState::Present).await?;
            let empty_metadata = EnvironmentPolicy { source: "kiln".into(), ..Default::default() };
            assert_eq!(resolve_source(&records, &id, empty_metadata, None, SourceState::Absent).await?, saved);
            Ok(())
        })
    }

    #[test]
    fn u10_restartable_import_preserves_differing_owners_and_later_edits() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let records = kiln_seams::records::SimRecords::new();
            let server = EnvironmentId { scope: EnvironmentScope::Server, name: "production".into() };
            let repository = EnvironmentId { scope: EnvironmentScope::Repository("repo-1".into()), name: "production".into() };
            let ci = EnvironmentPolicy { protection: Protection { reviewers: vec!["alice".into()], ..Default::default() }, concurrency: Some(1), ..Default::default() };
            let release = EnvironmentPolicy { protection: Protection { approvals: Some(Approvals { count: 2, reviewers: vec!["bob".into(), "carol".into()], self_approve: false }), ..Default::default() }, ..Default::default() };
            assert_eq!(import(&records, &repository, &ci).await?, ci);
            // A restart between imports neither conflates owners nor replaces the first copy.
            assert_eq!(import(&records, &repository, &release).await?, ci);
            assert_eq!(import(&records, &server, &release).await?, release);
            let edited = EnvironmentPolicy { concurrency: Some(3), ..release.clone() };
            save(&records, &server, &edited).await?;
            assert_eq!(import(&records, &server, &release).await?, edited);
            assert_eq!(get(&records, &repository).await?, Some(ci));
            Ok(())
        })
    }
}
