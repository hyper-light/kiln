//! U01/W05-R28: reset an already-expanded pipeline subgraph in the SAME plan.
//! The adapter proves actual execution retirement before applying this pure
//! transition. A plan's queued state alone cannot distinguish running work.
use std::collections::BTreeSet;

use super::{JobState, LegReport, PlanAction, PlanSource, RunPlan};

/// Durable log input for one retry. Expanded pipeline jobs each have one stable
/// key; unlike a new workflow run attempt, no unselected node is reconstructed.
#[derive(Clone,Debug,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
pub struct Selection {
    pub target: String,
    pub jobs: Vec<String>,
}

impl RunPlan {
    /// Select the exact transitive dependency closure without changing the plan.
    /// Imported v1 pipelines have one scope, one expanded job per saved key and
    /// ordinary job needs. Called/dynamic workflow jobs use their existing rerun.
    pub fn pipeline_retry_selection(&self,target:&str)->Result<Selection,String> {
        if self.cancelled {return Err("the run is cancelled".into());}
        if self.scopes.len()!=1 || self.nodes.iter().any(|n|n.scope!=0 || !n.leg_needs.is_empty()
            || n.legs.len()>1 || n.legs.iter().any(|l|l.called.is_some() || l.spec.key!=n.id) || n.fail_fast.run) {
            return Err("in-place retry requires the original already-expanded pipeline plan".into());
        }
        let root=self.nodes.iter().position(|n|n.id==target).ok_or_else(||format!("the plan has no job `{target}`"))?;
        let node=self.nodes.get(root).ok_or_else(||"the retry target disappeared".to_owned())?;
        if node.state!=JobState::Done {return Err(format!("`{target}` has not finished"));}
        if node.result==Some(super::Conclusion::Skipped) {return Err(format!("`{target}` was skipped: retry the job it needs, or the run"));}
        let mut dependents=vec![Vec::new();self.nodes.len()];
        for (index,node) in self.nodes.iter().enumerate() {
            for dependency in &node.needs {
                dependents.get_mut(*dependency).ok_or_else(||"pipeline dependency is outside its original plan".to_owned())?.push(index);
            }
        }
        let mut selected=BTreeSet::from([root]);
        let mut waiting=std::collections::VecDeque::from([root]);
        while let Some(parent)=waiting.pop_front() {
            for child in dependents.get(parent).ok_or_else(||"retry dependency node disappeared".to_owned())? {
                if selected.insert(*child) {waiting.push_back(*child);}
            }
        }
        Ok(Selection {target:target.into(),jobs:selected.into_iter().filter_map(|i|self.nodes.get(i).map(|n|n.id.clone())).collect()})
    }

    /// Apply a durably accepted selection to the CURRENT plan. This can occur
    /// after unrelated completions; installing a formerly cloned whole plan
    /// here would discard those original results and in-flight ownership.
    pub fn retry_pipeline(&mut self,selection:&Selection,source:&dyn PlanSource)->Result<Vec<PlanAction>,String> {
        let current=self.pipeline_retry_selection(&selection.target)?;
        if current!=*selection {return Err("the accepted retry subgraph changed".into());}
        let selected:BTreeSet<usize>=self.nodes.iter().enumerate().filter(|(_,n)|selection.jobs.contains(&n.id)).map(|(i,_)|i).collect();
        let mut next=self.clone();
        next.keys.retain(|_,(n,_)|!selected.contains(n));
        next.run_env.retain(|(n,_,_)|!selected.contains(n));
        next.removed.retain(|job,_|!selection.jobs.contains(job));
        for n in &selected {
            let node=next.nodes.get_mut(*n).ok_or_else(||"accepted retry node disappeared".to_owned())?;
            node.state=JobState::Waiting;
            node.result=None;
            node.legs.clear();
        }
        next.output_bytes=0;
        for node in &next.nodes {
            for leg in &node.legs {
                let bytes=report_bytes(&leg.report)?;
                next.output_bytes=next.output_bytes.checked_add(bytes).ok_or_else(||"retained output accounting overflow".to_owned())?;
            }
        }
        next.finished=None;
        let mut actions=Vec::new();
        next.advance(source,&mut actions);
        // The selected closure contains every dependent. Advancing this reset
        // cannot silently replace/cancel some other queued original execution.
        if actions.iter().any(|action|match action {
            PlanAction::Queue(leg)|PlanAction::Settle {leg,..}=>!selection.jobs.contains(&leg.job),
            PlanAction::Job {job,..}=>!selection.jobs.contains(job),
            PlanAction::Cancel {..}=>true,
            PlanAction::Warn {leg,..}=>!selection.jobs.contains(leg),
            PlanAction::Finish(_)=>false,
        }) {return Err("in-place retry affected an unselected job".into());}
        *self=next;
        Ok(actions)
    }
}

fn report_bytes(report:&LegReport)->Result<usize,String> {
    let mut bytes=0usize;
    for (key,value) in &report.outputs {
        bytes=bytes.checked_add(key.len()).and_then(|n|n.checked_add(value.len())).ok_or_else(||"retained output accounting overflow".to_owned())?;
    }
    for (key,value) in &report.json_outputs {
        bytes=bytes.checked_add(key.len()).and_then(|n|n.checked_add(value.to_string().len())).ok_or_else(||"retained output accounting overflow".to_owned())?;
    }
    Ok(bytes)
}
