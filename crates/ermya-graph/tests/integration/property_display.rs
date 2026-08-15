// SPDX-License-Identifier: MIT

use ermya_graph::Property;

#[test]
fn display_string() {
    assert_eq!(format!("{}", Property::String("hello".into())), "hello");
}

#[test]
fn display_i64() {
    assert_eq!(format!("{}", Property::I64(42)), "42");
}

#[test]
fn display_f64() {
    assert_eq!(format!("{}", Property::F64(1.5)), "1.5");
}

#[test]
fn display_bool_true() {
    assert_eq!(format!("{}", Property::Bool(true)), "true");
}

#[test]
fn display_bool_false() {
    assert_eq!(format!("{}", Property::Bool(false)), "false");
}

#[test]
fn display_bytes() {
    assert_eq!(format!("{}", Property::Bytes(vec![1, 2, 3])), "[3 bytes]");
}

#[test]
fn display_empty_bytes() {
    assert_eq!(format!("{}", Property::Bytes(vec![])), "[0 bytes]");
}

#[test]
fn display_empty_string() {
    assert_eq!(format!("{}", Property::String(String::new())), "");
}

#[test]
fn display_negative_i64() {
    assert_eq!(format!("{}", Property::I64(-100)), "-100");
}

#[test]
fn display_f64_infinity() {
    assert_eq!(format!("{}", Property::F64(f64::INFINITY)), "inf");
}
