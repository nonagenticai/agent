//! B3's outbox senders (plan §0 R5): one [`crate::provision::Sender`] per op string, registered in `state::build`.
pub mod connections;
pub mod store_delete;
pub mod stores;
