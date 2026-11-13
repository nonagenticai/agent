//! CSAT (F-13, B4 T16b; lifecycle SW-6, 0208). At most ONE request per conversation, by whoever resolved it, only
//! where the conversation's binding allows it (`settings.csat_enabled`, default on) and only on a channel a person
//! reads ([`channel_asks`]: never `api`, `mcp`, `voice` or `staff`):
//! - the agent resolved the case ([`arm`]): nothing is sent; the row waits (`awaiting_confirmation`) for the
//!   customer's next message. An acknowledgement ([`acknowledges`]: thanks, bedankt, top, 👍, …) is answered with the
//!   prompt ([`confirm`]); anything else dismisses the request and runs as a normal turn; silence never asks. No
//!   request is armed after a complaint or a customer-asked handover ([`suppressed`]), nor on a reply that offers a
//!   colleague or asks a question ([`askable`]);
//! - staff resolved it ([`ask_staff`]): the prompt is sent with the Resolve, when the Resolve asks for it (its default
//!   is [`staff_default`]: on, except after a complaint).
//!
//! The answer is a rating ([`lone_rating`]: `4`, `4/5`, `4 stars`, `⭐⭐⭐⭐`) on the customer's channel or through
//! `submitCsat`; any other message while it is asked dismisses it, so a later digit is never scored. A rating of 1 or 2
//! asks one follow-up ([`FOLLOW_UP`]) and keeps the next message as the comment (≤ [`COMMENT_MAX`] characters). A request
//! expires 24 h after it was armed or asked. Comments are customer text: never in a log, an audit detail or a metric
//! label. Dismissals are counted by reason (`agent_csat_dismissed_total`), suppressions too
//! (`agent_csat_suppressed_total`); neither is stored.
use sqlx::{PgConnection, Row};

use crate::error::{ApiError, ApiResult};

/// What the customer is asked.
pub const PROMPT: &str = "How did we do? Reply 1–5";
/// The `author` of the stored CSAT prompt and follow-up lines (`agent` lines): a rating counts only right after the
/// prompt, a comment only right after the follow-up.
pub const PROMPT_AUTHOR: &str = "csat";
/// What a recorded rating is answered with.
pub const THANKS: &str = "Thank you for your rating!";
/// What a rating of 1 or 2 is answered with: one question, its answer is the comment.
pub const FOLLOW_UP: &str = "Sorry to hear that. What could we have done better?";
/// What a recorded comment is answered with.
pub const COMMENT_THANKS: &str = "Thank you, we'll pass that on.";
/// The longest comment kept (characters); a longer one is cut.
pub const COMMENT_MAX: usize = 1000;

/// Why an open request was dismissed (the `reason` label of `agent_csat_dismissed_total`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dismissal {
    /// The customer wrote something else after the agent's resolution (no acknowledgement).
    NewQuestion,
    /// The customer answered the prompt with something that is not a rating.
    NonNumeric,
    /// The request expired before the customer answered (counted when a later message finds it).
    SilenceExpired,
}

impl Dismissal {
    pub fn label(self) -> &'static str {
        match self {
            Self::NewQuestion => "new_question",
            Self::NonNumeric => "non_numeric",
            Self::SilenceExpired => "silence_expired",
        }
    }
}

/// Whether CSAT is ever asked on conversation channel `channel`: a channel a person reads and answers.
pub fn channel_asks(channel: &str) -> bool {
    !matches!(channel, "api" | "mcp" | "voice" | "staff")
}

/// Why a request must not be armed by the agent on `conversation_id`: `complaint` (a `case.create{complaint}`
/// handover) or `customer_asked` (the customer asked for a person); `None` when nothing suppresses it.
pub async fn suppressed(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<&'static str>> {
    let row = sqlx::query(
        "SELECT bool_or(trigger = 'case.create{complaint}') AS complaint, \
                bool_or(reason = 'customer_asked') AS asked \
           FROM agent_handover_items WHERE conversation_id = $1",
    )
    .bind(conversation_id)
    .fetch_one(conn)
    .await?;
    Ok(if row.get::<Option<bool>, _>("complaint") == Some(true) {
        Some("complaint")
    } else if row.get::<Option<bool>, _>("asked") == Some(true) {
        Some("customer_asked")
    } else {
        None
    })
}

