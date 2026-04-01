// Copyright 2026 BelowZero Security OU. All rights reserved.

use std::collections::BTreeSet;
use tessera_graph_auth::lbac::{SecurityLabel, SecurityPolicy};

#[test]
fn property_key_constants_have_reserved_prefix() {
    assert!(SecurityPolicy::LEVEL_KEY.starts_with("__security"));
    assert!(SecurityPolicy::COMPARTMENTS_KEY.starts_with("__security"));
}

#[test]
fn inject_label_into_empty_properties() {
    let mut props = std::collections::HashMap::new();
    let comps: BTreeSet<String> = ["FINANCE", "HR"].iter().map(|s| (*s).to_string()).collect();
    let label = SecurityLabel::new(3, comps);
    SecurityPolicy::inject_label(&mut props, &label);
    assert!(props.contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(props.contains_key(SecurityPolicy::COMPARTMENTS_KEY));
}

#[test]
fn inject_then_extract_roundtrips_label() {
    let mut props = std::collections::HashMap::new();
    let comps: BTreeSet<String> = ["FINANCE", "HR"].iter().map(|s| (*s).to_string()).collect();
    let original = SecurityLabel::new(3, comps);
    SecurityPolicy::inject_label(&mut props, &original);
    let extracted = SecurityPolicy::extract_label(&props);
    assert_eq!(extracted.level, original.level);
    assert_eq!(extracted.compartments, original.compartments);
}

#[test]
fn extract_label_from_empty_properties_returns_default() {
    let props = std::collections::HashMap::new();
    let label = SecurityPolicy::extract_label(&props);
    assert_eq!(label.level, 0);
    assert!(label.compartments.is_empty());
}

#[test]
fn extract_label_level_zero_empty_compartments_string() {
    use tessera_graph::Property;
    let mut props = std::collections::HashMap::new();
    props.insert(SecurityPolicy::LEVEL_KEY.to_string(), Property::I64(0));
    props.insert(
        SecurityPolicy::COMPARTMENTS_KEY.to_string(),
        Property::String(String::new()),
    );
    let label = SecurityPolicy::extract_label(&props);
    assert_eq!(label.level, 0);
    assert!(label.compartments.is_empty());
}

#[test]
fn compartments_encode_sorted_comma_separated() {
    let mut props = std::collections::HashMap::new();
    let comps: BTreeSet<String> = ["LEGAL", "FINANCE", "HR"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let label = SecurityLabel::new(1, comps);
    SecurityPolicy::inject_label(&mut props, &label);
    let encoded = props
        .get(SecurityPolicy::COMPARTMENTS_KEY)
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(encoded, "FINANCE,HR,LEGAL");
}

#[test]
fn strip_security_properties_removes_reserved_keys() {
    use tessera_graph::Property;
    let mut props = std::collections::HashMap::new();
    props.insert("name".to_string(), Property::String("Alice".to_string()));
    props.insert(SecurityPolicy::LEVEL_KEY.to_string(), Property::I64(2));
    props.insert(
        SecurityPolicy::COMPARTMENTS_KEY.to_string(),
        Property::String("FINANCE".to_string()),
    );
    SecurityPolicy::strip_security_properties(&mut props);
    assert!(!props.contains_key(SecurityPolicy::LEVEL_KEY));
    assert!(!props.contains_key(SecurityPolicy::COMPARTMENTS_KEY));
    assert!(props.contains_key("name"));
}

#[test]
fn is_security_property_detects_reserved_keys() {
    assert!(SecurityPolicy::is_security_property(
        SecurityPolicy::LEVEL_KEY
    ));
    assert!(SecurityPolicy::is_security_property(
        SecurityPolicy::COMPARTMENTS_KEY
    ));
    assert!(!SecurityPolicy::is_security_property("name"));
    assert!(!SecurityPolicy::is_security_property("level"));
}
