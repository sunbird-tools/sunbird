//! Locates `ForwardDialog`'s single-destination `Workspace` channel switch
//! (and the sibling `clips-gallery` switch) for `phoenix.ui.disableForwardFollow`.

use std::collections::HashSet;

use crate::{
   Found,
   index::{Index, Op},
};

/// Targets for skipping post-forward channel switching on Desktop Phoenix.
#[derive(Clone, Copy, Debug)]
pub struct ForwardFollow {
   /// `ForwardDialog`'s `Workspace` channel navigation target.
   pub navigate: usize,
   /// `clips-gallery`'s `Workspace` channel navigation target, when present.
   pub clips: Option<usize>,
}

/// Locates `ForwardDialog`'s `Workspace` channel navigation target by walking
/// from `"chat-forward"` through its small single-destination switch helper to
/// the `"discord_desktop::workspace::Workspace"` target.
pub fn forward_follow(ix: &Index<'_>) -> Found<ForwardFollow> {
   let workspace_funcs: HashSet<usize> = ix
      .literal(b"discord_desktop::workspace::Workspace")
      .iter()
      .map(|func| func.start)
      .collect();
   if workspace_funcs.is_empty() {
      return Err("no Workspace literal found".to_owned());
   }

   let mut candidates = Vec::new();
   for caller in ix.literal(b"chat-forward").iter() {
      for callee in caller.calls() {
         let Ok(helper) = ix.func(callee) else {
            continue;
         };
         if helper.size() > 256 {
            continue;
         }
         let calls = helper.calls();
         if calls.len() != 1
            || !workspace_funcs.contains(&calls[0])
            || helper.callers().iter().count() != 1
         {
            continue;
         }
         let steps = helper.flat_steps();
         let checks_single = steps.iter().any(|step| {
            matches!(
               step.op,
               Op::Cmp { value: 1, .. } | Op::CmpMem { value: 1, .. }
            )
         });
         let branches = steps.iter().any(|step| matches!(step.op, Op::Branch(_)));
         if checks_single
            && branches
            && ix
               .func(calls[0])
               .is_ok_and(|func| func.callers().iter().count() == 1)
         {
            candidates.push(calls[0]);
         }
      }
   }
   candidates.sort_unstable();
   candidates.dedup();
   let [navigate] = candidates.as_slice() else {
      return Err(format!(
         "expected 1 forward follow target from chat-forward, found {}",
         candidates.len()
      ));
   };
   let navigate = *navigate;

   let mut clip_candidates = Vec::new();
   for caller in ix.literal(b"clips-gallery").iter() {
      for callee in caller.calls() {
         let Ok(wrapper) = ix.func(callee) else {
            continue;
         };
         if wrapper.size() > 256 {
            continue;
         }
         let calls = wrapper.calls();
         if calls.len() != 1 || wrapper.callers().iter().count() != 1 {
            continue;
         }
         let target = calls[0];
         if target == navigate || !workspace_funcs.contains(&target) {
            continue;
         }
         if ix
            .func(target)
            .is_ok_and(|func| func.callers().iter().count() == 1)
         {
            clip_candidates.push(target);
         }
      }
   }
   clip_candidates.sort_unstable();
   clip_candidates.dedup();
   let clips = match clip_candidates.as_slice() {
      [] => None,
      [only] => Some(*only),
      many => {
         return Err(format!(
            "expected at most 1 clips forward follow target, found {}",
            many.len()
         ));
      },
   };

   Ok(ForwardFollow { navigate, clips })
}
