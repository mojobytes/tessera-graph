// Copyright 2026 BelowZero Security OU. All rights reserved.

use tessera_import::error::{ExportError, ImportError};

#[test]
fn import_error_csv_parse_display() {
    let e = ImportError::CsvParse {
        row: 3,
        reason: "unexpected end of line".to_owned(),
    };
    let msg = e.to_string();
    assert!(msg.contains("row 3"), "got: {msg}");
    assert!(msg.contains("unexpected end of line"), "got: {msg}");
}

#[test]
fn import_error_json_invalid_display() {
    let e = ImportError::JsonInvalid("bad json".to_owned());
    let msg = e.to_string();
    assert!(msg.contains("bad json"), "got: {msg}");
}

#[test]
fn import_error_json_missing_field_display() {
    let e = ImportError::JsonMissingField("nodes".to_owned());
    let msg = e.to_string();
    assert!(msg.contains("nodes"), "got: {msg}");
}

#[test]
fn import_error_node_not_found_display() {
    let e = ImportError::NodeNotFoundForEdge {
        label: "Person".to_owned(),
        prop: "name".to_owned(),
        value: "Alice".to_owned(),
    };
    let msg = e.to_string();
    assert!(msg.contains("Person"), "got: {msg}");
    assert!(msg.contains("name"), "got: {msg}");
    assert!(msg.contains("Alice"), "got: {msg}");
}

#[test]
fn import_error_gql_statement_display() {
    let e = ImportError::GqlStatement {
        line: 10,
        reason: "unexpected token".to_owned(),
    };
    let msg = e.to_string();
    assert!(msg.contains("10"), "got: {msg}");
    assert!(msg.contains("unexpected token"), "got: {msg}");
}

#[test]
fn import_error_graph_write_display() {
    let e = ImportError::GraphWrite("node insert failed".to_owned());
    let msg = e.to_string();
    assert!(msg.contains("node insert failed"), "got: {msg}");
}

#[test]
fn export_error_graph_read_display() {
    let e = ExportError::GraphRead("not found".to_owned());
    let msg = e.to_string();
    assert!(msg.contains("not found"), "got: {msg}");
}

#[test]
fn export_error_serialize_display() {
    let e = ExportError::Serialize("bad json".to_owned());
    let msg = e.to_string();
    assert!(msg.contains("bad json"), "got: {msg}");
}

#[test]
fn import_error_io_from_std() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let e: ImportError = io_err.into();
    let msg = e.to_string();
    assert!(msg.contains("I/O"), "got: {msg}");
}

#[test]
fn export_error_io_from_std() {
    let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
    let e: ExportError = io_err.into();
    let msg = e.to_string();
    assert!(msg.contains("I/O"), "got: {msg}");
}
