//! A raw view of the `FileDescriptorSet` protoc writes: just the parts
//! the request and event codegen reads, WITH gsb's custom options.
//!
//! Why our own messages: `prost_types`' option messages have no extension
//! support, so `(gsb.op)`, `(gsb.error)`, `(gsb.event)` … are dropped
//! when prost decodes the set. Protobuf skips unknown fields, so these trimmed
//! mirrors of `descriptor.proto` decode the same bytes and keep exactly
//! what we name — field numbers from `google/protobuf/descriptor.proto`,
//! the options' numbers from `gsb/options.proto`.

/// `google.protobuf.FileDescriptorSet`.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct FileSet {
    #[prost(message, repeated, tag = "1")]
    pub file: Vec<File>,
}

/// `google.protobuf.FileDescriptorProto`.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct File {
    #[prost(string, optional, tag = "1")]
    pub name: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub package: Option<String>,
    #[prost(message, repeated, tag = "4")]
    pub message_type: Vec<Message>,
    #[prost(message, repeated, tag = "6")]
    pub service: Vec<Service>,
    #[prost(message, optional, tag = "8")]
    pub options: Option<FileOptions>,
}

/// `google.protobuf.FileOptions` — only gsb's extension.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct FileOptions {
    /// `(gsb.reserved_ops)` (packed or not: prost reads both).
    #[prost(uint32, repeated, tag = "50005")]
    pub reserved_ops: Vec<u32>,
}

/// `google.protobuf.DescriptorProto` (name, nesting and options only).
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Message {
    #[prost(string, optional, tag = "1")]
    pub name: Option<String>,
    #[prost(message, repeated, tag = "3")]
    pub nested_type: Vec<Message>,
    #[prost(message, optional, tag = "7")]
    pub options: Option<MessageOptions>,
}

/// `google.protobuf.MessageOptions` — only gsb's extension.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct MessageOptions {
    /// `(gsb.event)`.
    #[prost(message, optional, tag = "50004")]
    pub event: Option<EventOptions>,
}

/// `gsb.EventOptions` (`gsb/options.proto`). proto3 scalars: an `op: 0`
/// or a `to: DESTINATION_UNSPECIFIED` is not encoded, so it reads as
/// absent — the same refusal as leaving it out.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct EventOptions {
    #[prost(uint32, optional, tag = "1")]
    pub op: Option<u32>,
    /// `gsb.Destination` as its number (an open enum: kept raw).
    #[prost(int32, optional, tag = "2")]
    pub to: Option<i32>,
}

/// `google.protobuf.ServiceDescriptorProto`.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Service {
    #[prost(string, optional, tag = "1")]
    pub name: Option<String>,
    #[prost(message, repeated, tag = "2")]
    pub method: Vec<Method>,
    #[prost(message, optional, tag = "3")]
    pub options: Option<ServiceOptions>,
}

/// `google.protobuf.MethodDescriptorProto`.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Method {
    #[prost(string, optional, tag = "1")]
    pub name: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub input_type: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub output_type: Option<String>,
    #[prost(message, optional, tag = "4")]
    pub options: Option<MethodOptions>,
    #[prost(bool, optional, tag = "5")]
    pub client_streaming: Option<bool>,
    #[prost(bool, optional, tag = "6")]
    pub server_streaming: Option<bool>,
}

/// `google.protobuf.MethodOptions` — only gsb's extensions.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct MethodOptions {
    /// `(gsb.op)`.
    #[prost(uint32, optional, tag = "50001")]
    pub op: Option<u32>,
    /// `(gsb.error)`.
    #[prost(string, optional, tag = "50002")]
    pub error: Option<String>,
}

/// `google.protobuf.ServiceOptions` — only gsb's extension.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ServiceOptions {
    /// `(gsb.reserved_op)` (packed or not: prost reads both).
    #[prost(uint32, repeated, tag = "50003")]
    pub reserved_op: Vec<u32>,
}
