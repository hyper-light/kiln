//! Attention lifecycle, plan 01a199e1b177752e84dad5dc990e6a15: only declared automation acts.
//! Day counts are the person's settings; milliseconds/day is a unit conversion.
const DAY_MS:i64=86_400_000;

pub fn park_due(days:Option<u32>,last_use:Option<i64>,now:i64,quiet:bool,pinned:bool,parked:bool)->bool{
    quiet && !pinned && !parked && match(days,last_use){(Some(days),Some(last))if days>0=>now.saturating_sub(last)>=i64::from(days).saturating_mul(DAY_MS),_=>false}
}
pub fn archive_due(declared:bool,days:u32,parked_at:Option<i64>,now:i64,quiet:bool,other_person_used_it:bool)->bool{
    declared && days>0 && quiet && !other_person_used_it && parked_at.is_some_and(|at|now.saturating_sub(at)>=i64::from(days).saturating_mul(DAY_MS))
}

/// Plan 01a199e1b177752e84dad5dc990e6a15: Park defers the exact conditions the
/// person saw. A different or unidentified condition wakes it, regardless of clock order.
/// `all_deferred` is true for no current requests as well as an entirely captured set.
pub fn wake_park_requests(parked:bool,unknown:bool,all_deferred:bool)->bool{
    parked && (unknown || !all_deferred)
}
#[cfg(test)]mod tests{
    use super::*;
    #[test]fn mc_attention_only_declared_known_quiet_chats_park(){assert!(!park_due(None,Some(1),9*DAY_MS,true,false,false));assert!(!park_due(Some(3),None,9*DAY_MS,true,false,false));assert!(!park_due(Some(3),Some(1),9*DAY_MS,false,false,false));assert!(park_due(Some(3),Some(1),4*DAY_MS,true,false,false));assert!(!park_due(Some(3),Some(1),4*DAY_MS,true,true,false));}
    #[test]fn mc_attention_private_parking_cannot_archive_another_persons_work(){assert!(!archive_due(false,7,Some(1),9*DAY_MS,true,false));assert!(!archive_due(true,7,Some(1),9*DAY_MS,true,true));assert!(!archive_due(true,7,Some(1),9*DAY_MS,false,false));assert!(archive_due(true,7,Some(1),9*DAY_MS,true,false));}
    #[test]
    fn mc_attention_park_wakes_for_a_different_or_unknown_condition_not_clock_order(){
        for (parked,unknown,all_deferred,wakes) in [
            (false,false,false,false),(false,false,true,false),
            (false,true,false,false),(false,true,true,false),
            (true,false,false,true),(true,false,true,false),
            (true,true,false,true),(true,true,true,true),
        ]{
            assert_eq!(wake_park_requests(parked,unknown,all_deferred),wakes);
        }
    }
}
