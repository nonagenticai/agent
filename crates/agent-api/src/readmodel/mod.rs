//! Read models (B1 X7): the answers of the UI's read methods, built from the tables B1 owns. Each submodule names
//! the extension points later plans fill (B2 releases, B3 data plane, B4 runtime, B7 licensing, B8 generator); B1
//! returns honest defaults there, never sample data.
pub mod agent_status;
pub mod home;
pub mod job_attention;
pub mod jobs;
pub mod platform;
pub mod tenant;
pub mod wording;
