//! The `shares` group each mock of a workspace keeps its sessions under
//! (`docs/configuration.md`, `mocks.<name>.shares`): the group a mock names,
//! or by default the registrable domain its hosts have in common with another
//! mock's, so a REST mock and a GraphQL mock of one vendor read each other's
//! records without a setting. The registry works it out here for the runtimes
//! it builds and the Admin API it answers (see [`crate::registry`]), and the CLI
//! and MCP start answers for a config file, from the settings alone.

use std::collections::{BTreeMap, BTreeSet};

use crate::proxy::siblings::registrable_domain;

pub use mb_admin::{SHARES_NONE, SharesGroup};

/// One mock of a workspace as [`shares_groups`] reads it.
#[derive(Clone, Copy, Debug)]
pub struct SharesInput<'a> {
    pub name: &'a str,
    pub hosts: &'a [String],
    /// Its `shares` setting: a group name, [`SHARES_NONE`], or `None` (unset).
    pub shares: Option<&'a str>,
}

/// The group of each mock of one workspace that is in one, by mock name:
///
/// - a mock naming a group (`shares: <name>`) is in it;
/// - `shares: none` ([`SHARES_NONE`]) keeps a mock out of every group;
/// - a mock that sets nothing joins the default group of the mocks it shares a
///   registrable domain with (through its `hosts`, by [`registrable_domain`]):
///   mocks linked so, directly or through another such mock, form one group,
///   named after the alphabetically first domain two of them share. A mock with no
///   hosts, or only IP literals and single-label hosts, joins none, and nor
///   does one whose domains no other such mock has.
#[must_use]
pub fn shares_groups<'a>(
    mocks: impl IntoIterator<Item = SharesInput<'a>>,
) -> BTreeMap<String, SharesGroup> {
    let mut out = BTreeMap::new();
    // The domains of each mock that sets nothing.
    let mut unset: Vec<(&str, BTreeSet<&str>)> = Vec::new();
    for m in mocks {
        match m.shares {
            Some(SHARES_NONE) => {}
            Some(group) => {
                out.insert(
                    m.name.to_owned(),
                    SharesGroup {
                        group: group.to_owned(),
                        default: false,
                    },
                );
            }
            None => {
                let domains: BTreeSet<&str> = m
                    .hosts
                    .iter()
                    .filter_map(|h| registrable_domain(h))
                    .collect();
                if !domains.is_empty() {
                    unset.push((m.name, domains));
                }
            }
        }
    }
    // Linked mocks, by union-find over the mocks that set nothing.
    let mut parent: Vec<usize> = (0..unset.len()).collect();
    let mut first_holder: BTreeMap<&str, usize> = BTreeMap::new();
    // The domains two mocks share.
    let mut shared: BTreeSet<&str> = BTreeSet::new();
    for (i, (_, domains)) in unset.iter().enumerate() {
        for &domain in domains {
            match first_holder.get(domain) {
                Some(&j) => {
                    shared.insert(domain);
                    let (a, b) = (root(&mut parent, i), root(&mut parent, j));
                    parent[a] = b;
                }
                None => {
                    first_holder.insert(domain, i);
                }
            }
        }
    }
    // Each linked set's name: the first shared domain one of its mocks has.
    let mut names: BTreeMap<usize, &str> = BTreeMap::new();
    for &domain in &shared {
        let Some(&holder) = first_holder.get(domain) else {
            continue;
        };
        let r = root(&mut parent, holder);
        names.entry(r).or_insert(domain);
    }
    for (i, (name, _)) in unset.iter().enumerate() {
        let r = root(&mut parent, i);
        if let Some(group) = names.get(&r) {
            out.insert(
                (*name).to_owned(),
                SharesGroup {
                    group: (*group).to_owned(),
                    default: true,
                },
            );
        }
    }
    out
}

