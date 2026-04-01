// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_graph_import::property_coerce::is_valid_property_key;

#[test]
fn valid_simple_name() {
    assert!(is_valid_property_key("name"));
}

#[test]
fn valid_underscore_prefix() {
    assert!(is_valid_property_key("_private"));
}

#[test]
fn valid_with_digit_suffix() {
    assert!(is_valid_property_key("prop_1"));
}

#[test]
fn invalid_empty_string() {
    assert!(!is_valid_property_key(""));
}

#[test]
fn invalid_starts_with_digit() {
    assert!(!is_valid_property_key("1bad"));
}

#[test]
fn invalid_contains_hyphen() {
    assert!(!is_valid_property_key("has-dash"));
}

#[test]
fn invalid_contains_space() {
    assert!(!is_valid_property_key("has space"));
}

#[test]
fn invalid_contains_null_byte() {
    // The null byte is used as separator in the lookup composite key, so it
    // must always be considered invalid in property names.
    assert!(!is_valid_property_key("has\0null"));
}
