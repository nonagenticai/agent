//! Plain-language names staff read (SW T6), the same as the web's `CHANNEL_LABEL` (`conversations/labels.ts`),
//! `displayName` (`support/names.ts`) and `REASON[…].label` (`journey/J21Handover.ts`).
//!
//! A customer is named by the party's name, else the name the channel's profile gave, else a short handle built from
//! the last four characters of the conversation id: `Web visitor · 4f2a` on web chat, `{channel label} user · 3456`
//! on every other channel. A raw channel id (`wc:…`, `tg-…`) is never a name.

/// The label of a channel (`CHANNEL_LABEL`); `None` for a channel outside [`crate::conversations::CHANNELS`].
pub fn channel_label(channel: &str) -> Option<&'static str> {
    Some(match channel {
        "staff" => "Staff test chat",
        "telegram" => "Telegram",
        "web_chat" => "Web chat",
        "voice" => "Voice",
        "api" => "API",
        _ => return None,
    })
}

/// The customer's name as staff read it: `party` (the party's display name), else `profile` (the channel profile's
/// name), else the fallback handle of `channel` and the last four characters of `conversation_id`. Blank names do
/// not count.
pub fn display_name(
    party: Option<&str>,
    profile: Option<&str>,
    channel: &str,
    conversation_id: &str,
) -> String {
    if let Some(n) = [party, profile]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|n| !n.is_empty())
    {
        return n.to_string();
    }
    let chars: Vec<char> = conversation_id.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(4)..].iter().collect();
    if channel == "web_chat" {
        return format!("Web visitor · {tail}");
    }
    match channel_label(channel) {
        Some(label) => format!("{label} user · {tail}"),
        None => format!("{channel} user · {tail}"),
    }
}

/// The plain-language label of a handover reason (`crate::conversations::handover::REASONS`); the reason code
/// itself is shown only inside Details.
pub fn reason_label(reason: &str) -> &'static str {
    match reason {
        "customer_asked" => "Customer asked",
        "identity_failed" => "Identity failed",
        "gate_blocked" => "Gate blocked",
        "tool_failed" => "Tool failed",
        "not_offered" => "Required action not available",
        "success_criteria_unmet" => "Success criteria not met",
        "step_up_required" => "Step-up needed",
        "unknown_outcome" => "Unknown outcome",
        "out_of_scope" => "Out of scope",
        "gate_unavailable" => "Gate service unavailable",
        "staff_decision" => "Staff decision",
        _ => "Handed over",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_and_channel_has_a_label() {
        for r in crate::conversations::handover::REASONS {
            assert_ne!(reason_label(r), "Handed over", "{r}");
        }
        for c in crate::conversations::CHANNELS {
            assert!(channel_label(c).is_some(), "{c}");
        }
    }
}
