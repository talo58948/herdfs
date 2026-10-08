use serde::{Deserialize, Serialize};

use super::Perm;
use crate::SpacePath;

/// A space reachable by the caller and any permission it holds at the root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceAccess {
    pub space: String,
    pub root_perm: Option<Perm>,
}

/// One explicit ACL row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AclRow {
    pub path: SpacePath,
    pub principal: String,
    pub perm: Perm,
}
