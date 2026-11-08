//! Relative examination, plan 01a199e1b177752e84dad5dc990e6a15 step 5.
//!
//! PBM assumes P(click | row, position, context) = examination(position, context)
//! times relevance(row, context). We harvest position contrasts from two ordinary
//! Thompson proposals assigned by a fair arm draw. This preserves the marginal ranking
//! distribution. Assignment is independent of the current response, conditional on the
//! two proposals; no per-row relevance model, extra swaps or exploration rate is used.
//!
//! AllPairs shares a nuisance mean relevance for each unordered position pair. Its
//! Bernoulli likelihood estimates relative examination within connected position graphs.
//! Each component is normalized at its most-examined fitted position, not assumed rank 1.
//! Layouts/components have no identified common absolute scale. The caller applies
//! self-normalized relative correction, never labels it globally unbiased, and never
//! fills historical missing propensities from future observations.
//!
//! Sources: Agarwal et al. (2019), doi:10.1145/3289600.3291017, §§3–4,
//! https://www.cs.cornell.edu/people/tj/publications/agarwal_etal_19a.pdf;
//! Di Benedetto et al. (2023), Policy-Aware Contextual Intervention Harvesting,
//! https://ceur-ws.org/Vol-3590/short4.pdf. PBM is a modeling assumption, not proof
//! that real attention ignores surrounding rows or trust effects.
use std::collections::{BTreeMap,BTreeSet};
use serde::{Deserialize,Serialize};
use crate::attention::RelativeCorrection;

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct Selection{pub agent:String,pub at:i64}

/// Exact UI projection of both proposals through the same filters/Needs/pins. `shown`
/// must equal the assigned proposal. Manual sorts or unknown projection set model_order
/// false. Open visits stay censored; only closed visits can contribute non-click labels.
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct Exposure{
    pub id:String,pub at:i64,pub closed_at:Option<i64>,pub context:String,
    pub order_a:Vec<String>,pub order_b:Vec<String>,pub shown:Vec<String>,pub arm:u8,
    pub choice:Option<Selection>,pub model_order:bool,
}

