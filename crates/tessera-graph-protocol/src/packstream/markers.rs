// SPDX-License-Identifier: LicenseRef-TesseraGraph-Proprietary

//! `PackStream` marker byte constants.

pub const NULL: u8 = 0xC0;
pub const BOOL_FALSE: u8 = 0xC2;
pub const BOOL_TRUE: u8 = 0xC3;
pub const FLOAT64: u8 = 0xC1;

pub const INT8: u8 = 0xC8;
pub const INT16: u8 = 0xC9;
pub const INT32: u8 = 0xCA;
pub const INT64: u8 = 0xCB;

pub const TINY_STRING_BASE: u8 = 0x80;
pub const STRING8: u8 = 0xD0;
pub const STRING16: u8 = 0xD1;
pub const STRING32: u8 = 0xD2;

pub const BYTES8: u8 = 0xCC;
pub const BYTES16: u8 = 0xCD;
pub const BYTES32: u8 = 0xCE;

pub const TINY_LIST_BASE: u8 = 0x90;
pub const LIST8: u8 = 0xD4;
pub const LIST16: u8 = 0xD5;
pub const LIST32: u8 = 0xD6;

pub const TINY_DICT_BASE: u8 = 0xA0;
pub const DICT8: u8 = 0xD8;
pub const DICT16: u8 = 0xD9;
pub const DICT32: u8 = 0xDA;

pub const TINY_STRUCT_BASE: u8 = 0xB0;

/// Bolt struct tag for a Node.
pub const TAG_NODE: u8 = 0x4E;
/// Bolt struct tag for a Relationship.
pub const TAG_RELATIONSHIP: u8 = 0x52;
/// Bolt struct tag for a Path.
pub const TAG_PATH: u8 = 0x50;
/// Bolt struct tag for an `UnboundRelationship` (used only inside a Path).
pub const TAG_UNBOUND_RELATIONSHIP: u8 = 0x72;