/// The root of `i`'s set in the union-find `parent`, halving paths on the way.
fn root(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// The group of mock `name` among `mocks` (its workspace's, itself included):
/// see [`shares_groups`]. `None` when it is in none. The Admin API's
/// [`mb_admin::SharesGroupOf`].
#[must_use]
pub fn shares_group_of(name: &str, mocks: &[mb_store::Mock]) -> Option<SharesGroup> {
    shares_groups(mocks.iter().map(|m| SharesInput {
        name: &m.name,
        hosts: &m.hosts,
        shares: m.behavior.shares.as_deref(),
    }))
    .remove(name)
}

/// The group of each mock of `cfg` that is in one, by mock name: [`shares_groups`]
/// over the mocks of each workspace.
#[must_use]
pub fn config_groups(cfg: &crate::file_config::FileConfig) -> BTreeMap<String, SharesGroup> {
    let mut by_workspace: BTreeMap<&str, Vec<SharesInput<'_>>> = BTreeMap::new();
    for (name, m) in &cfg.mocks {
        by_workspace
            .entry(m.workspace.as_str())
            .or_default()
            .push(SharesInput {
                name,
                hosts: &m.hosts,
                shares: m.shares.as_deref(),
            });
    }
    by_workspace.into_values().flat_map(shares_groups).collect()
}

/// The mocks (by name) that answer for one host without all being in the same
/// `shares` group (each mock's effective group: named, default or none), with
/// the hosts they have in common: each set of mocks once, with all its hosts. Only mocks of one workspace are compared, as a
/// `shares` group holds mocks of one workspace. Host names are compared
/// without case.
pub(crate) fn same_host_without_shares<'a>(
    mocks: impl IntoIterator<Item = (&'a str, &'a str, &'a [String], Option<&'a str>)>,
) -> Vec<(Vec<String>, Vec<String>)> {
    /// A mock's name and `shares` group.
    type Member<'a> = (&'a str, Option<&'a str>);
    let mut by_host: BTreeMap<(&str, String), Vec<Member<'_>>> = BTreeMap::new();
    for (name, workspace, hosts, shares) in mocks {
        let hosts: BTreeSet<String> = hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
        for host in hosts {
            by_host
                .entry((workspace, host))
                .or_default()
                .push((name, shares));
        }
    }
    let mut groups: BTreeMap<Vec<String>, Vec<String>> = BTreeMap::new();
    for ((_, host), mocks) in by_host {
        let Some(&(_, first)) = mocks.first() else {
            continue;
        };
        if mocks.len() < 2 || (first.is_some() && mocks.iter().all(|&(_, s)| s == first)) {
            continue;
        }
        let names: BTreeSet<String> = mocks.iter().map(|&(n, _)| n.to_owned()).collect();
        groups
            .entry(names.into_iter().collect())
            .or_default()
            .push(host);
    }
    groups.into_iter().collect()
}