/// Count a suppressed request by `reason`.
pub fn count_suppressed(reason: &'static str) {
    metrics::counter!("agent_csat_suppressed_total", "reason" => reason).increment(1);
}

/// Whether the staff Resolve of `conversation_id` asks for a rating when the Resolve does not say: yes, except after a
/// complaint. A customer who asked for a person is asked (staff's own service is what the rating measures); only the
/// agent's request is suppressed then ([`suppressed`]).
pub async fn staff_default(conn: &mut PgConnection, conversation_id: &str) -> ApiResult<bool> {
    Ok(suppressed(conn, conversation_id).await? != Some("complaint"))
}

/// Whether `conversation_id` has had a request already (any state): its one request is used.
pub async fn requested_before(conn: &mut PgConnection, conversation_id: &str) -> ApiResult<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM case_csat WHERE conversation_id = $1)")
            .bind(conversation_id)
            .fetch_one(conn)
            .await?,
    )
}

/// The agent resolved `case_id`: arm the conversation's one request, unsent (`awaiting_confirmation`). `false` when
/// the conversation has a request already. Callers hold the conversation's control lock (`ownership::guard`).
pub async fn arm(conn: &mut PgConnection, case_id: &str) -> ApiResult<bool> {
    insert_once(conn, case_id, "awaiting_confirmation", "agent", None).await
}

/// Staff member `sub` resolved `case_id` and asks for a rating: the conversation's one request, `asked` now. `false`
/// when the conversation has a request already. Callers hold the conversation's control lock.
pub async fn ask_staff(conn: &mut PgConnection, case_id: &str, sub: &str) -> ApiResult<bool> {
    insert_once(conn, case_id, "asked", "staff", Some(sub)).await
}

async fn insert_once(
    conn: &mut PgConnection,
    case_id: &str,
    state: &'static str,
    asked_by: &'static str,
    sub: Option<&str>,
) -> ApiResult<bool> {
    let row: Option<String> = sqlx::query_scalar(
        "INSERT INTO case_csat \
            (tenant_id, job_id, case_id, conversation_id, channel, env, release_id, draft_rev, \
             requested_at, expires_at, state, asked_by, asked_by_sub, asked_at) \
         SELECT tenant_id, job_id, case_id, conversation_id, channel, env, release_id, draft_rev, \
                now(), now() + interval '24 hours', $2, $3, $4, \
                CASE WHEN $2 = 'asked' THEN now() END \
           FROM agent_case WHERE case_id = $1 \
            AND NOT EXISTS (SELECT 1 FROM case_csat c WHERE c.conversation_id = agent_case.conversation_id) \
         ON CONFLICT DO NOTHING RETURNING case_id",
    )
    .bind(case_id)
    .bind(state)
    .bind(asked_by)
    .bind(sub)
    .fetch_optional(conn)
    .await?;
    Ok(row.is_some())
}

/// The conversation's request waiting for the customer's acknowledgement: its case, and whether it has expired.
pub async fn awaiting(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<(String, bool)>> {
    let row = sqlx::query(
        "SELECT case_id, expires_at <= now() AS expired FROM case_csat \
          WHERE conversation_id = $1 AND state = 'awaiting_confirmation' \
          ORDER BY requested_at DESC LIMIT 1 FOR UPDATE",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| (r.get("case_id"), r.get("expired"))))
}

