//! Public advisor facade.
//!
//! Responsibility-specific implementations live in the sibling modules while
//! this module preserves the established `codexlens::advisor` API.

mod diff;
mod proposal;
mod report;
mod scope;

#[cfg(test)]
pub(crate) mod test_support;

pub use diff::{DiffBatch, DiffError, RenderedDiff, SkippedProposal, render_diff, render_diffs};
pub use proposal::{Proposal, ProposalAction, ProposalError, ProposalPlan, proposals_for_findings};
pub use report::{
    DoctorFinding, DoctorGroup, DoctorOptions, DoctorReport, doctor, render_doctor,
    render_proposal_summary,
};
pub use scope::{ScopeRecommendation, recommend_scope};