#[derive(Debug,Clone,PartialEq,Serialize,Deserialize)]
pub struct Position{pub position:usize,pub relative:f64,pub standard_error:Option<f64>}
#[derive(Debug,Clone,PartialEq,Serialize,Deserialize)]
pub struct Curve{
    pub layout:String,pub reference:usize,pub positions:Vec<Position>,
    pub opportunities:usize,pub reason:String,
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct Unavailable{pub layout:String,pub positions:Vec<usize>,pub reason:String}
#[derive(Debug,Clone,Default,PartialEq,Serialize,Deserialize)]
pub struct Report{
    pub curves:Vec<Curve>,pub unavailable:Vec<Unavailable>,pub exposures:usize,
    pub censored:usize,pub ignored:usize,pub opportunities:usize,
}

#[derive(Clone,Default)]
struct Edge{a:usize,b:usize,na:f64,nb:f64,ca:f64,cb:f64,arm_a:bool,arm_b:bool}

fn ranks(order:&[String])->Option<BTreeMap<&str,usize>>{
    let out:BTreeMap<_,_>=order.iter().enumerate().map(|(i,id)|(id.as_str(),i+1)).collect();
    (out.len()==order.len() && order.iter().all(|id|!id.is_empty())).then_some(out)
}

/// Only observations closed strictly before now are used. In particular the current
/// choice cannot supply its own examination correction or leak into held-out feedback.
pub fn fit(exposures:&[Exposure],now:i64)->Report{
    let mut report=Report::default();let mut layouts:BTreeMap<String,BTreeMap<(usize,usize),Edge>>=BTreeMap::new();
    let mut seen=BTreeSet::new();
    for exposure in exposures{
        if !seen.insert(exposure.id.as_str()){report.ignored+=1;continue;}
        let Some(closed)=exposure.closed_at.filter(|closed|*closed<now) else{report.censored+=1;continue;};
        if exposure.id.is_empty() || exposure.context.is_empty() || !exposure.model_order || exposure.at<0 || closed<exposure.at || exposure.arm>1{report.ignored+=1;continue;}
        let selected=if exposure.arm==0{&exposure.order_a}else{&exposure.order_b};
        let Some(a)=ranks(&exposure.order_a) else{report.ignored+=1;continue;};
        let Some(b)=ranks(&exposure.order_b) else{report.ignored+=1;continue;};
        if selected!=&exposure.shown || exposure.choice.as_ref().is_some_and(|choice|choice.at<exposure.at || choice.at>closed || !selected.contains(&choice.agent)){report.ignored+=1;continue;}
        report.exposures+=1;
        let edges=layouts.entry(exposure.context.clone()).or_default();
        // Pagination may give the proposals different visible sets: only rows present in
        // both projected orders identify a position contrast. There is no invented exposure.
        for(id,ka)in a{
            let Some(kb)=b.get(id).copied().filter(|kb|*kb!=ka) else{continue;};
            let(pair_a,pair_b)=(ka.min(kb),ka.max(kb));
            let edge=edges.entry((pair_a,pair_b)).or_insert_with(||Edge{a:pair_a,b:pair_b,..Default::default()});
            if exposure.arm==0{edge.arm_a=true;}else{edge.arm_b=true;}
            let position=if exposure.arm==0{ka}else{kb};
            let clicked=if exposure.choice.as_ref().is_some_and(|choice|choice.agent==id){1.0}else{0.0};
            if position==pair_a{edge.na+=1.0;edge.ca+=clicked;}else{edge.nb+=1.0;edge.cb+=clicked;}
            report.opportunities+=1;
        }
    }
    for(layout,edges)in layouts{
        let mut informative=Vec::new();
        for edge in edges.into_values(){
            if !edge.arm_a || !edge.arm_b || edge.na==0.0 || edge.nb==0.0 || edge.ca+edge.cb==0.0 || edge.ca+edge.cb==edge.na+edge.nb{
                report.unavailable.push(Unavailable{layout:layout.clone(),positions:vec![edge.a,edge.b],reason:"This pair has no two-arm interior click/non-click evidence; no prior or pseudo-count was supplied.".into()});
            }else{informative.push(edge);}
        }
        let mut graph:BTreeMap<usize,BTreeSet<usize>>=BTreeMap::new();
        for edge in &informative{graph.entry(edge.a).or_default().insert(edge.b);graph.entry(edge.b).or_default().insert(edge.a);}
        let mut remaining:BTreeSet<_>=graph.keys().copied().collect();
        while let Some(first)=remaining.first().copied(){
            let mut component=BTreeSet::new();let mut todo=vec![first];
            while let Some(position)=todo.pop(){if component.insert(position){remaining.remove(&position);todo.extend(graph.get(&position).into_iter().flatten().copied());}}
            let positions:Vec<_>=component.into_iter().collect();
            let edges:Vec<_>=informative.iter().filter(|edge|positions.binary_search(&edge.a).is_ok()).cloned().collect();
            match estimate(&layout,&positions,&edges){Some(curve)=>report.curves.push(curve),None=>report.unavailable.push(Unavailable{layout:layout.clone(),positions,reason:"The relative PBM likelihood is boundary, singular or unconverged; correction remains unknown.".into()})}
        }
    }
    report
}

/// Carries the reference explicitly; the ranker self-normalizes by layout/reference.
/// Unobserved ranks and disconnected components are never silently interpolated.
pub fn correction(report:&Report,layout:&str,position:usize)->Option<RelativeCorrection>{
    report.curves.iter().filter(|curve|curve.layout==layout).find_map(|curve|curve.positions.iter().find(|estimate|estimate.position==position).map(|estimate|RelativeCorrection{layout:layout.into(),reference:curve.reference,examination:estimate.relative}))
}

struct Objective{loss:f64,gradient:Vec<f64>,information:Vec<Vec<f64>>,nuisance:Vec<(f64,f64,f64)>}
fn add(vector:&mut[f64],index:usize,value:f64)->Option<()>{*vector.get_mut(index)?+=value;Some(())}
fn cell(matrix:&mut[Vec<f64>],row:usize,col:usize,value:f64)->Option<()>{*matrix.get_mut(row)?.get_mut(col)?+=value;Some(())}
fn norm(values:impl IntoIterator<Item=f64>)->f64{values.into_iter().map(f64::abs).fold(0.0,f64::max)}

/// Profile the pair-relevance Fisher block. The remaining information is a weighted
/// position-graph Laplacian, with the first position anchored to zero log-examination.
/// This avoids an (edges + positions)^2 matrix and retains every identified contrast.
fn objective(theta:&[f64],mu:&[f64],indices:&BTreeMap<usize,usize>,edges:&[Edge])->Option<Objective>{
    let dim=theta.len().checked_sub(1)?;
    let mut out=Objective{loss:0.0,gradient:vec![0.0;dim],information:vec![vec![0.0;dim];dim],nuisance:Vec::new()};
    for(edge,mu)in edges.iter().zip(mu){
        let(ia,ib)=(*indices.get(&edge.a)?,*indices.get(&edge.b)?);
        let(ta,tb)=(theta.get(ia)?+mu,theta.get(ib)?+mu);let(qa,qb)=(ta.exp(),tb.exp());
        if !ta.is_finite() || !tb.is_finite() || qa<=0.0 || qb<=0.0 || qa>=1.0 || qb>=1.0{return None;}
        out.loss-=edge.ca*ta+(edge.na-edge.ca)*(-qa).ln_1p()+edge.cb*tb+(edge.nb-edge.cb)*(-qb).ln_1p();
        let(ga,gb)=((edge.na*qa-edge.ca)/(1.0-qa),(edge.nb*qb-edge.cb)/(1.0-qb));
        let(wa,wb)=(edge.na*qa/(1.0-qa),edge.nb*qb/(1.0-qb));let total=wa+wb;
        let score=(wb*ga-wa*gb)/total;let info=wa*wb/total;
        if let Some(i)=ia.checked_sub(1){add(&mut out.gradient,i,score)?;cell(&mut out.information,i,i,info)?;}
        if let Some(i)=ib.checked_sub(1){add(&mut out.gradient,i,-score)?;cell(&mut out.information,i,i,info)?;}
        if let(Some(i),Some(j))=(ia.checked_sub(1),ib.checked_sub(1)){cell(&mut out.information,i,j,-info)?;cell(&mut out.information,j,i,-info)?;}
        out.nuisance.push((ga+gb,wa,wb));
    }
    (out.loss.is_finite() && out.gradient.iter().all(|v|v.is_finite()) && out.information.iter().flatten().all(|v|v.is_finite()) && out.nuisance.iter().all(|(g,a,b)|g.is_finite() && a.is_finite() && b.is_finite())).then_some(out)
}

fn estimate(layout:&str,positions:&[usize],edges:&[Edge])->Option<Curve>{
    let indices:BTreeMap<_,_>=positions.iter().enumerate().map(|(index,position)|(*position,index)).collect();
    let mut theta=vec![0.0;positions.len()];
    let mut mu:Vec<_>=edges.iter().map(|edge|((edge.ca+edge.cb)/(edge.na+edge.nb)).ln()).collect();
    let total:f64=edges.iter().map(|edge|edge.na+edge.nb).sum();
    let accuracy=f64::EPSILON.sqrt();
    let dim=positions.len().checked_sub(1)?.saturating_add(edges.len());
    for _ in 0..(f64::MANTISSA_DIGITS as usize).saturating_mul(dim){
        let current=objective(&theta,&mu,&indices,edges)?;let covariance=crate::attention::inverse(&current.information)?;
        let step:Vec<_>=std::iter::once(0.0).chain(covariance.iter().map(|row|row.iter().zip(&current.gradient).map(|(a,b)|a*b).sum::<f64>())).collect();
        let nuisance:Vec<_>=edges.iter().zip(&current.nuisance).map(|(edge,(score,wa,wb))|Some((score-wa*step.get(*indices.get(&edge.a)?)?-wb*step.get(*indices.get(&edge.b)?)?)/(wa+wb))).collect::<Option<_>>()?;
        if !step.iter().chain(&nuisance).all(|value|value.is_finite()){return None;}
        let score=norm(current.gradient.iter().copied().chain(current.nuisance.iter().map(|(g,_,_)|*g)));
        if score<=accuracy*total && norm(step.iter().chain(&nuisance).copied())<=accuracy*(1.0+norm(theta.iter().chain(&mu).copied())){
            return curve(layout,positions,edges,&theta,&covariance);
        }
        let mut factor=1.0;
        let next=loop{
            let t:Vec<_>=theta.iter().zip(&step).map(|(t,s)|t-factor*s).collect();let m:Vec<_>=mu.iter().zip(&nuisance).map(|(m,s)|m-factor*s).collect();
            if t==theta && m==mu{break None;}
            if let Some(proposed)=objective(&t,&m,&indices,edges) && proposed.loss<current.loss{break Some((t,m));}
            factor/=2.0;if factor<f64::EPSILON{break None;}
        };
        let(next_theta,next_mu)=next?;theta=next_theta;mu=next_mu;
    }
    None
}

fn curve(layout:&str,positions:&[usize],edges:&[Edge],theta:&[f64],covariance:&[Vec<f64>])->Option<Curve>{
    let(reference,maximum)=theta.iter().enumerate().max_by(|(ia,a),(ib,b)|a.total_cmp(b).then_with(||ib.cmp(ia)))?;
    let cov=|a:usize,b:usize|if let(Some(a),Some(b))=(a.checked_sub(1),b.checked_sub(1)){covariance.get(a).and_then(|row|row.get(b)).copied()}else{Some(0.0)};
    let estimates:Vec<_>=positions.iter().zip(theta).enumerate().map(|(index,(position,value))|{
        let relative=(value-maximum).exp();let variance=cov(index,index)?+cov(reference,reference)?-2.0*cov(index,reference)?;
        (relative.is_finite() && relative>0.0 && relative<=1.0).then(||Position{position:*position,relative,standard_error:(variance>=0.0).then(||relative*variance.sqrt()).filter(|value|value.is_finite())})
    }).collect::<Option<_>>()?;
    Some(Curve{layout:layout.into(),reference:*positions.get(reference)?,positions:estimates,opportunities:edges.iter().map(|edge|(edge.na+edge.nb)as usize).sum(),reason:"Relative PBM AllPairs fit from randomized Thompson position contrasts; max fitted examination is normalized to 1. Local Fisher uncertainty is model-based; no absolute or cross-layout/component scale is identified.".into()})
}

#[cfg(test)]mod tests;
