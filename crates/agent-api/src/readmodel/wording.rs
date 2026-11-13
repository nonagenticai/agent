//! Plain-language wording for attention items (the Rust side of `web/src/lib/ui/labels.ts`): the backend words a
//! title / detail the way people say it, and keeps the raw technical text in the item's `code`.
//! No customer data and no payloads pass through here (K50): only ids, keys and codes become words.

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// A release id (`cfg-2026-10-18.1`) as "18 Oct release" ("18 Oct release (2)" for a second one that day);
/// anything that is not a release id comes back unchanged.
pub fn release_name(id: &str) -> String {
    let parse = || -> Option<String> {
        let rest = id.strip_prefix("cfg-")?;
        let (date, n) = rest.split_once('.')?;
        let mut p = date.split('-');
        let (_y, m, d) = (p.next()?, p.next()?, p.next()?);
        if p.next().is_some() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let (m, d): (usize, u32) = (m.parse().ok()?, d.parse().ok()?);
        let name = format!("{d} {} release", MONTHS.get(m.checked_sub(1)?)?);
        Some(if n == "1" {
            name
        } else {
            format!("{name} ({n})")
        })
    };
    parse().unwrap_or_else(|| id.to_string())
}

/// A snake_case or dotted key as plain words; the known names get their own label.
pub fn human_key(key: &str) -> String {
    let known = match key {
        "strata_ingest.vision" => "Image reading",
        "strata_ingest.ocr" => "Document reading",
        "strata_ingest" => "Document ingest",
        "strata_rag" => "Knowledge search",
        "licence_ensure" => "Licence setup",
        "licence_revoke" => "Licence removal",
        "connections_put" => "Connection setup",
        "store_ensure" => "Knowledge store setup",
        "policy.order_changes" => "Order-change policy",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    let words: String = key
        .split(['.', '_', '-'])
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    capitalise(&words)
}

/// An operation key (`Orders.lookup_order`) as "Orders › lookup order"; other keys through [`human_key`].
pub fn operation_name(op: &str) -> String {
    match op.split_once('.') {
        Some((o, k)) if o.starts_with(|c: char| c.is_ascii_uppercase()) && !k.is_empty() => {
            format!("{o} \u{203a} {}", human_key(k).to_lowercase())
        }
        _ => human_key(op),
    }
}

/// A template layer at a version (`retail`, 2) as "Retail v2"; `job/x` is "X template v2", `base` "Base template v2".
pub fn layer_version(layer: &str, version: &str) -> String {
    let (kind, path) = match layer.split_once('/') {
        Some((k @ ("job" | "industry" | "industry_dept" | "department"), p)) => (Some(k), p),
        _ => (None, layer),
    };
    let name = capitalise(&path.replace(['/', '_'], " "));
    match (kind, path) {
        (Some("job"), _) => format!("{name} template v{version}"),
        (None, "base") => format!("Base template v{version}"),
        _ => format!("{name} v{version}"),
    }
}

/// Free text (a compile error) with the technical tokens worded: release ids, `snake_case` names and `@vN`.
pub fn plain(text: &str) -> String {
    text.split(' ')
        .map(|w| {
            let core = w
                .trim_matches(|c: char| !(c.is_alphanumeric() || "_-.@/".contains(c)))
                .trim_matches('.');
            let rel = release_name(core);
            if rel != core {
                return w.replace(core, &rel);
            }
            if let Some((l, v)) = core.split_once("@v")
                && !l.is_empty()
                && !v.is_empty()
                && v.bytes().all(|b| b.is_ascii_digit())
            {
                return w.replace(core, &layer_version(l, v));
            }
            if core.contains('_') && !core.contains(['/', '@']) {
                return w.replace(core, &human_key(core).to_lowercase());
            }
            w.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_and_layer_names() {
        assert_eq!(release_name("cfg-2026-10-18.1"), "18 Oct release");
        assert_eq!(release_name("cfg-2026-10-18.2"), "18 Oct release (2)");
        assert_eq!(release_name("whatever"), "whatever");
        assert_eq!(layer_version("retail", "2"), "Retail v2");
        assert_eq!(
            layer_version("job/customer_care_agent", "3"),
            "Customer care agent template v3"
        );
        assert_eq!(layer_version("base", "3"), "Base template v3");
    }

    #[test]
    fn keys_and_text() {
        assert_eq!(human_key("strata_rag"), "Knowledge search");
        assert_eq!(human_key("excluded_categories"), "Excluded categories");
        assert_eq!(
            operation_name("Orders.lookup_order"),
            "Orders \u{203a} lookup order"
        );
        assert_eq!(
            plain("slot return_window is missing in retail@v2 for cfg-2026-10-18.1."),
            "slot return window is missing in Retail v2 for 18 Oct release."
        );
    }
}
