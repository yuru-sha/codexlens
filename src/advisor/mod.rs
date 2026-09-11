//! Public advisor facade.
//!
//! Responsibility-specific implementations live in the sibling modules while
//! this module preserves the established `codexlens::advisor` API.

mod apply;
mod diff;
mod proposal;
mod report;
mod scope;

#[cfg(test)]
pub(crate) mod test_support;

pub use apply::{
    ApplyError, ApplyPlan, ApplyReport, RecoveryStatus, prepare_apply, prepare_apply_proposals,
};
pub use diff::{DiffBatch, DiffError, RenderedDiff, SkippedProposal, render_diff, render_diffs};
pub use proposal::{Proposal, ProposalAction, ProposalError, ProposalPlan, proposals_for_findings};
pub use report::{
    DoctorFinding, DoctorGroup, DoctorOptions, DoctorReport, ReportCoverage, SessionSummary,
    doctor, doctor_with_coverage, render_doctor, render_doctor_with_coverage,
    render_doctor_with_period, render_json_diff, render_json_diff_with_period,
    render_json_finding_report, render_json_finding_report_with_coverage,
    render_json_finding_report_with_period, render_json_sessions, render_json_sessions_with_period,
    render_proposal_summary, render_report_metadata, render_report_metadata_with_period,
    report_coverage, report_coverage_with_period, report_sessions,
};
pub use scope::{ScopeRecommendation, recommend_scope};
