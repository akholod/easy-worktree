//! The deliberately separate compensation effect boundary.
use crate::{
    compensation::CompensationActionV1,
    lifecycle::{CreatedArtifactV3, CreatedLocalBranch, CreatedWorktree, RepositoryIdentity},
};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityRefusal {
    Unsupported,
    PlatformUnsupported,
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactObservation {
    BeforeExact,
    AfterExact,
    Drift,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeObservation {
    BeforeExact,
    AfterExact,
    Drift,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchObservation {
    BeforeExact,
    AfterExact,
    Drift,
}

pub trait CompensationBackend {
    type Error;
    fn discover_repository(&self, anchor: &Path) -> Result<RepositoryIdentity, Self::Error>;
    fn check_capability(&self, action: &CompensationActionV1) -> Result<(), CapabilityRefusal>;
    fn observe_created_artifact(
        &self,
        value: &CreatedArtifactV3,
    ) -> Result<ArtifactObservation, Self::Error>;
    fn invoke_remove_created_artifact(&self, value: &CreatedArtifactV3) -> Result<(), Self::Error>;
    fn observe_created_worktree(
        &self,
        value: &CreatedWorktree,
    ) -> Result<WorktreeObservation, Self::Error>;
    fn invoke_remove_created_worktree(&self, value: &CreatedWorktree) -> Result<(), Self::Error>;
    fn observe_created_local_branch(
        &self,
        value: &CreatedLocalBranch,
    ) -> Result<BranchObservation, Self::Error>;
    fn invoke_delete_created_local_branch(
        &self,
        value: &CreatedLocalBranch,
    ) -> Result<(), Self::Error>;
}
