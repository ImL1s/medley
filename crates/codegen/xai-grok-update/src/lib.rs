pub mod auto_update;
pub mod dist_channel;
pub mod version;
mod version_policy;

pub use auto_update::UpdateStatus;
pub use dist_channel::{DistIdentity, SelfUpdate, self_update_refusal};
pub use version::{UpdateConfig, channel_label, channel_name, write_version_cache};
pub use version_policy::enforce_version_policy_or_exit;