/// The next step for `mocks` that answer for `hosts` without a common
/// `shares` group.
pub(crate) fn unshared_hosts_step(mocks: &[String], hosts: &[String]) -> String {
    let quote = |names: &[String]| {
        names
            .iter()
            .map(|n| format!("{n:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "Mocks {} answer for the same host ({}) but are in no common shares group: one sets `shares: none`, they name different groups, or the host has no registrable domain (an IP address or a single label such as localhost). If they serve the same API (REST beside GraphQL, for example), give each the same `shares: <group>` in the config (mocks.<name>.shares) so a record created through one is read through the other, and where a type's name differs between their contracts, describe it with the world's types.<type>.same_as (the mockingbird-world skill).",
        quote(mocks),
        quote(hosts)
    )
}

/// What a start answer (CLI `mockingbird server start`, MCP `start_server`)
/// says of the `shares` groups of config `cfg`: `shares`, each mock's group
/// (`{group, default}`, `null` for none), and the `next_steps` naming mocks that
/// answer for one host without a common group.
#[must_use]
pub fn start_notice(cfg: &crate::file_config::FileConfig) -> (serde_json::Value, Vec<String>) {
    let groups = config_groups(cfg);
    let shares: serde_json::Map<String, serde_json::Value> = cfg
        .mocks
        .keys()
        .map(|name| {
            (
                name.clone(),
                groups
                    .get(name)
                    .and_then(|g| serde_json::to_value(g).ok())
                    .unwrap_or(serde_json::Value::Null),
            )
        })
        .collect();
    let steps = same_host_without_shares(cfg.mocks.iter().map(|(name, m)| {
        (
            name.as_str(),
            m.workspace.as_str(),
            m.hosts.as_slice(),
            groups.get(name).map(|g| g.group.as_str()),
        )
    }))
    .into_iter()
    .map(|(mocks, hosts)| unshared_hosts_step(&mocks, &hosts))
    .collect();
    (serde_json::Value::Object(shares), steps)
}

/// Adds [`start_notice`]'s `shares` and `next_steps` for the config at `path` to
/// a start answer; nothing when the config cannot be read (the server says why).
pub fn add_start_notice(answer: &mut serde_json::Value, path: &std::path::Path) {
    let Ok(cfg) = crate::file_config::load(path) else {
        return;
    };
    let (shares, steps) = start_notice(&cfg);
    let Some(obj) = answer.as_object_mut() else {
        return;
    };
    if shares.as_object().is_some_and(|s| !s.is_empty()) {
        obj.insert("shares".into(), shares);
    }
    if !steps.is_empty() {
        let next = obj
            .entry("next_steps")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        if let Some(next) = next.as_array_mut() {
            next.extend(steps.into_iter().map(serde_json::Value::String));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(name: &'a str, hosts: &'a [String], shares: Option<&'a str>) -> SharesInput<'a> {
        SharesInput {
            name,
            hosts,
            shares,
        }
    }

    fn hosts(h: &[&str]) -> Vec<String> {
        h.iter().map(|s| (*s).to_owned()).collect()
    }

    fn default(name: &str) -> SharesGroup {
        SharesGroup {
            group: name.into(),
            default: true,
        }
    }

    #[test]
    fn mocks_of_one_registrable_domain_join_its_default_group() {
        let rest = hosts(&["api.example.test"]);
        let graphql = hosts(&["api.example.test"]);
        let uploads = hosts(&["uploads.example.test"]);
        let groups = shares_groups([
            input("rest", &rest, None),
            input("graphql", &graphql, None),
            input("uploads", &uploads, None),
        ]);
        assert_eq!(groups.len(), 3);
        for m in ["rest", "graphql", "uploads"] {
            assert_eq!(groups[m], default("example.test"), "{m}");
        }
    }

    #[test]
    fn different_domains_ips_single_labels_and_no_hosts_join_none() {
        let a = hosts(&["api.example.test"]);
        let b = hosts(&["api.other.test"]);
        let ip = hosts(&["10.0.0.1"]);
        let ip2 = hosts(&["10.0.0.1"]);
        let local = hosts(&["localhost"]);
        let local2 = hosts(&["localhost"]);
        let suffix = hosts(&["a.github.io"]);
        let suffix2 = hosts(&["b.github.io"]);
        let groups = shares_groups([
            input("a", &a, None),
            input("b", &b, None),
            input("ip", &ip, None),
            input("ip2", &ip2, None),
            input("local", &local, None),
            input("local2", &local2, None),
            input("suffix", &suffix, None),
            input("suffix2", &suffix2, None),
            input("anyhost", &[], None),
            input("anyhost2", &[], None),
        ]);
        assert!(groups.is_empty(), "{groups:?}");
    }

    #[test]
    fn a_named_group_or_none_overrides_the_default() {
        let h = hosts(&["api.example.test"]);
        let groups = shares_groups([
            input("rest", &h, None),
            input("graphql", &h, Some(SHARES_NONE)),
            input("v2", &h, Some("v2")),
            input("v2b", &h, Some("v2")),
        ]);
        // `rest` is left alone with the domain: no group.
        assert_eq!(groups.get("rest"), None);
        assert_eq!(groups.get("graphql"), None);
        assert_eq!(
            groups["v2"],
            SharesGroup {
                group: "v2".into(),
                default: false
            }
        );
        assert_eq!(groups["v2b"].group, "v2");
    }

    #[test]
    fn mocks_linked_through_another_form_one_group_named_by_the_first_domain() {
        let a = hosts(&["api.b-vendor.test"]);
        let ab = hosts(&["api.b-vendor.test", "files.a-vendor.test"]);
        let b = hosts(&["www.a-vendor.test"]);
        let groups = shares_groups([
            input("a", &a, None),
            input("ab", &ab, None),
            input("b", &b, None),
        ]);
        for m in ["a", "ab", "b"] {
            assert_eq!(groups[m], default("a-vendor.test"), "{m}");
        }
    }

    #[test]
    fn same_host_mocks_without_a_common_shares_group_are_named() {
        let hosts = |h: &[&str]| h.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>();
        let (api, both, other) = (
            hosts(&["api.example.com"]),
            hosts(&["API.example.com", "uploads.example.com"]),
            hosts(&["other.example.com"]),
        );
        let found = same_host_without_shares([
            ("rest", "default", api.as_slice(), None),
            ("graph", "default", both.as_slice(), None),
            ("lone", "default", other.as_slice(), None),
        ]);
        assert_eq!(
            found,
            vec![(
                vec!["graph".to_owned(), "rest".to_owned()],
                vec!["api.example.com".to_owned()]
            )]
        );
        // One group in common: nothing to say.
        assert!(
            same_host_without_shares([
                ("rest", "default", api.as_slice(), Some("api")),
                ("graph", "default", both.as_slice(), Some("api")),
            ])
            .is_empty()
        );
        // Different groups, or one without: named.
        for (a, b) in [(Some("a"), Some("b")), (Some("a"), None), (None, Some("a"))] {
            assert_eq!(
                same_host_without_shares([
                    ("rest", "default", api.as_slice(), a),
                    ("graph", "default", api.as_slice(), b)
                ])
                .len(),
                1,
                "{a:?} {b:?}"
            );
        }
        // Mocks of different workspaces cannot share a group: nothing to say.
        assert!(
            same_host_without_shares([
                ("rest", "one", api.as_slice(), None),
                ("graph", "two", api.as_slice(), None),
            ])
            .is_empty()
        );
        // Hosts listed by only one mock, or none, say nothing.
        assert!(
            same_host_without_shares([
                ("rest", "default", api.as_slice(), None),
                ("x", "default", &[][..], None)
            ])
            .is_empty()
        );
    }

    #[test]
    fn the_unshared_hosts_step_names_the_mocks_shares_and_same_as() {
        let step = unshared_hosts_step(
            &["graph".to_owned(), "rest".to_owned()],
            &["api.example.com".to_owned()],
        );
        for part in [
            "\"graph\", \"rest\"",
            "\"api.example.com\"",
            "mocks.<name>.shares",
            "same_as",
            "mockingbird-world",
        ] {
            assert!(step.contains(part), "{part}: {step}");
        }
    }
}
