// Copyright 2026 BelowZero Security OU. All rights reserved.

//! Maps external group names (LDAP groups, OIDC claims) to internal RBAC role IDs.

use std::collections::{HashMap, HashSet};

use crate::rbac::{RoleId, RoleStore};

/// Map external group names to internal `RoleId`s using a configurable table.
///
/// Groups that have no entry in `mapping` are silently ignored.
/// Duplicate role assignments (two groups mapping to the same role) are deduplicated.
///
/// The role name strings must match the predefined role names in `RoleStore`:
/// `"admin"`, `"readwrite"`, `"readonly"`, `"monitor"`.
#[must_use]
pub fn map_groups<S: std::hash::BuildHasher>(
    external_groups: &[String],
    mapping: &HashMap<String, String, S>,
) -> Vec<RoleId> {
    let mut seen = HashSet::new();
    let mut roles = Vec::new();

    for group in external_groups {
        let Some(role_name) = mapping.get(group) else {
            continue;
        };
        let Some(role_id) = role_name_to_id(role_name) else {
            continue;
        };
        if seen.insert(role_id) {
            roles.push(role_id);
        }
    }

    roles
}

/// Parse a comma-separated `"ldap_group=role_name"` string into a `HashMap`.
///
/// Malformed pairs (no `=`) are silently ignored.
#[must_use]
pub fn parse_group_mapping(raw: &str) -> HashMap<String, String> {
    if raw.is_empty() {
        return HashMap::new();
    }
    raw.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((k.trim().to_owned(), v.trim().to_owned()))
        })
        .collect()
}

/// Convert a role name string to a predefined `RoleId`.
fn role_name_to_id(name: &str) -> Option<RoleId> {
    match name {
        "admin" => Some(RoleStore::ADMIN_ROLE_ID),
        "readwrite" => Some(RoleStore::READWRITE_ROLE_ID),
        "readonly" => Some(RoleStore::READONLY_ROLE_ID),
        "monitor" => Some(RoleStore::MONITOR_ROLE_ID),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_mapping() -> HashMap<String, String> {
        [
            ("admin".to_owned(), "admin".to_owned()),
            ("developers".to_owned(), "readwrite".to_owned()),
            ("viewers".to_owned(), "readonly".to_owned()),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn map_groups_returns_admin_role() {
        let mapping = default_mapping();
        let roles = map_groups(&["admin".to_owned()], &mapping);
        assert_eq!(roles, vec![RoleStore::ADMIN_ROLE_ID]);
    }

    #[test]
    fn map_groups_returns_multiple_roles() {
        let mapping = default_mapping();
        let mut roles = map_groups(&["admin".to_owned(), "viewers".to_owned()], &mapping);
        roles.sort_by_key(|r| r.raw());
        assert_eq!(
            roles,
            vec![RoleStore::ADMIN_ROLE_ID, RoleStore::READONLY_ROLE_ID]
        );
    }

    #[test]
    fn map_groups_unknown_group_is_ignored() {
        let mapping = default_mapping();
        let roles = map_groups(&["unknown_group".to_owned()], &mapping);
        assert!(roles.is_empty());
    }

    #[test]
    fn map_groups_empty_input_returns_empty() {
        let mapping = default_mapping();
        let roles = map_groups(&[], &mapping);
        assert!(roles.is_empty());
    }

    #[test]
    fn map_groups_deduplicates_roles() {
        let mapping: HashMap<String, String> = [
            ("cn=admins".to_owned(), "admin".to_owned()),
            ("cn=superusers".to_owned(), "admin".to_owned()),
        ]
        .into_iter()
        .collect();
        let roles = map_groups(
            &["cn=admins".to_owned(), "cn=superusers".to_owned()],
            &mapping,
        );
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0], RoleStore::ADMIN_ROLE_ID);
    }

    #[test]
    fn map_groups_unknown_role_name_ignored() {
        let mapping: HashMap<String, String> =
            std::iter::once(("group1".to_owned(), "nonexistent_role".to_owned())).collect();
        let roles = map_groups(&["group1".to_owned()], &mapping);
        assert!(roles.is_empty());
    }

    #[test]
    fn parse_group_mapping_valid_string() {
        let raw = "admin=admin,developers=readwrite,viewers=readonly";
        let map = parse_group_mapping(raw);
        assert_eq!(map.get("admin").map(String::as_str), Some("admin"));
        assert_eq!(map.get("developers").map(String::as_str), Some("readwrite"));
        assert_eq!(map.get("viewers").map(String::as_str), Some("readonly"));
    }

    #[test]
    fn parse_group_mapping_ignores_malformed_pairs() {
        let raw = "admin=admin,badentry,developers=readwrite";
        let map = parse_group_mapping(raw);
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("admin"));
        assert!(map.contains_key("developers"));
    }

    #[test]
    fn parse_group_mapping_empty_string_returns_empty() {
        let map = parse_group_mapping("");
        assert!(map.is_empty());
    }

    #[test]
    fn parse_group_mapping_trims_whitespace() {
        let raw = " admin = admin , devs = readwrite ";
        let map = parse_group_mapping(raw);
        assert_eq!(map.get("admin").map(String::as_str), Some("admin"));
        assert_eq!(map.get("devs").map(String::as_str), Some("readwrite"));
    }

    #[test]
    fn external_user_info_fields_roundtrip() {
        use crate::providers::ExternalUserInfo;
        let info = ExternalUserInfo {
            username: "alice".to_owned(),
            groups: vec!["admin".to_owned()],
            email: Some("alice@example.com".to_owned()),
            display_name: Some("Alice".to_owned()),
        };
        assert_eq!(info.username, "alice");
        assert_eq!(info.groups, ["admin"]);
        assert_eq!(info.email.as_deref(), Some("alice@example.com"));
    }
}
