//! "The customer asks for a person" (B12 §3 flows): a conservative phrase match in English, Dutch and German. Words are
//! compared whole (lowercased, punctuation stripped), so `mensen` is not `mens` and `humane` is not `human`. A match
//! queues a handover item with reason `customer_asked` (the D64 value of the web `HandoverReason`).

/// Single words that, on their own in a message, ask for a person.
const WORDS: &[&str] = &[
    "human",
    "humans",
    "medewerker",
    "mens",
    "mitarbeiter",
    "mitarbeiterin",
    "mensch",
    "menschen",
];

/// Word sequences that ask for a person.
const PHRASES: &[&str] = &[
    "real person",
    "live person",
    "live agent",
    "real agent",
    "human agent",
    "talk to someone",
    "speak to someone",
    "speak with someone",
    "talk to somebody",
    "speak to somebody",
    "talk to a person",
    "speak to a person",
    "speak with a person",
    "customer service agent",
    "echt persoon",
    "echte persoon",
    "iemand spreken",
    "met iemand praten",
    "iemand aan de lijn",
    "echte person",
    "echten person",
    "mit jemandem sprechen",
    "mit jemandem reden",
];

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// Does `text` ask for a person?
pub fn asks_for_person(text: &str) -> bool {
    let ws = words(text);
    if ws.iter().any(|w| WORDS.contains(&w.as_str())) {
        return true;
    }
    PHRASES.iter().any(|p| {
        let pw: Vec<&str> = p.split(' ').collect();
        ws.windows(pw.len())
            .any(|win| win.iter().zip(&pw).all(|(a, b)| a == b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_dutch_and_german_requests_match() {
        for t in [
            "I want to talk to a human!",
            "Can I speak to a real person please?",
            "talk to someone",
            "Ik wil een medewerker spreken",
            "Kan ik met een mens praten?",
            "Ich möchte einen Mitarbeiter sprechen",
            "Bitte einen Menschen.",
            "live agent",
        ] {
            assert!(asks_for_person(t), "{t}");
        }
    }

    #[test]
    fn ordinary_messages_do_not_match() {
        for t in [
            "Where is my order?",
            "Wat kost verzending naar België?",
            "Mensen zeggen dat het snel is",
            "That was humane of you",
            "Wann kommt mein Paket?",
            "The person who delivered it was rude",
            "",
        ] {
            assert!(!asks_for_person(t), "{t}");
        }
    }
}