/// The customer acknowledged the agent's resolution: request `case_id` is `asked` now (and runs 24 h from now).
pub async fn confirm(conn: &mut PgConnection, case_id: &str) -> ApiResult<()> {
    sqlx::query(
        "UPDATE case_csat SET state = 'asked', asked_at = now(), expires_at = now() + interval '24 hours' \
          WHERE case_id = $1 AND state = 'awaiting_confirmation'",
    )
    .bind(case_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Dismiss the open request `case_id` (`awaiting_confirmation` or `asked`). `true` when it was open; the caller counts
/// `why` with [`count_dismissed`] once its transaction committed (the reason is never stored).
pub async fn dismiss(conn: &mut PgConnection, case_id: &str) -> ApiResult<bool> {
    let n = sqlx::query(
        "UPDATE case_csat SET state = 'dismissed', dismissed_at = now() \
          WHERE case_id = $1 AND state IN ('awaiting_confirmation', 'asked')",
    )
    .bind(case_id)
    .execute(conn)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// Count a committed dismissal by its reason.
pub fn count_dismissed(why: Dismissal) {
    metrics::counter!("agent_csat_dismissed_total", "reason" => why.label()).increment(1);
}

/// A short acknowledgement of the agent's resolution, in English or Dutch (NL is the primary market): thanks, great,
/// perfect, bedankt, top, prima, opgelost, 👍 … — at most six words, no question, no negation. A bare yes/no is not
/// one (it answers something else).
pub fn acknowledges(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    if t.is_empty() || t.contains('?') {
        return false;
    }
    let words: Vec<&str> = t
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | '!' | ';' | ':'))
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() || words.len() > 6 {
        return false;
    }
    const NEGATIONS: [&str; 7] = ["not", "no", "don't", "didn't", "niet", "geen", "nee"];
    if words.iter().any(|w| NEGATIONS.contains(w)) {
        return false;
    }
    const EMOJI: [&str; 6] = ["👍", "🙏", "😊", "🙂", "👌", "❤️"];
    if words.iter().all(|w| {
        EMOJI
            .iter()
            .fold(w.to_string(), |rest, e| rest.replace(e, ""))
            .is_empty()
    }) {
        return true;
    }
    const WORDS: [&str; 21] = [
        "thanks",
        "thank",
        "thx",
        "ty",
        "cheers",
        "great",
        "perfect",
        "sorted",
        "awesome",
        "excellent",
        "bedankt",
        "dank",
        "dankjewel",
        "dankuwel",
        "top",
        "prima",
        "super",
        "opgelost",
        "fijn",
        "mooi",
        "helder",
    ];
    const PHRASES: [&str; 6] = [
        "that's all",
        "thats all",
        "all good",
        "helemaal goed",
        "is goed",
        "dat was het",
    ];
    words.iter().any(|w| WORDS.contains(w)) || PHRASES.iter().any(|p| t.contains(p))
}

/// The case of the conversation's open (asked, unanswered, unexpired) request.
pub async fn pending(conn: &mut PgConnection, conversation_id: &str) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT case_id FROM case_csat \
          WHERE conversation_id = $1 AND state = 'asked' AND expires_at > now() \
          ORDER BY requested_at DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?)
}

/// The conversation's `asked` request whose window has closed (silence): its case, to dismiss.
pub async fn expired_asked(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT case_id FROM case_csat \
          WHERE conversation_id = $1 AND state = 'asked' AND expires_at <= now() \
          ORDER BY requested_at DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?)
}

/// Whether the binding the conversation arrived on allows CSAT: its `settings.csat_enabled`, default `true` (also
/// when the conversation has no binding).
pub async fn enabled(conn: &mut PgConnection, conversation_id: &str) -> ApiResult<bool> {
    let Some((_, Some(binding))) = super::store::customer_ref(&mut *conn, conversation_id).await?
    else {
        return Ok(true);
    };
    let settings: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT settings FROM connector_bindings WHERE id = $1")
            .bind(binding)
            .fetch_optional(conn)
            .await?;
    Ok(settings
        .and_then(|s| s.get("csat_enabled").and_then(serde_json::Value::as_bool))
        .unwrap_or(true))
}

