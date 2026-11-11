//! Vendor siblings: unmapped hosts that share a registrable domain
//! (eTLD+1 by the Public Suffix List) with a host a mock claims, such as
//! `verify.twilio.com` when a mock serves `api.twilio.com`. The proxy
//! blocks them by default (`proxy.vendor_subdomains`), so test traffic
//! does not reach the vendor a mock stands in for.

use std::net::IpAddr;

/// The registrable domain of `host` (normalized, see
/// [`normalize_host`](crate::registry::normalize_host)): the public suffix
/// plus one label, by the Public Suffix List and its implicit `*` rule for
/// unlisted top-level domains. `None` for IP literals, single-label hosts
/// and public suffixes themselves (`co.uk`).
pub fn registrable_domain(host: &str) -> Option<&str> {
    if !host.contains('.') || host.parse::<IpAddr>().is_ok() {
        return None;
    }
    let domain = psl::domain(host.as_bytes())?;
    std::str::from_utf8(domain.as_bytes()).ok()
}

/// The hosts in `mapped` that share `host`'s registrable domain, sorted
/// and without `host` itself; empty when `host` has none (an IP literal,
/// a single label) or no mapped host shares it.
pub fn siblings_of<'a>(host: &str, mapped: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let Some(domain) = registrable_domain(host) else {
        return Vec::new();
    };
    let mut hosts: Vec<String> = mapped
        .into_iter()
        .filter(|m| *m != host && registrable_domain(m) == Some(domain))
        .map(str::to_owned)
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

#[cfg(test)]
mod tests {
    use super::{registrable_domain, siblings_of};

    #[test]
    fn subdomains_and_the_apex_of_a_mapped_vendor_are_siblings() {
        let mapped = ["api.twilio.com", "lookups.twilio.com"];
        assert_eq!(
            siblings_of("verify.twilio.com", mapped),
            ["api.twilio.com", "lookups.twilio.com"]
        );
        assert_eq!(
            siblings_of("twilio.com", ["api.twilio.com"]),
            ["api.twilio.com"]
        );
        assert_eq!(
            siblings_of("a.b.twilio.com", ["api.twilio.com"]),
            ["api.twilio.com"]
        );
    }

    #[test]
    fn multi_label_public_suffixes_split_registrants() {
        let mapped = ["api.example.co.uk"];
        assert_eq!(
            siblings_of("other.example.co.uk", mapped),
            ["api.example.co.uk"]
        );
        assert!(siblings_of("other.co.uk", mapped).is_empty());
        assert!(siblings_of("co.uk", mapped).is_empty());
        assert!(siblings_of("bar.co.uk", ["foo.co.uk"]).is_empty());
        // Private suffixes count too: two GitHub Pages sites are unrelated.
        assert!(siblings_of("b.github.io", ["a.github.io"]).is_empty());
    }

    #[test]
    fn unrelated_hosts_ip_literals_and_single_labels_are_not_siblings() {
        let mapped = ["api.twilio.com", "10.0.0.1", "localhost"];
        assert!(siblings_of("api.stripe.com", mapped).is_empty());
        assert!(siblings_of("twilio.co", mapped).is_empty());
        assert!(siblings_of("10.0.0.2", mapped).is_empty());
        assert!(siblings_of("::1", mapped).is_empty());
        assert!(siblings_of("intranet", mapped).is_empty());
        assert!(siblings_of("api.twilio.com", mapped).is_empty());
        assert_eq!(registrable_domain("10.0.0.1"), None);
        assert_eq!(registrable_domain("localhost"), None);
    }

    #[test]
    fn unlisted_top_level_domains_use_the_implicit_rule() {
        assert_eq!(registrable_domain("api.orders.test"), Some("orders.test"));
        assert_eq!(
            siblings_of("verify.orders.test", ["api.orders.test"]),
            ["api.orders.test"]
        );
        assert!(siblings_of("elsewhere.test", ["api.orders.test"]).is_empty());
    }
}
