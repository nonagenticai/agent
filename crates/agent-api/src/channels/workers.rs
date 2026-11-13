//! Shared pieces of the worker-side connectors (D104, plan B12 §5): the handover card's text and the fixed
//! confirmation texts a staff member gets in the thread after answering.
use super::core::{CoreError, StaffResp, StaffStatus, WorkerItem};

/// How many of the last customer-conversation lines a card shows, and how long each may be.
pub const EXCERPT_LINES: usize = 5;
pub const EXCERPT_CHARS: usize = 300;

/// Fixed confirmation texts (never an internal error).
pub const SENT: &str = "Sent to the customer.";
pub const STORED: &str =
    "Saved to the conversation. It was not sent to a customer channel (none is connected).";
pub const UNVERIFIED: &str = "I couldn't verify who you are; ask an admin to check your helpdesk email matches your agent account.";
pub const NOT_STAFF: &str =
    "Could not deliver: your account does not hold the Support agent role for this job.";
pub const NOT_DELIVERED: &str = "Could not deliver: the customer's channel did not accept the message. It is saved in the conversation.";
pub const FAILED: &str =
    "Could not deliver: something went wrong. Please try again or answer from the console.";
/// A colleague owns the conversation (agent-api 409 `owner_changed`, SW-5): the reply was not sent.
pub const OWNED_ELSEWHERE: &str =
    "Not sent: a colleague is answering this conversation in the workspace.";
/// The thread's handover item is closed (agent-api 409 `closed`): the reply was not sent.
pub const THREAD_CLOSED: &str =
    "Not sent: this handover is closed. Answer the customer from the workspace.";

/// The confirmation for agent-api's answer to a staff message; `None` = say nothing (a thread that is not a
/// handover thread: staff talking among themselves).
pub fn confirmation(r: &Result<StaffResp, CoreError>) -> Option<&'static str> {
    match r {
        Ok(StaffResp {
            status: StaffStatus::Delivered,
            ..
        }) => Some(SENT),
        Ok(StaffResp {
            status: StaffStatus::Stored,
            ..
        }) => Some(STORED),
        Err(CoreError::Code(c)) if c == "thread_not_found" => None,
        Err(CoreError::Code(c)) if c == "not_staff" => Some(NOT_STAFF),
        Err(CoreError::Code(c)) if c == "delivery_failed" => Some(NOT_DELIVERED),
        Err(CoreError::Code(c)) if c == "owner_changed" || c == "not_owner" => {
            Some(OWNED_ELSEWHERE)
        }
        Err(CoreError::Code(c)) if c == "closed" => Some(THREAD_CLOSED),
        Err(_) => Some(FAILED),
    }
}

/// `text` cut to `max` characters (an ellipsis marks a cut).
pub fn clip(text: &str, max: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let mut s: String = t.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// The last [`EXCERPT_LINES`] lines of the item's excerpt as `(role, clipped text)`.
pub fn excerpt(item: &WorkerItem) -> Vec<(String, String)> {
    let n = item.excerpt.len();
    item.excerpt[n.saturating_sub(EXCERPT_LINES)..]
        .iter()
        .map(|l| (l.role.clone(), clip(&l.text, EXCERPT_CHARS)))
        .collect()
}

/// The card's title line.
pub fn title(item: &WorkerItem) -> String {
    format!("Handover {}: {}", item.id, clip(&item.subject, 120))
}

/// `(label, value)` facts every card shows.
pub fn facts(item: &WorkerItem) -> Vec<(&'static str, String)> {
    vec![
        ("Item", item.id.clone()),
        ("Customer", clip(&item.customer, 120)),
        ("Channel", item.channel.clone()),
        ("Reason", item.reason.clone()),
        ("Priority", item.priority.clone()),
    ]
}

/// A plausible email (`x@y`), trimmed; anything else is no email.
pub fn email_like(s: &str) -> Option<String> {
    let s = s.trim();
    let (local, domain) = s.split_once('@')?;
    (!local.is_empty() && domain.contains('.') && !s.contains(char::is_whitespace))
        .then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::core::ExcerptLine;

    fn item(n: usize) -> WorkerItem {
        WorkerItem {
            id: "HO-7".into(),
            subject: "Refund".into(),
            customer: "Ada".into(),
            channel: "web_chat".into(),
            reason: "customer_request".into(),
            priority: "high".into(),
            excerpt: (0..n)
                .map(|i| ExcerptLine {
                    role: "customer".into(),
                    text: format!("line {i}"),
                })
                .collect(),
            customer_email: None,
        }
    }

    #[test]
    fn excerpt_keeps_the_last_lines_clipped() {
        let e = excerpt(&item(8));
        assert_eq!(e.len(), EXCERPT_LINES);
        assert_eq!(e[0].1, "line 3");
        assert_eq!(clip(&"x".repeat(10), 5), "xxxx…");
        assert_eq!(title(&item(1)), "Handover HO-7: Refund");
    }

    #[test]
    fn confirmations_are_fixed_and_unknown_threads_silent() {
        let ok = Ok(StaffResp {
            status: StaffStatus::Delivered,
            item_id: "HO-1".into(),
        });
        assert_eq!(confirmation(&ok), Some(SENT));
        assert_eq!(
            confirmation(&Err(CoreError::Code("thread_not_found".into()))),
            None
        );
        assert_eq!(
            confirmation(&Err(CoreError::Unavailable("connect".into()))),
            Some(FAILED)
        );
        // SW-5: a colleague owns the conversation, or the thread's item is closed
        for c in ["owner_changed", "not_owner"] {
            assert_eq!(
                confirmation(&Err(CoreError::Code(c.into()))),
                Some(OWNED_ELSEWHERE)
            );
        }
        assert_eq!(
            confirmation(&Err(CoreError::Code("closed".into()))),
            Some(THREAD_CLOSED)
        );
    }

    #[test]
    fn email_shape() {
        assert_eq!(email_like(" a@b.co ").as_deref(), Some("a@b.co"));
        assert_eq!(email_like("live.com#a"), None);
        assert_eq!(email_like("a@b"), None);
    }
}