/// Whether a resolved reply may arm a request: not when it offers a colleague (a handover is pending or offered) and
/// not when it ends with a question (the customer is being asked something else, and "yes" would answer that).
pub fn askable(reply: &str) -> bool {
    let t = reply.trim_end();
    !t.to_lowercase().contains("colleague") && !t.ends_with('?')
}

/// A message that is only a rating, 1–5: `4`, `4.` / `4!`, `4/5`, `4 stars` / `4 sterren`, or one to five ⭐.
pub fn lone_rating(text: &str) -> Option<u8> {
    let t = text.trim();
    let stars = t.chars().filter(|c| *c == '⭐').count();
    if stars > 0
        && t.chars()
            .all(|c| c == '⭐' || c.is_whitespace() || c == '\u{fe0f}')
    {
        return u8::try_from(stars).ok().filter(|n| (1..=5).contains(n));
    }
    let t = t.to_lowercase();
    let t = t.trim_end_matches(['.', '!']).trim();
    let t = ["/5", " stars", " star", " sterren", " ster"]
        .iter()
        .find_map(|s| t.strip_suffix(s))
        .unwrap_or(t)
        .trim();
    match t {
        "1" | "2" | "3" | "4" | "5" => t.parse().ok(),
        _ => None,
    }
}

/// 422 `invalid_rating` unless `rating` is 1 to 5.
pub fn check_rating(rating: i64) -> ApiResult<u8> {
    u8::try_from(rating)
        .ok()
        .filter(|r| (1..=5).contains(r))
        .ok_or_else(|| {
            ApiError::Unprocessable(
                "invalid_rating",
                "A rating is a whole number from 1 to 5.".into(),
            )
        })
}

