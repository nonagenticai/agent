//! Static domain lists for the `domain` intake signal (B1b T3, F-03): free-mail providers (a personal address:
//! `info`) and disposable-mail services (`risk`). Lower-case, sorted, unique (unit-tested), matched on the exact
//! domain of the requester's address. No lookup leaves the process (no MX, no reputation service: not ruled).

/// Free-mail providers: a personal address, not a company one.
pub const FREEMAIL: &[&str] = &[
    "aol.com",
    "gmail.com",
    "gmx.at",
    "gmx.ch",
    "gmx.de",
    "gmx.net",
    "googlemail.com",
    "hotmail.co.uk",
    "hotmail.com",
    "hotmail.de",
    "hotmail.fr",
    "hotmail.nl",
    "icloud.com",
    "kpnmail.nl",
    "laposte.net",
    "libero.it",
    "live.com",
    "live.nl",
    "mail.com",
    "mail.ru",
    "me.com",
    "msn.com",
    "orange.fr",
    "outlook.com",
    "outlook.de",
    "proton.me",
    "protonmail.com",
    "t-online.de",
    "web.de",
    "yahoo.co.uk",
    "yahoo.com",
    "yahoo.de",
    "yahoo.fr",
    "yandex.com",
    "yandex.ru",
    "ziggo.nl",
    "zoho.com",
];

/// Disposable-mail services: an address made to be thrown away.
pub const DISPOSABLE: &[&str] = &[
    "10minutemail.com",
    "dispostable.com",
    "emailondeck.com",
    "fakeinbox.com",
    "getnada.com",
    "guerrillamail.com",
    "guerrillamail.net",
    "maildrop.cc",
    "mailinator.com",
    "mintemail.com",
    "mohmal.com",
    "sharklasers.com",
    "temp-mail.org",
    "tempmail.com",
    "throwawaymail.com",
    "trashmail.com",
    "yopmail.com",
];

/// The domain of `email`, lower-cased (`None` without an `@`).
pub fn domain(email: &str) -> Option<String> {
    email
        .trim()
        .rsplit_once('@')
        .map(|(_, d)| d.to_ascii_lowercase())
        .filter(|d| !d.is_empty())
}

pub fn is_freemail(domain: &str) -> bool {
    FREEMAIL.binary_search(&domain).is_ok()
}

pub fn is_disposable(domain: &str) -> bool {
    DISPOSABLE.binary_search(&domain).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freemail_list_is_sorted_lowercase() {
        for list in [FREEMAIL, DISPOSABLE] {
            assert!(
                list.windows(2).all(|w| w[0] < w[1]),
                "sorted and unique: {list:?}"
            );
            for d in list {
                assert_eq!(*d, d.to_ascii_lowercase(), "{d} is lower-case");
                assert!(d.contains('.') && !d.contains('@'), "{d} is a domain");
            }
        }
        assert!(
            FREEMAIL.iter().all(|d| !is_disposable(d)),
            "no domain is on both lists"
        );
        assert!(is_freemail("gmail.com"));
        assert!(is_disposable("mailinator.com"));
        assert!(!is_freemail("globex.com"));
        assert_eq!(domain(" Ann@GMail.com ").as_deref(), Some("gmail.com"));
        assert_eq!(domain("nope"), None);
    }
}
