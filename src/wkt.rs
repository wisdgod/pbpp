//! Well-known types table.
//!
//! `google/protobuf/*.proto` files never participate as inputs; references
//! to well-known types resolve against this builtin table and only the
//! `import` lines survive pruning.

/// The well-known `.proto` files.
pub(crate) const FILES: &[&str] = &[
    "google/protobuf/any.proto",
    "google/protobuf/api.proto",
    "google/protobuf/duration.proto",
    "google/protobuf/empty.proto",
    "google/protobuf/field_mask.proto",
    "google/protobuf/source_context.proto",
    "google/protobuf/struct.proto",
    "google/protobuf/timestamp.proto",
    "google/protobuf/type.proto",
    "google/protobuf/wrappers.proto",
];

#[must_use]
pub(crate) fn is_wkt_file(path: &str) -> bool {
    FILES.contains(&path)
}

/// True for anything under the `google/protobuf/` namespace.
///
/// The design excludes the whole namespace from pruning inputs: only the
/// ten well-known files are importable, but siblings like
/// `descriptor.proto` or `compiler/plugin.proto` — proto2 files — must not
/// be treated as inputs either.
#[must_use]
pub fn is_google_protobuf_path(path: &str) -> bool {
    path.starts_with("google/protobuf/")
}

pub(crate) struct WktType {
    /// Simple name under `google.protobuf.`.
    pub(crate) name: &'static str,
    pub(crate) file: &'static str,
    pub(crate) is_enum: bool,
}

const fn msg(name: &'static str, file: &'static str) -> WktType {
    WktType {
        name,
        file,
        is_enum: false,
    }
}

const fn en(name: &'static str, file: &'static str) -> WktType {
    WktType {
        name,
        file,
        is_enum: true,
    }
}

pub(crate) const TYPES: &[WktType] = &[
    msg("Any", "google/protobuf/any.proto"),
    msg("Api", "google/protobuf/api.proto"),
    msg("Method", "google/protobuf/api.proto"),
    msg("Mixin", "google/protobuf/api.proto"),
    msg("Duration", "google/protobuf/duration.proto"),
    msg("Empty", "google/protobuf/empty.proto"),
    msg("FieldMask", "google/protobuf/field_mask.proto"),
    msg("SourceContext", "google/protobuf/source_context.proto"),
    msg("Struct", "google/protobuf/struct.proto"),
    msg("Value", "google/protobuf/struct.proto"),
    en("NullValue", "google/protobuf/struct.proto"),
    msg("ListValue", "google/protobuf/struct.proto"),
    msg("Timestamp", "google/protobuf/timestamp.proto"),
    msg("Type", "google/protobuf/type.proto"),
    msg("Field", "google/protobuf/type.proto"),
    en("Kind", "google/protobuf/type.proto"),
    en("Cardinality", "google/protobuf/type.proto"),
    msg("Enum", "google/protobuf/type.proto"),
    msg("EnumValue", "google/protobuf/type.proto"),
    msg("Option", "google/protobuf/type.proto"),
    en("Syntax", "google/protobuf/type.proto"),
    msg("DoubleValue", "google/protobuf/wrappers.proto"),
    msg("FloatValue", "google/protobuf/wrappers.proto"),
    msg("Int64Value", "google/protobuf/wrappers.proto"),
    msg("UInt64Value", "google/protobuf/wrappers.proto"),
    msg("Int32Value", "google/protobuf/wrappers.proto"),
    msg("UInt32Value", "google/protobuf/wrappers.proto"),
    msg("BoolValue", "google/protobuf/wrappers.proto"),
    msg("StringValue", "google/protobuf/wrappers.proto"),
    msg("BytesValue", "google/protobuf/wrappers.proto"),
];
