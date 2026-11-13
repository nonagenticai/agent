//! The id-shape guard (owner ruling 2026-10-26, "I KNOW MY CUSTOMER THAT I SERVE"; owner decision 2026-10-27:
//! log-only). A customer-facing reply must hold no record id. [`id_kinds`] recognises id-shaped text; [`observe`]
//! is the runtime check: it logs and counts a hit and returns, so a reply is NEVER blocked, altered or handed over
//! because of it. Neither the log line nor the metric holds the matched text or the reply text (it may be PII).
//!
//! The shapes:
//! - [`IdKind::DemoCode`]: the demo estate's prefixed ids (`RET-01`, `ORD-LATE-01`, `O-12`, `SHP-9102`, `CS-3`,
//!   `PAY-1`, `RMA-7`; prefixes RET, ORD, O, SHP, CS, PAY, RMA, REF, DP, WAR, C, DEL, TRK, then a dash and a digit or
//!   capital) and generic codes (2-5 capitals starting a word, an optional dash, >= 2 digits, ending the word:
//!   `AB12345`, `ORD-1234`; not `WH-1000XM6`);
//! - [`IdKind::Uuid`]: a hyphenated UUID;
//! - [`IdKind::LongDigits`]: a run of >= 6 digits that is not money (`38.00`, `1234567.50`);
//! - [`IdKind::OpaqueToken`]: any other >= 24-character token of letters, digits and `-_+/=` holding both a letter
//!   and a digit.
//!
//! Not dates (`2026-10-21`, `2026-10-21T15:00:00Z`), times, money, product or carrier names, "21 October".
use uuid::Uuid;

/// What an id-shaped piece of text looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdKind {
    DemoCode,
    Uuid,
    LongDigits,
    OpaqueToken,
}

impl IdKind {
    /// The metric label / log value: a fixed, low-cardinality set.
    pub fn as_str(self) -> &'static str {
        match self {
            IdKind::DemoCode => "demo_code",
            IdKind::Uuid => "uuid",
            IdKind::LongDigits => "long_digits",
            IdKind::OpaqueToken => "opaque_token",
        }
    }
}

/// The counter of replies found id-shaped, labelled `tenant` and `kind` (one increment per reply and kind).
pub const METRIC: &str = "agent_reply_id_shaped_total";

/// How many id-shaped pieces of each kind `text` holds, kinds in [`IdKind`] order, absent kinds left out.
pub fn id_hits(text: &str) -> Vec<(IdKind, usize)> {
    let (uuids, opaque) = tokens(text);
    [
        (IdKind::DemoCode, demo_codes(text)),
        (IdKind::Uuid, uuids),
        (IdKind::LongDigits, long_digits(&without_uuids(text))),
        (IdKind::OpaqueToken, opaque),
    ]
    .into_iter()
    .filter(|(_, n)| *n > 0)
    .collect()
}

/// The kinds of id-shaped text `text` holds (empty: none).
pub fn id_kinds(text: &str) -> Vec<IdKind> {
    id_hits(text).into_iter().map(|(k, _)| k).collect()
}

/// Does `text` hold anything id-shaped?
pub fn has_id_shape(text: &str) -> bool {
    !id_hits(text).is_empty()
}

/// Where a checked reply goes, for the log line. All ids, none of the customer's text.
pub struct ReplyOrigin<'a> {
    pub tenant: Uuid,
    pub job: &'a str,
    pub conversation: &'a str,
    pub case: Option<&'a str>,
    pub channel: &'a str,
}

