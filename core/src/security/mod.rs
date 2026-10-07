pub mod auth_policy;
pub mod identity;
pub mod trust_model;
pub mod trust_store;

pub use auth_policy::{
    authorize, is_valid_subnet_entry, subnet_verdict, AuthContext, SubnetVerdict,
};
pub use identity::DeviceIdentity;
pub use trust_model::{AccessMode, AccessScope, Decision, Op, Presence, SessionGrant, TrustLevel};
pub use trust_store::{
    endpoint_kind_label, endpoint_priority, format_bytes, AccessAuditRow, BindOutcome,
    DeviceEndpoint, SecurityEvent, SecurityEventKind, TransferMetrics,
    TransferRecord, TrustStore, TrustedDevice, UnpairOutcome,
};