/// Record `rating` for the request of `case_id` in `conversation_id`: 404 `csat_not_requested` (none, or not sent:
/// awaiting the customer's acknowledgement, or dismissed), 409 `csat_answered`, 409 `csat_expired`. `true` when the
/// rating is low (1 or 2): the caller asks the one follow-up ([`ask_follow_up`]).
pub async fn record(
    conn: &mut PgConnection,
    conversation_id: &str,
    case_id: &str,
    rating: u8,
) -> ApiResult<bool> {
    let not_requested = || {
        ApiError::Missing(
            "csat_not_requested",
            "No rating was asked for this case.".into(),
        )
    };
    let row = sqlx::query(
        "SELECT state, expires_at <= now() AS expired FROM case_csat \
          WHERE case_id = $1 AND conversation_id = $2 FOR UPDATE",
    )
    .bind(case_id)
    .bind(conversation_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(not_requested)?;
    match row.get::<String, _>("state").as_str() {
        "answered" => {
            return Err(ApiError::Conflict(
                "csat_answered",
                "This case was rated already.".into(),
            ));
        }
        "asked" => {}
        _ => return Err(not_requested()),
    }
    if row.get::<bool, _>("expired") {
        return Err(ApiError::Conflict(
            "csat_expired",
            "The rating window has closed.".into(),
        ));
    }
    sqlx::query(
        "UPDATE case_csat SET rating = $2, answered_at = now(), state = 'answered' WHERE case_id = $1",
    )
    .bind(case_id)
    .bind(i16::from(rating))
    .execute(conn)
    .await?;
    Ok(rating <= 2)
}

/// Ask the one follow-up of low rating `case_id` (its answer is the comment).
pub async fn ask_follow_up(conn: &mut PgConnection, case_id: &str) -> ApiResult<()> {
    sqlx::query(
        "UPDATE case_csat SET comment_asked_at = now() \
          WHERE case_id = $1 AND state = 'answered' AND comment_asked_at IS NULL",
    )
    .bind(case_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// The case whose follow-up the conversation's customer has not answered yet (asked within the last 24 h).
pub async fn awaiting_comment(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT case_id FROM case_csat \
          WHERE conversation_id = $1 AND comment_asked_at > now() - interval '24 hours' AND comment IS NULL \
          ORDER BY comment_asked_at DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?)
}

/// Keep `text` (cut to [`COMMENT_MAX`] characters) as the comment of `case_id`'s low rating. `false` when it has one.
pub async fn record_comment(conn: &mut PgConnection, case_id: &str, text: &str) -> ApiResult<bool> {
    let comment: String = text.trim().chars().take(COMMENT_MAX).collect();
    if comment.is_empty() {
        return Ok(false);
    }
    let n = sqlx::query(
        "UPDATE case_csat SET comment = $2, comment_at = now() \
          WHERE case_id = $1 AND comment_asked_at IS NOT NULL AND comment IS NULL",
    )
    .bind(case_id)
    .bind(comment)
    .execute(conn)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// The conversation's latest request that was sent (`asked` or `answered`), as `ChatConversation.csat`.
pub async fn latest(
    conn: &mut PgConnection,
    conversation_id: &str,
) -> ApiResult<serde_json::Value> {
    let row = sqlx::query(
        "SELECT case_id, requested_at, rating, expires_at FROM case_csat \
          WHERE conversation_id = $1 AND state IN ('asked', 'answered') ORDER BY requested_at DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map_or(serde_json::Value::Null, |r| {
        serde_json::json!({
            "case_id": r.get::<String, _>("case_id"),
            "requested_at": super::iso(r.get("requested_at")),
            "rating": r.get::<Option<i16>, _>("rating"),
            "expires_at": super::iso(r.get("expires_at")),
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_that_offer_a_colleague_or_ask_a_question_are_not_rated() {
        assert!(askable("Your return is registered."));
        assert!(!askable(
            "I can't see a shipment for that order yet. If you'd like, I can pass this to a colleague."
        ));
        assert!(!askable("I've passed this to a Colleague."));
        assert!(!askable("Which order do you mean, the rug or the lamp?"));
        assert!(!askable("Shall I go ahead?  \n"));
    }

    #[test]
    fn lone_ratings() {
        for (t, r) in [
            ("4", Some(4)),
            (" 5! ", Some(5)),
            ("1.", Some(1)),
            ("3/5", Some(3)),
            ("4 stars", Some(4)),
            ("1 star", Some(1)),
            ("5 Sterren", Some(5)),
            ("⭐⭐⭐⭐", Some(4)),
            ("⭐ ⭐", Some(2)),
        ] {
            assert_eq!(lone_rating(t), r, "{t}");
        }
        for t in [
            "0",
            "6",
            "45",
            "four",
            "great",
            "",
            "-1",
            "⭐⭐⭐⭐⭐⭐",
            "4 stars please",
            "😡",
        ] {
            assert_eq!(lone_rating(t), None, "{t}");
        }
        assert_eq!(check_rating(0).unwrap_err().code(), "invalid_rating");
        assert_eq!(check_rating(6).unwrap_err().code(), "invalid_rating");
        assert_eq!(check_rating(3).unwrap(), 3);
    }

    #[test]
    fn acknowledgements_in_english_and_dutch() {
        for t in [
            "Thanks!",
            "thank you so much",
            "Perfect, thanks",
            "that's all",
            "Bedankt",
            "dank je wel!",
            "Dank u",
            "top",
            "Prima, helemaal goed",
            "opgelost, super",
            "👍",
            "🙏🙏",
        ] {
            assert!(acknowledges(t), "{t}");
        }
        for t in [
            "",
            "yes",
            "ja",
            "thanks but where is my refund?",
            "not great, thanks",
            "niet opgelost",
            "nee bedankt",
            "And what about my other order",
            "thanks, but the parcel still has not arrived at my address",
        ] {
            assert!(!acknowledges(t), "{t}");
        }
    }

    #[test]
    fn csat_is_asked_only_on_channels_a_person_reads() {
        for c in ["web_chat", "telegram"] {
            assert!(channel_asks(c), "{c}");
        }
        for c in ["api", "mcp", "voice", "staff"] {
            assert!(!channel_asks(c), "{c}");
        }
    }
}