/// The runtime check, once per customer-facing reply: on a hit one `reply_id_shaped` warning (tenant, job,
/// conversation, case, channel, kinds and their counts) and one [`METRIC`] increment per kind. Returns the hits;
/// `text` is only read, so the reply goes out byte-identical either way.
pub fn observe(origin: &ReplyOrigin<'_>, text: &str) -> Vec<(IdKind, usize)> {
    let hits = id_hits(text);
    if hits.is_empty() {
        return hits;
    }
    let tenant = origin.tenant.to_string();
    for (kind, _) in &hits {
        metrics::counter!(METRIC, "tenant" => tenant.clone(), "kind" => kind.as_str()).increment(1);
    }
    let summary = hits
        .iter()
        .map(|(k, n)| format!("{}={n}", k.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    tracing::warn!(
        tenant = %origin.tenant,
        job = origin.job,
        conversation = origin.conversation,
        case = origin.case.unwrap_or(""),
        channel = origin.channel,
        kinds = summary,
        "reply_id_shaped"
    );
    hits
}

/// Word starts that open a demo or generic code. A word is counted once even when both rules match it.
fn demo_codes(text: &str) -> usize {
    const PREFIXES: [&str; 13] = [
        "RET", "ORD", "O", "SHP", "CS", "PAY", "RMA", "REF", "DP", "WAR", "C", "DEL", "TRK",
    ];
    let chars: Vec<char> = text.chars().collect();
    let mut n = 0;
    for (i, c) in chars.iter().enumerate() {
        // a code starts a word; `-` and `_` join a code's parts (`ORD-LATE-01` is one code, not two)
        if !c.is_ascii_uppercase()
            || (i > 0 && (chars[i - 1].is_alphanumeric() || matches!(chars[i - 1], '-' | '_')))
        {
            continue;
        }
        let mut j = i;
        while j < chars.len() && chars[j].is_ascii_uppercase() {
            j += 1;
        }
        let run: String = chars[i..j].iter().collect();
        let prefixed = PREFIXES.contains(&run.as_str())
            && chars.get(j) == Some(&'-')
            && chars
                .get(j + 1)
                .is_some_and(|n| n.is_ascii_digit() || n.is_ascii_uppercase());
        let generic = (2..=5).contains(&(j - i)) && {
            let mut k = j;
            if chars.get(k) == Some(&'-') {
                k += 1;
            }
            let d0 = k;
            while k < chars.len() && chars[k].is_ascii_digit() {
                k += 1;
            }
            k - d0 >= 2 && !chars.get(k).is_some_and(|n| n.is_alphanumeric())
        };
        if prefixed || generic {
            n += 1;
        }
    }
    n
}

/// A hyphenated UUID is one id (its digit groups are not a second, long-digits one): `text` without those tokens.
fn without_uuids(text: &str) -> String {
    text.split_whitespace()
        .filter(|w| !is_uuid(w.trim_matches(|c: char| !c.is_ascii_alphanumeric())))
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_uuid(t: &str) -> bool {
    t.len() == 36 && t.contains('-') && Uuid::parse_str(t).is_ok()
}

fn long_digits(text: &str) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut n = 0;
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        let digit_at = |j: Option<usize>| {
            j.and_then(|j| chars.get(j))
                .is_some_and(char::is_ascii_digit)
        };
        let decimal_after = matches!(chars.get(i), Some('.' | ',')) && digit_at(Some(i + 1));
        let decimal_before =
            start >= 2 && matches!(chars[start - 1], '.' | ',') && digit_at(Some(start - 2));
        if i - start >= 6 && !decimal_after && !decimal_before {
            n += 1;
        }
    }
    n
}

/// (hyphenated UUID tokens, other opaque tokens).
fn tokens(text: &str) -> (usize, usize) {
    let (mut uuids, mut opaque) = (0, 0);
    for w in text.split_whitespace() {
        let t = w.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if is_uuid(t) {
            uuids += 1;
        } else if t.len() >= 24
            && t.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '/' | '='))
            && t.chars().any(|c| c.is_ascii_digit())
            && t.chars().any(|c| c.is_ascii_alphabetic())
        {
            opaque += 1;
        }
    }
    (uuids, opaque)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use metrics::{
        Counter, CounterFn, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit,
    };

    use super::*;

    #[test]
    fn detects_each_kind_with_its_count() {
        assert_eq!(id_kinds("your return RET-01"), vec![IdKind::DemoCode]);
        assert_eq!(
            id_hits("RET-01 and ORD-LATE-01, also AB12345"),
            vec![(IdKind::DemoCode, 3)]
        );
        assert_eq!(
            id_kinds("id 123e4567-e89b-12d3-a456-426614174000 here"),
            vec![IdKind::Uuid]
        );
        assert_eq!(id_kinds("number 1234567"), vec![IdKind::LongDigits]);
        assert_eq!(
            id_kinds("token dGhpc2lzYW5vcGFxdWV0b2tlbjEyMw=="),
            vec![IdKind::OpaqueToken]
        );
        assert_eq!(
            id_kinds("RET-01 and 98765432"),
            vec![IdKind::DemoCode, IdKind::LongDigits]
        );
    }

    #[test]
    fn spares_dates_money_names_and_plain_sentences() {
        for ok in [
            "Your Stoneware mug set (4) is in transit with PostNL, expected by 21 October.",
            "Delivered 2026-10-21 at 15:00, or 2026-10-21T15:00:00Z.",
            "We refunded €24,95 on 21 October.",
            "The refund of 38.00 EUR, 1234567.50 in total, 1,234,567 units.",
            "Sony WH-1000XM6 headphones with DHL and UPS",
            "Linen throw and Wool rug 160x230",
            "Our team is closed right now; a colleague will reply after 08:00.",
            "state-of-the-art-international-shipping-service",
            "",
        ] {
            assert!(id_kinds(ok).is_empty(), "{ok}");
            assert!(!has_id_shape(ok), "{ok}");
        }
    }

    #[derive(Default)]
    struct Count(Mutex<u64>);
    impl CounterFn for Count {
        fn increment(&self, v: u64) {
            *self.0.lock().unwrap() += v;
        }
        fn absolute(&self, v: u64) {
            *self.0.lock().unwrap() = v;
        }
    }

    /// Records counters by (name, sorted labels); everything else is ignored.
    #[derive(Default)]
    struct Capture(Mutex<BTreeMap<String, Arc<Count>>>);
    impl Recorder for Capture {
        fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
        fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
        fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
        fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
            let mut labels: Vec<String> = key
                .labels()
                .map(|l| format!("{}={}", l.key(), l.value()))
                .collect();
            labels.sort();
            let id = format!("{}{{{}}}", key.name(), labels.join(","));
            let c = self.0.lock().unwrap().entry(id).or_default().clone();
            Counter::from_arc(c)
        }
        fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
            Gauge::noop()
        }
        fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
            Histogram::noop()
        }
    }
    impl Capture {
        fn counts(&self) -> BTreeMap<String, u64> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), *v.0.lock().unwrap()))
                .collect()
        }
    }

    fn origin(tenant: Uuid) -> ReplyOrigin<'static> {
        ReplyOrigin {
            tenant,
            job: "support",
            conversation: "conv-1",
            case: Some("case-1"),
            channel: "webchat",
        }
    }

    #[test]
    fn a_reply_with_an_id_is_counted_by_tenant_and_kind_and_left_unchanged() {
        let tenant = Uuid::from_u128(7);
        let reply = String::from("Your return RET-01 is on its way.");
        let before = reply.clone();
        let cap = Capture::default();
        let hits = metrics::with_local_recorder(&cap, || observe(&origin(tenant), &reply));
        assert_eq!(hits, vec![(IdKind::DemoCode, 1)]);
        assert_eq!(reply, before);
        let want = format!("{METRIC}{{kind=demo_code,tenant={tenant}}}");
        assert_eq!(cap.counts(), BTreeMap::from([(want, 1)]));
    }

    #[test]
    fn a_clean_reply_records_nothing() {
        let cap = Capture::default();
        let hits = metrics::with_local_recorder(&cap, || {
            observe(
                &origin(Uuid::from_u128(7)),
                "Your mug set is with PostNL, expected on 21 October. The refund is €24,95.",
            )
        });
        assert!(hits.is_empty());
        assert!(cap.counts().is_empty());
    }
}
