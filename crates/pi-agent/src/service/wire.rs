//! Wire grammar for Chord service calls, catalogues, snapshots, and provider
//! updates.
//!
//! Mirrors the TypeScript `packages/chord/src/services/wire.ts` grammar over
//! the canonical value domain: strict field sets (absent required keys and
//! unknown keys are rejected), non-empty string identifiers, integer minima
//! (`generation >= 1`, update `sequence >= 1`, member snapshot
//! `sequence >= 0`), `"singleton"`/`"keyed"` modes, and unique catalogue ids.
//! A present `null` never models an omitted optional field; only a genuinely
//! absent `instance` key means "no address", exactly like the TypeScript
//! `undefined` check.
//!
//! Identifiers, member names, and instance keys are [`JsString`], so
//! source-admitted UTF-16 — including unpaired surrogates — passes validation
//! unchanged.  Generations and sequences are [`JsInteger`]: the source's
//! `Number.isInteger` admits any finite whole-valued double, so there is no
//! `u64` or `2^53` admission cap at this boundary; the remote transport
//! applies its own stricter limit before bytes cross the wire.
//!
//! Parsing consumes canonical [`JsonValue`] trees and serialization is the
//! explicit `into_json` conversion on each wire type — never an intermediate
//! foreign JSON tree.  Operation payloads stay typed: the decoded-domain
//! parsers build [`DeltaOp`] batches while the wire-domain parsers build
//! [`WireOp`] batches, both through the delta layer's `from_json`, and the
//! two validators stay separate.  Path-grammar rules — including the reserved
//! JavaScript prototype names — remain delegated to delta deserialization, so
//! service parsing never silently drops paths that a JavaScript replica would
//! reject.

use std::collections::HashSet;

use thiserror::Error;

use super::delta::{DeltaError, DeltaOp, WireOp};
use super::value::{JsInteger, JsObject, JsString, JsonValue};

/// Malformed service wire input.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WireError {
    /// A record fails its field-set, identifier, or bound checks.  The
    /// description mirrors the TypeScript `Invalid ${description}` message.
    #[error("Invalid {description}")]
    Invalid {
        /// Record description used in the error message.
        description: &'static str,
    },
    /// An operation inside a snapshot or update failed delta validation.
    #[error(transparent)]
    Delta(#[from] DeltaError),
}

const fn invalid(description: &'static str) -> WireError {
    WireError::Invalid { description }
}

/// The operation domain carried by service snapshots and provider updates:
/// decoded [`DeltaOp`] batches or wire [`WireOp`] tuples.
///
/// This is the boundary the source expresses with its `assertOp` parameter:
/// the two vocabularies share the service grammar but never the operation
/// parser.
pub trait ServiceOp: Sized {
    /// Parses one operation tuple from a canonical JSON tree, enforcing the
    /// domain's grammar and path rules.
    ///
    /// # Errors
    ///
    /// Returns `DeltaError` when the value does not match the operation grammar.
    fn from_json(value: &JsonValue) -> Result<Self, DeltaError>;
    /// Converts one operation into its canonical JSON tuple tree.
    fn into_json(self) -> JsonValue;
}

impl ServiceOp for DeltaOp {
    fn from_json(value: &JsonValue) -> Result<Self, DeltaError> {
        DeltaOp::from_json(value)
    }

    fn into_json(self) -> JsonValue {
        DeltaOp::into_json(self)
    }
}

impl ServiceOp for WireOp {
    fn from_json(value: &JsonValue) -> Result<Self, DeltaError> {
        WireOp::from_json(value)
    }

    fn into_json(self) -> JsonValue {
        WireOp::into_json(self)
    }
}

/// Subscription addressing for a service: one shared implementation or one per
/// key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ServiceMode {
    /// One shared implementation.
    Singleton,
    /// One implementation per instance key.
    Keyed,
}

impl ServiceMode {
    /// The source wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Singleton => "singleton",
            Self::Keyed => "keyed",
        }
    }
}

/// Stable identity for one instance of a keyed service.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ServiceInstanceAddress {
    /// Instance key chosen by the provider.
    pub key: JsString,
    /// Reincarnation counter; starts at one and grows when an instance is
    /// replaced under the same key.
    pub generation: JsInteger,
}

impl ServiceInstanceAddress {
    /// Converts to the canonical `{key, generation}` tree.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        JsonValue::Object(JsObject::from([
            (key("key"), JsonValue::String(self.key)),
            (key("generation"), JsonValue::Number(self.generation.as_f64())),
        ]))
    }
}

/// One member of an instance snapshot, parameterized by the operation domain.
#[derive(Clone, Debug, PartialEq)]
pub enum ServiceMemberSnapshot<Op: ServiceOp> {
    /// A callable method with no replicated state.
    Method {
        /// Member name.
        name: JsString,
    },
    /// A replicated state member.
    State {
        /// Member name.
        name: JsString,
        /// Monotonic publication counter starting at zero.
        sequence: JsInteger,
        /// Operations since the last base batch.
        ops: Vec<Op>,
    },
}

impl<Op: ServiceOp> ServiceMemberSnapshot<Op> {
    /// Converts to the canonical `{name, kind, ...}` tree.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        match self {
            Self::Method { name } => JsonValue::Object(JsObject::from([
                (key("name"), JsonValue::String(name)),
                (key("kind"), string("method")),
            ])),
            Self::State { name, sequence, ops } => JsonValue::Object(JsObject::from([
                (key("name"), JsonValue::String(name)),
                (key("kind"), string("state")),
                (key("sequence"), JsonValue::Number(sequence.as_f64())),
                (key("ops"), JsonValue::Array(ops_into_json(ops))),
            ])),
        }
    }
}

/// Snapshot of one live instance: its optional address and member descriptions.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceInstanceSnapshot<Op: ServiceOp> {
    /// Address when the service is keyed; absent for singletons.
    pub instance: Option<ServiceInstanceAddress>,
    /// Member descriptions in provider-defined order.
    pub members: Vec<ServiceMemberSnapshot<Op>>,
}

impl<Op: ServiceOp> ServiceInstanceSnapshot<Op> {
    /// Converts to the canonical `{members, instance?}` tree.  A `None`
    /// address omits the `instance` key entirely — never a present `null`.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        let mut object = JsObject::new();
        if let Some(instance) = self.instance {
            object.insert(key("instance"), instance.into_json());
        }
        object.insert(
            key("members"),
            JsonValue::Array(self.members.into_iter().map(ServiceMemberSnapshot::into_json).collect()),
        );
        JsonValue::Object(object)
    }
}

/// Snapshot of one subscribed service: catalogue identity plus live instances.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceSubscriptionSnapshot<Op: ServiceOp> {
    /// Subscribed service id.
    pub service_id: JsString,
    /// Subscription mode.
    pub mode: ServiceMode,
    /// Live instances at snapshot time.
    pub instances: Vec<ServiceInstanceSnapshot<Op>>,
}

impl<Op: ServiceOp> ServiceSubscriptionSnapshot<Op> {
    /// Converts to the canonical `{serviceId, mode, instances}` tree.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        JsonValue::Object(JsObject::from([
            (key("serviceId"), JsonValue::String(self.service_id)),
            (key("mode"), string(self.mode.as_str())),
            (
                key("instances"),
                JsonValue::Array(self.instances.into_iter().map(ServiceInstanceSnapshot::into_json).collect()),
            ),
        ]))
    }
}

/// Pushed provider update, parameterized by the operation domain.
#[derive(Clone, Debug, PartialEq)]
pub enum ServiceProviderUpdate<Op: ServiceOp> {
    /// One state member published new operations.
    State {
        /// Instance address when the service is keyed.
        instance: Option<ServiceInstanceAddress>,
        /// State member name.
        member: JsString,
        /// Publication sequence starting at one.
        sequence: JsInteger,
        /// Operations since the previous publication.
        ops: Vec<Op>,
    },
    /// The provider is temporarily unavailable.
    Unavailable,
    /// A live instance was replaced in place under its key.
    Replaced {
        /// Complete snapshot of the replacement instance.
        snapshot: ServiceInstanceSnapshot<Op>,
    },
    /// A new instance appeared.
    Spawned {
        /// Complete snapshot of the arriving instance.
        instance: ServiceInstanceSnapshot<Op>,
    },
    /// An instance closed.
    Closed {
        /// Address of the closed instance.
        instance: ServiceInstanceAddress,
    },
}

impl<Op: ServiceOp> ServiceProviderUpdate<Op> {
    /// Converts to the canonical `{type, ...}` tree.  A `None` state address
    /// omits the `instance` key entirely — never a present `null`.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        match self {
            Self::State {
                instance,
                member,
                sequence,
                ops,
            } => {
                let mut object = JsObject::from([
                    (key("type"), string("state")),
                    (key("member"), JsonValue::String(member)),
                    (key("sequence"), JsonValue::Number(sequence.as_f64())),
                    (key("ops"), JsonValue::Array(ops_into_json(ops))),
                ]);
                if let Some(instance) = instance {
                    object.insert(key("instance"), instance.into_json());
                }
                JsonValue::Object(object)
            }
            Self::Unavailable => JsonValue::Object(JsObject::from([(key("type"), string("unavailable"))])),
            Self::Replaced { snapshot } => JsonValue::Object(JsObject::from([
                (key("type"), string("replaced")),
                (key("snapshot"), snapshot.into_json()),
            ])),
            Self::Spawned { instance } => JsonValue::Object(JsObject::from([
                (key("type"), string("spawned")),
                (key("instance"), instance.into_json()),
            ])),
            Self::Closed { instance } => JsonValue::Object(JsObject::from([
                (key("type"), string("closed")),
                (key("instance"), instance.into_json()),
            ])),
        }
    }
}

/// Catalogue entry describing one published service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceCatalogueEntry {
    /// Published service id.
    pub service_id: JsString,
    /// Subscription mode the provider implements.
    pub mode: ServiceMode,
}

impl ServiceCatalogueEntry {
    /// Converts to the canonical `{serviceId, mode}` tree.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        JsonValue::Object(JsObject::from([
            (key("serviceId"), JsonValue::String(self.service_id)),
            (key("mode"), string(self.mode.as_str())),
        ]))
    }
}

/// An invocation addressed to one service member.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceCall {
    /// Target service id.
    pub service_id: JsString,
    /// Instance address for keyed services; absent for singletons.
    pub instance: Option<ServiceInstanceAddress>,
    /// Target member name.
    pub member: JsString,
    /// Canonical arguments copied from the parsed value tree.
    pub args: Vec<JsonValue>,
}

impl ServiceCall {
    /// Converts to the canonical `{serviceId, member, args, instance?}` tree.
    /// A `None` address omits the `instance` key entirely — never a present
    /// `null`.
    #[must_use]
    pub fn into_json(self) -> JsonValue {
        let mut object = JsObject::new();
        if let Some(instance) = self.instance {
            object.insert(key("instance"), instance.into_json());
        }
        object.insert(key("serviceId"), JsonValue::String(self.service_id));
        object.insert(key("member"), JsonValue::String(self.member));
        object.insert(key("args"), JsonValue::Array(self.args));
        JsonValue::Object(object)
    }
}

/// Wire-domain member snapshot carrying [`WireOp`] tuples.
pub type WireServiceMemberSnapshot = ServiceMemberSnapshot<WireOp>;
/// Wire-domain instance snapshot carrying [`WireOp`] tuples.
pub type WireServiceInstanceSnapshot = ServiceInstanceSnapshot<WireOp>;
/// Wire-domain subscription snapshot carrying [`WireOp`] tuples.
pub type WireServiceSubscriptionSnapshot = ServiceSubscriptionSnapshot<WireOp>;
/// Wire-domain provider update carrying [`WireOp`] tuples.
pub type WireServiceProviderUpdate = ServiceProviderUpdate<WireOp>;

const SERVICE_CONTROL_ID: &str = "$chord.service";
const SERVICE_CATALOGUE_MEMBER: &str = "catalogue";
const SERVICE_SUBSCRIBE_MEMBER: &str = "subscribe";
const SERVICE_UNSUBSCRIBE_MEMBER: &str = "unsubscribe";

/// A decoded `$chord.service` control call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceControlCall {
    /// Request the catalogue.
    Catalogue,
    /// Open or refresh a subscription.
    Subscribe {
        /// Caller-chosen subscription id.
        subscription_id: JsString,
        /// Subscribed service id.
        service_id: JsString,
        /// Subscription mode.
        mode: ServiceMode,
    },
    /// Close one subscription.
    Unsubscribe {
        /// Caller-chosen subscription id.
        subscription_id: JsString,
    },
}

/// Creates the `$chord.service.catalogue` control call.
#[must_use]
pub fn create_service_catalogue_call() -> ServiceCall {
    ServiceCall {
        service_id: JsString::from_utf8(SERVICE_CONTROL_ID),
        instance: None,
        member: JsString::from_utf8(SERVICE_CATALOGUE_MEMBER),
        args: Vec::new(),
    }
}

/// Creates the `$chord.service.subscribe` control call.
#[must_use]
pub fn create_service_subscribe_call(
    subscription_id: impl Into<JsString>,
    service_id: impl Into<JsString>,
    mode: ServiceMode,
) -> ServiceCall {
    ServiceCall {
        service_id: JsString::from_utf8(SERVICE_CONTROL_ID),
        instance: None,
        member: JsString::from_utf8(SERVICE_SUBSCRIBE_MEMBER),
        args: vec![
            JsonValue::String(subscription_id.into()),
            JsonValue::String(service_id.into()),
            string(mode.as_str()),
        ],
    }
}

/// Creates the `$chord.service.unsubscribe` control call.
#[must_use]
pub fn create_service_unsubscribe_call(subscription_id: impl Into<JsString>) -> ServiceCall {
    ServiceCall {
        service_id: JsString::from_utf8(SERVICE_CONTROL_ID),
        instance: None,
        member: JsString::from_utf8(SERVICE_UNSUBSCRIBE_MEMBER),
        args: vec![JsonValue::String(subscription_id.into())],
    }
}

/// Recognizes a control call with exact member arity.  A control id carrying an
/// instance address, a wrong arity, or malformed arguments is not a control
/// call and returns `None`, mirroring the source.
#[must_use]
pub fn decode_service_control_call(call: &ServiceCall) -> Option<ServiceControlCall> {
    if !same_text(&call.service_id, SERVICE_CONTROL_ID) || call.instance.is_some() {
        return None;
    }
    if same_text(&call.member, SERVICE_CATALOGUE_MEMBER) && call.args.is_empty() {
        return Some(ServiceControlCall::Catalogue);
    }
    if same_text(&call.member, SERVICE_SUBSCRIBE_MEMBER) && call.args.len() == 3 {
        if let (Some(subscription_id), Some(service_id), Some(mode)) = (
            id_value(&call.args[0]),
            id_value(&call.args[1]),
            mode_value(&call.args[2]),
        ) {
            return Some(ServiceControlCall::Subscribe {
                subscription_id: subscription_id.clone(),
                service_id: service_id.clone(),
                mode,
            });
        }
        return None;
    }
    if same_text(&call.member, SERVICE_UNSUBSCRIBE_MEMBER)
        && call.args.len() == 1
        && let Some(subscription_id) = id_value(&call.args[0])
    {
        return Some(ServiceControlCall::Unsubscribe {
            subscription_id: subscription_id.clone(),
        });
    }
    None
}

/// Validates a raw service call record.
///
/// # Errors
///
/// Returns `WireError::Invalid` when the record is not an object, is missing
/// required fields, has unknown keys, empty identifiers, or a malformed address.
pub fn parse_service_call(value: &JsonValue) -> Result<ServiceCall, WireError> {
    const DESCRIPTION: &str = "service call";
    let call = record(value, DESCRIPTION)?;
    assert_keys(call, &["serviceId", "member", "args"], &["instance"], DESCRIPTION)?;
    let (Some(service_id), Some(member), Some(args)) = (
        id_field(call, "serviceId"),
        id_field(call, "member"),
        field(call, "args").and_then(as_array),
    ) else {
        return Err(invalid(DESCRIPTION));
    };
    let instance = optional_address(call)?;
    Ok(ServiceCall {
        service_id: service_id.clone(),
        instance,
        member: member.clone(),
        args: args.clone(),
    })
}

/// Validates a raw catalogue reply.  Service ids must be unique.
///
/// # Errors
///
/// Returns `WireError::Invalid` when the input is not an array or an entry is
/// malformed or has a duplicate service id.
pub fn parse_service_catalogue(value: &JsonValue) -> Result<Vec<ServiceCatalogueEntry>, WireError> {
    const DESCRIPTION: &str = "service catalogue";
    const ENTRY: &str = "service catalogue entry";
    let Some(entries) = as_array(value) else {
        return Err(invalid(DESCRIPTION));
    };
    let mut seen = HashSet::with_capacity(entries.len());
    let mut parsed = Vec::with_capacity(entries.len());
    for candidate in entries {
        let entry = record(candidate, ENTRY)?;
        assert_keys(entry, &["serviceId", "mode"], &[], ENTRY)?;
        let Some(service_id) = id_field(entry, "serviceId") else {
            return Err(invalid(DESCRIPTION));
        };
        let Some(mode) = field(entry, "mode").and_then(mode_value) else {
            return Err(invalid(DESCRIPTION));
        };
        if !seen.insert(service_id.clone()) {
            return Err(invalid(DESCRIPTION));
        }
        parsed.push(ServiceCatalogueEntry {
            service_id: service_id.clone(),
            mode,
        });
    }
    Ok(parsed)
}

/// Validates a raw subscription snapshot whose operations are decoded
/// [`DeltaOp`] batches.
///
/// # Errors
///
/// Returns `WireError::Invalid` for a malformed snapshot envelope or
/// `WireError::Delta` if an operation batch is rejected.
pub fn parse_service_subscription_snapshot(
    value: &JsonValue,
) -> Result<ServiceSubscriptionSnapshot<DeltaOp>, WireError> {
    parse_subscription_snapshot::<DeltaOp>(value)
}

/// Validates a raw subscription snapshot whose operations are wire [`WireOp`]
/// tuples.
///
/// # Errors
///
/// Returns `WireError::Invalid` for a malformed snapshot envelope or
/// `WireError::Delta` if an operation batch is rejected.
pub fn parse_wire_service_subscription_snapshot(value: &JsonValue) -> Result<WireServiceSubscriptionSnapshot, WireError> {
    parse_subscription_snapshot::<WireOp>(value)
}

/// Validates a raw provider update whose operations are decoded [`DeltaOp`]
/// batches.
///
/// # Errors
///
/// Returns `WireError::Invalid` for a malformed update or `WireError::Delta`
/// if an operation batch is rejected.
pub fn parse_service_provider_update(value: &JsonValue) -> Result<ServiceProviderUpdate<DeltaOp>, WireError> {
    parse_provider_update::<DeltaOp>(value)
}

/// Validates a raw provider update whose operations are wire [`WireOp`] tuples.
///
/// # Errors
///
/// Returns `WireError::Invalid` for a malformed update or `WireError::Delta`
/// if an operation batch is rejected.
pub fn parse_wire_service_provider_update(value: &JsonValue) -> Result<WireServiceProviderUpdate, WireError> {
    parse_provider_update::<WireOp>(value)
}

fn parse_subscription_snapshot<Op: ServiceOp>(
    value: &JsonValue,
) -> Result<ServiceSubscriptionSnapshot<Op>, WireError> {
    const DESCRIPTION: &str = "service subscription snapshot";
    let snapshot = record(value, DESCRIPTION)?;
    assert_keys(snapshot, &["serviceId", "mode", "instances"], &[], DESCRIPTION)?;
    let Some(service_id) = id_field(snapshot, "serviceId") else {
        return Err(invalid(DESCRIPTION));
    };
    let Some(mode) = field(snapshot, "mode").and_then(mode_value) else {
        return Err(invalid(DESCRIPTION));
    };
    let Some(instances) = field(snapshot, "instances").and_then(as_array) else {
        return Err(invalid(DESCRIPTION));
    };
    let mut parsed = Vec::with_capacity(instances.len());
    for instance in instances {
        parsed.push(parse_instance::<Op>(instance)?);
    }
    Ok(ServiceSubscriptionSnapshot {
        service_id: service_id.clone(),
        mode,
        instances: parsed,
    })
}

fn parse_provider_update<Op: ServiceOp>(value: &JsonValue) -> Result<ServiceProviderUpdate<Op>, WireError> {
    let update = record(value, "service provider update")?;
    let Some(JsonValue::String(kind)) = field(update, "type") else {
        return Err(invalid("service provider update"));
    };
    if same_text(kind, "state") {
        assert_keys(update, &["type", "member", "sequence", "ops"], &["instance"], "state update")?;
        let Some(member) = id_field(update, "member") else {
            return Err(invalid("service state update"));
        };
        let sequence = integer_field(update, "sequence", 1.0, "service state update")?;
        let Some(ops) = field(update, "ops").and_then(as_array) else {
            return Err(invalid("service state update"));
        };
        let instance = optional_address(update)?;
        let mut parsed = Vec::with_capacity(ops.len());
        for op in ops {
            parsed.push(parse_op::<Op>(op)?);
        }
        Ok(ServiceProviderUpdate::State {
            instance,
            member: member.clone(),
            sequence,
            ops: parsed,
        })
    } else if same_text(kind, "unavailable") {
        assert_keys(update, &["type"], &[], "unavailable update")?;
        Ok(ServiceProviderUpdate::Unavailable)
    } else if same_text(kind, "replaced") {
        assert_keys(update, &["type", "snapshot"], &[], "replacement update")?;
        Ok(ServiceProviderUpdate::Replaced {
            snapshot: parse_instance(required_field(update, "snapshot", "replacement update")?)?,
        })
    } else if same_text(kind, "spawned") {
        assert_keys(update, &["type", "instance"], &[], "spawn update")?;
        Ok(ServiceProviderUpdate::Spawned {
            instance: parse_instance(required_field(update, "instance", "spawn update")?)?,
        })
    } else if same_text(kind, "closed") {
        assert_keys(update, &["type", "instance"], &[], "close update")?;
        Ok(ServiceProviderUpdate::Closed {
            instance: parse_address(required_field(update, "instance", "close update")?)?,
        })
    } else {
        Err(invalid("service provider update"))
    }
}

fn parse_instance<Op: ServiceOp>(value: &JsonValue) -> Result<ServiceInstanceSnapshot<Op>, WireError> {
    const DESCRIPTION: &str = "service instance snapshot";
    let instance = record(value, DESCRIPTION)?;
    assert_keys(instance, &["members"], &["instance"], DESCRIPTION)?;
    let address = optional_address(instance)?;
    let Some(members) = field(instance, "members").and_then(as_array) else {
        return Err(invalid(DESCRIPTION));
    };
    let mut parsed = Vec::with_capacity(members.len());
    for member in members {
        parsed.push(parse_member::<Op>(member)?);
    }
    Ok(ServiceInstanceSnapshot {
        instance: address,
        members: parsed,
    })
}

fn parse_member<Op: ServiceOp>(value: &JsonValue) -> Result<ServiceMemberSnapshot<Op>, WireError> {
    let member = record(value, "service member snapshot")?;
    let Some(JsonValue::String(kind)) = field(member, "kind") else {
        return Err(invalid("service member snapshot"));
    };
    if same_text(kind, "method") {
        const DESCRIPTION: &str = "service method snapshot";
        assert_keys(member, &["name", "kind"], &[], DESCRIPTION)?;
        let Some(name) = id_field(member, "name") else {
            return Err(invalid(DESCRIPTION));
        };
        Ok(ServiceMemberSnapshot::Method { name: name.clone() })
    } else if same_text(kind, "state") {
        const DESCRIPTION: &str = "service state snapshot";
        assert_keys(member, &["name", "kind", "sequence", "ops"], &[], DESCRIPTION)?;
        let Some(name) = id_field(member, "name") else {
            return Err(invalid(DESCRIPTION));
        };
        let sequence = integer_field(member, "sequence", 0.0, DESCRIPTION)?;
        let Some(ops) = field(member, "ops").and_then(as_array) else {
            return Err(invalid(DESCRIPTION));
        };
        let mut parsed = Vec::with_capacity(ops.len());
        for op in ops {
            parsed.push(parse_op::<Op>(op)?);
        }
        Ok(ServiceMemberSnapshot::State {
            name: name.clone(),
            sequence,
            ops: parsed,
        })
    } else {
        Err(invalid("service member snapshot"))
    }
}

fn parse_address(value: &JsonValue) -> Result<ServiceInstanceAddress, WireError> {
    const DESCRIPTION: &str = "service instance address";
    let address = record(value, DESCRIPTION)?;
    assert_keys(address, &["key", "generation"], &[], DESCRIPTION)?;
    let Some(key) = id_field(address, "key") else {
        return Err(invalid(DESCRIPTION));
    };
    let generation = integer_field(address, "generation", 1.0, DESCRIPTION)?;
    Ok(ServiceInstanceAddress {
        key: key.clone(),
        generation,
    })
}

/// Parses one operation tuple through the delta layer, which enforces the
/// operation grammar and the path rules (including reserved prototype names).
fn parse_op<Op: ServiceOp>(value: &JsonValue) -> Result<Op, WireError> {
    Op::from_json(value).map_err(WireError::Delta)
}

/// Rejects non-object records, including arrays and `null`.
fn record<'a>(value: &'a JsonValue, description: &'static str) -> Result<&'a JsObject, WireError> {
    match value {
        JsonValue::Object(map) => Ok(map),
        _ => Err(invalid(description)),
    }
}

/// Enforces the exact field set: every required key present, no unknown keys.
fn assert_keys(
    map: &JsObject,
    required: &[&str],
    optional: &[&str],
    description: &'static str,
) -> Result<(), WireError> {
    for &key in required {
        if field(map, key).is_none() {
            return Err(invalid(description));
        }
    }
    if map.keys().any(|name| {
        !required
            .iter()
            .chain(optional.iter())
            .any(|allowed| same_text(name, allowed))
    }) {
        return Err(invalid(description));
    }
    Ok(())
}

/// Compares a canonical UTF-16 string with a UTF-8 literal without
/// replacement characters.
fn same_text(value: &JsString, expected: &str) -> bool {
    value.as_utf16().iter().copied().eq(expected.encode_utf16())
}

/// Looks up one field by its source JSON name without allocating: the key
/// comparison runs directly on UTF-16 code units.
fn field<'a>(map: &'a JsObject, key: &str) -> Option<&'a JsonValue> {
    map.iter()
        .find_map(|(name, value)| same_text(name, key).then_some(value))
}

fn required_field<'a>(map: &'a JsObject, key: &str, description: &'static str) -> Result<&'a JsonValue, WireError> {
    field(map, key).ok_or_else(|| invalid(description))
}

fn as_array(value: &JsonValue) -> Option<&Vec<JsonValue>> {
    match value {
        JsonValue::Array(items) => Some(items),
        _ => None,
    }
}

fn id_value(value: &JsonValue) -> Option<&JsString> {
    match value {
        JsonValue::String(id) if !id.as_utf16().is_empty() => Some(id),
        _ => None,
    }
}

fn id_field<'a>(map: &'a JsObject, key: &str) -> Option<&'a JsString> {
    field(map, key).and_then(id_value)
}

fn mode_value(value: &JsonValue) -> Option<ServiceMode> {
    match value {
        JsonValue::String(mode) if same_text(mode, "singleton") => Some(ServiceMode::Singleton),
        JsonValue::String(mode) if same_text(mode, "keyed") => Some(ServiceMode::Keyed),
        _ => None,
    }
}

fn optional_address(map: &JsObject) -> Result<Option<ServiceInstanceAddress>, WireError> {
    match field(map, "instance") {
        // A present `null` fails address validation exactly like the source;
        // only a genuinely absent field models `undefined`.
        Some(raw) => parse_address(raw).map(Some),
        None => Ok(None),
    }
}

/// Validates an integer with a minimum, preserving the source's
/// `Number.isInteger` behavior exactly: any finite whole-valued double is
/// admitted, including values beyond `2^53` or `u64::MAX`.  There is no
/// magnitude cap at this boundary; the remote transport applies its own
/// stricter limit before bytes cross the wire.
fn integer_value(value: &JsonValue, minimum: f64, description: &'static str) -> Result<JsInteger, WireError> {
    let JsonValue::Number(number) = value else {
        return Err(invalid(description));
    };
    if *number < minimum {
        return Err(invalid(description));
    }
    JsInteger::new(*number).map_err(|_| invalid(description))
}

fn integer_field(map: &JsObject, key: &str, minimum: f64, description: &'static str) -> Result<JsInteger, WireError> {
    match field(map, key) {
        Some(value) => integer_value(value, minimum, description),
        None => Err(invalid(description)),
    }
}

/// Builds a canonical object key from a source field name.
fn key(name: &str) -> JsString {
    JsString::from_utf8(name)
}

/// Builds a canonical string value from a fixed ASCII discriminant.
fn string(value: &str) -> JsonValue {
    JsonValue::String(JsString::from_utf8(value))
}

fn ops_into_json<Op: ServiceOp>(ops: Vec<Op>) -> Vec<JsonValue> {
    ops.into_iter().map(ServiceOp::into_json).collect()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "test fixtures and assertions use expect and panic for irrecoverable failures"
)]
mod tests {
    use super::*;
    use crate::service::value::parse_json;

    fn json(text: &str) -> JsonValue {
        parse_json(text).expect("test JSON parses")
    }

    fn integer(value: f64) -> JsInteger {
        JsInteger::new(value).expect("test integer")
    }

    fn delta_op(text: &str) -> DeltaOp {
        DeltaOp::from_json(&json(text)).expect("test op parses")
    }

    fn object_field<'a>(value: &'a JsonValue, name: &str) -> Option<&'a JsonValue> {
        match value {
            JsonValue::Object(map) => field(map, name),
            _ => None,
        }
    }

    #[test]
    fn control_calls_encode_and_decode_with_exact_arity() {
        let subscribe = create_service_subscribe_call("sub-1", "chat", ServiceMode::Keyed);
        assert_eq!(
            subscribe.clone().into_json(),
            json(r#"{"serviceId":"$chord.service","member":"subscribe","args":["sub-1","chat","keyed"]}"#),
        );
        // `instance` is absent, never null.
        assert_eq!(object_field(&subscribe.clone().into_json(), "instance"), None);
        assert_eq!(
            decode_service_control_call(&subscribe),
            Some(ServiceControlCall::Subscribe {
                subscription_id: JsString::from_utf8("sub-1"),
                service_id: JsString::from_utf8("chat"),
                mode: ServiceMode::Keyed,
            })
        );
        assert_eq!(
            decode_service_control_call(&create_service_catalogue_call()),
            Some(ServiceControlCall::Catalogue)
        );
        assert_eq!(
            decode_service_control_call(&create_service_unsubscribe_call("sub-1")),
            Some(ServiceControlCall::Unsubscribe {
                subscription_id: JsString::from_utf8("sub-1"),
            })
        );

        let control = |instance: Option<ServiceInstanceAddress>, member: &str, args: Vec<JsonValue>| ServiceCall {
            service_id: JsString::from_utf8(SERVICE_CONTROL_ID),
            instance,
            member: JsString::from_utf8(member),
            args,
        };
        let addressed = Some(ServiceInstanceAddress {
            key: JsString::from_utf8("room"),
            generation: integer(1.0),
        });
        assert_eq!(
            decode_service_control_call(&control(addressed.clone(), SERVICE_CATALOGUE_MEMBER, vec![])),
            None
        );
        assert_eq!(
            decode_service_control_call(&control(
                None,
                SERVICE_CATALOGUE_MEMBER,
                vec![JsonValue::Number(1.0)]
            )),
            None
        );
        assert_eq!(
            decode_service_control_call(&control(
                None,
                SERVICE_SUBSCRIBE_MEMBER,
                vec![string("sub"), string("chat")]
            )),
            None
        );
        assert_eq!(
            decode_service_control_call(&control(
                None,
                SERVICE_SUBSCRIBE_MEMBER,
                vec![string("sub"), string("chat"), string("local")]
            )),
            None
        );
        assert_eq!(
            decode_service_control_call(&control(None, SERVICE_UNSUBSCRIBE_MEMBER, vec![])),
            None
        );
        assert_eq!(
            decode_service_control_call(&ServiceCall {
                service_id: JsString::from_utf8("chat"),
                instance: None,
                member: JsString::from_utf8(SERVICE_CATALOGUE_MEMBER),
                args: vec![],
            }),
            None
        );
    }

    #[test]
    fn service_call_instance_omission_is_not_null() {
        let call = parse_service_call(&json(
            r#"{"serviceId":"chat","member":"post","instance":{"key":"room","generation":2},"args":[{"text":"hi"}]}"#,
        ))
        .expect("parse call");
        assert_eq!(
            call.instance,
            Some(ServiceInstanceAddress {
                key: JsString::from_utf8("room"),
                generation: integer(2.0),
            })
        );
        // The parsed call re-encodes to the identical tree, address included.
        assert_eq!(
            call.clone().into_json(),
            json(r#"{"serviceId":"chat","member":"post","instance":{"key":"room","generation":2},"args":[{"text":"hi"}]}"#),
        );

        let bare = parse_service_call(&json(r#"{"serviceId":"chat","member":"post","args":[]}"#)).expect("bare call");
        assert_eq!(bare.instance, None);
        // Absence round-trips as absence: no `instance` key is emitted.
        assert_eq!(
            bare.into_json(),
            json(r#"{"serviceId":"chat","member":"post","args":[]}"#),
        );

        let null_instance = json(r#"{"serviceId":"chat","member":"post","args":[],"instance":null}"#);
        assert_eq!(
            parse_service_call(&null_instance).expect_err("null instance is not omission"),
            WireError::Invalid {
                description: "service instance address",
            }
        );
    }

    #[test]
    fn service_call_rejects_malformed_records() {
        let missing_member = json(r#"{"serviceId":"chat","args":[]}"#);
        assert_eq!(
            parse_service_call(&missing_member).expect_err("missing member"),
            WireError::Invalid {
                description: "service call",
            }
        );
        let unknown_key = json(r#"{"serviceId":"chat","member":"post","args":[],"extra":1}"#);
        assert_eq!(
            parse_service_call(&unknown_key).expect_err("unknown key"),
            WireError::Invalid {
                description: "service call",
            }
        );
        let empty_id = json(r#"{"serviceId":"","member":"post","args":[]}"#);
        assert_eq!(
            parse_service_call(&empty_id).expect_err("empty id"),
            WireError::Invalid {
                description: "service call",
            }
        );
        let args_not_array = json(r#"{"serviceId":"chat","member":"post","args":"x"}"#);
        assert_eq!(
            parse_service_call(&args_not_array).expect_err("args must be an array"),
            WireError::Invalid {
                description: "service call",
            }
        );
        let zero_generation = json(
            r#"{"serviceId":"chat","member":"post","args":[],"instance":{"key":"room","generation":0}}"#,
        );
        assert_eq!(
            parse_service_call(&zero_generation).expect_err("generation starts at one"),
            WireError::Invalid {
                description: "service instance address",
            }
        );
    }

    #[test]
    fn source_admitted_utf16_ids_survive_validation() {
        // A lone surrogate is a valid JavaScript string and therefore a valid
        // service id; the canonical parser and validator must not reject or
        // replace it.
        let call = parse_service_call(&json(
            "{\"serviceId\":\"\\ud800x\",\"member\":\"post\",\"args\":[]}",
        ))
        .expect("lone surrogate id is source-admitted");
        assert_eq!(call.service_id.as_utf16(), &[0xD800, u16::from(b'x')]);
        // The same id re-encodes with the surrogate re-escaped, not replaced.
        assert_eq!(
            call.into_json(),
            json("{\"serviceId\":\"\\ud800x\",\"member\":\"post\",\"args\":[]}"),
        );

        // A valid surrogate pair survives as a pair.
        let pair = parse_service_call(&json(
            "{\"serviceId\":\"\\ud83d\\ude00\",\"member\":\"post\",\"args\":[]}",
        ))
        .expect("paired surrogate id");
        assert_eq!(pair.service_id.as_utf16(), &[0xD83D, 0xDE00]);

        let keyed = parse_service_call(&json(
            "{\"serviceId\":\"chat\",\"member\":\"post\",\"args\":[],\"instance\":{\"key\":\"\\udc00\",\"generation\":1}}",
        ))
        .expect("lone surrogate instance key");
        assert_eq!(
            keyed
                .instance
                .as_ref()
                .expect("keyed address")
                .key
                .as_utf16(),
            &[0xDC00],
        );
    }

    #[test]
    fn catalogue_requires_unique_ids_and_known_modes() {
        let parsed = parse_service_catalogue(&json(
            r#"[{"serviceId":"chat","mode":"singleton"},{"serviceId":"room","mode":"keyed"}]"#,
        ))
        .expect("parse catalogue");
        assert_eq!(
            parsed,
            vec![
                ServiceCatalogueEntry {
                    service_id: JsString::from_utf8("chat"),
                    mode: ServiceMode::Singleton,
                },
                ServiceCatalogueEntry {
                    service_id: JsString::from_utf8("room"),
                    mode: ServiceMode::Keyed,
                },
            ]
        );
        let duplicate =
            json(r#"[{"serviceId":"chat","mode":"singleton"},{"serviceId":"chat","mode":"keyed"}]"#);
        assert_eq!(
            parse_service_catalogue(&duplicate).expect_err("duplicate id"),
            WireError::Invalid {
                description: "service catalogue",
            }
        );
        let unknown_mode = json(r#"[{"serviceId":"chat","mode":"local"}]"#);
        assert_eq!(
            parse_service_catalogue(&unknown_mode).expect_err("unknown mode"),
            WireError::Invalid {
                description: "service catalogue",
            }
        );
        let not_array = json(r#"{"serviceId":"chat"}"#);
        assert_eq!(
            parse_service_catalogue(&not_array).expect_err("catalogue is an array"),
            WireError::Invalid {
                description: "service catalogue",
            }
        );
    }

    #[test]
    fn wire_and_decoded_op_validators_stay_separate() {
        let wire_only = json(
            r##"{"serviceId":"chat","mode":"singleton","instances":[{"members":[{"kind":"state","name":"text","sequence":1,"ops":[["#",0,["a"]],["a",0,"x"]]}]}]}"##,
        );
        let wire_snapshot = parse_wire_service_subscription_snapshot(&wire_only).expect("wire domain");
        // The wire member re-encodes to the identical tree.
        assert_eq!(
            wire_snapshot.instances[0].members[0].clone().into_json(),
            json(r##"{"kind":"state","name":"text","sequence":1,"ops":[["#",0,["a"]],["a",0,"x"]]}"##),
        );
        assert!(parse_service_subscription_snapshot(&wire_only).is_err());

        let short_form = json(
            r#"{"serviceId":"chat","mode":"singleton","instances":[{"members":[{"kind":"state","name":"text","sequence":1,"ops":[["s",5]]}]}]}"#,
        );
        assert!(parse_wire_service_subscription_snapshot(&short_form).is_ok());
        assert!(parse_service_subscription_snapshot(&short_form).is_err());

        let unsafe_path = json(
            r#"{"serviceId":"chat","mode":"singleton","instances":[{"members":[{"kind":"state","name":"text","sequence":1,"ops":[["s",["constructor"],1]]}]}]}"#,
        );
        assert!(matches!(
            parse_service_subscription_snapshot(&unsafe_path),
            Err(WireError::Delta(_))
        ));
        assert!(matches!(
            parse_wire_service_subscription_snapshot(&unsafe_path),
            Err(WireError::Delta(_))
        ));
    }

    #[test]
    fn provider_update_parses_every_variant() {
        let state = parse_wire_service_provider_update(&json(
            r#"{"type":"state","instance":{"key":"room","generation":1},"member":"counter","sequence":4,"ops":[["s",["count"],7]]}"#,
        ))
        .expect("state update");
        let WireServiceProviderUpdate::State {
            instance,
            member,
            sequence,
            ops,
        } = &state
        else {
            panic!("state variant");
        };
        assert_eq!(
            instance,
            &Some(ServiceInstanceAddress {
                key: JsString::from_utf8("room"),
                generation: integer(1.0),
            })
        );
        assert_eq!(member, &JsString::from_utf8("counter"));
        assert_eq!(*sequence, integer(4.0));
        assert_eq!(ops.len(), 1);
        // The parsed update re-encodes to the identical tree.
        assert_eq!(
            state.clone().into_json(),
            json(r#"{"type":"state","instance":{"key":"room","generation":1},"member":"counter","sequence":4,"ops":[["s",["count"],7]]}"#),
        );

        assert_eq!(
            parse_wire_service_provider_update(&json(r#"{"type":"unavailable"}"#)).expect("unavailable"),
            WireServiceProviderUpdate::Unavailable
        );
        let replaced = parse_wire_service_provider_update(&json(
            r#"{"type":"replaced","snapshot":{"instance":{"key":"room","generation":3},"members":[{"kind":"method","name":"post"}]}}"#,
        ))
        .expect("replaced");
        assert_eq!(
            replaced,
            WireServiceProviderUpdate::Replaced {
                snapshot: ServiceInstanceSnapshot {
                    instance: Some(ServiceInstanceAddress {
                        key: JsString::from_utf8("room"),
                        generation: integer(3.0),
                    }),
                    members: vec![ServiceMemberSnapshot::Method {
                        name: JsString::from_utf8("post"),
                    }],
                },
            }
        );
        assert_eq!(
            parse_wire_service_provider_update(&json(r#"{"type":"spawned","instance":{"members":[]}}"#))
                .expect("spawned"),
            WireServiceProviderUpdate::Spawned {
                instance: ServiceInstanceSnapshot {
                    instance: None,
                    members: vec![],
                },
            }
        );
        assert_eq!(
            parse_wire_service_provider_update(&json(
                r#"{"type":"closed","instance":{"key":"room","generation":3}}"#,
            ))
            .expect("closed"),
            WireServiceProviderUpdate::Closed {
                instance: ServiceInstanceAddress {
                    key: JsString::from_utf8("room"),
                    generation: integer(3.0),
                },
            }
        );
        let unknown_type = json(r#"{"type":"drained"}"#);
        assert_eq!(
            parse_wire_service_provider_update(&unknown_type).expect_err("unknown type"),
            WireError::Invalid {
                description: "service provider update",
            }
        );
        let extra_key = json(r#"{"type":"unavailable","reason":"restart"}"#);
        assert_eq!(
            parse_wire_service_provider_update(&extra_key).expect_err("extra key"),
            WireError::Invalid {
                description: "unavailable update",
            }
        );
    }

    #[test]
    fn numeric_bounds_follow_source_semantics() {
        let state_update = |sequence: &str| {
            json(&format!(
                r#"{{"type":"state","member":"counter","sequence":{sequence},"ops":[]}}"#
            ))
        };
        assert!(parse_wire_service_provider_update(&state_update("1")).is_ok());
        assert_eq!(
            parse_wire_service_provider_update(&state_update("0")).expect_err("update sequence starts at one"),
            WireError::Invalid {
                description: "service state update",
            }
        );
        assert_eq!(
            parse_wire_service_provider_update(&state_update("1.5")).expect_err("fractional sequence"),
            WireError::Invalid {
                description: "service state update",
            }
        );
        assert_eq!(
            parse_wire_service_provider_update(&state_update("-3")).expect_err("negative sequence"),
            WireError::Invalid {
                description: "service state update",
            }
        );
        assert_eq!(
            parse_wire_service_provider_update(&state_update("\"4\"")).expect_err("string sequence"),
            WireError::Invalid {
                description: "service state update",
            }
        );
        // JavaScript's Number.isInteger accepts whole-valued doubles.
        assert!(parse_wire_service_provider_update(&state_update("2.0")).is_ok());
        // There is no 2^53 or u64 admission cap: 2^64 is a finite whole-valued
        // double and the source accepts it.
        let huge = parse_wire_service_provider_update(&state_update("18446744073709551616"))
            .expect("2^64 sequence is source-admitted");
        let WireServiceProviderUpdate::State { sequence, .. } = &huge else {
            panic!("state variant");
        };
        assert_eq!(
            sequence.as_f64().to_bits(),
            18_446_744_073_709_551_616.0_f64.to_bits(),
        );

        // A member snapshot sequence starts at zero.
        let member_zero = json(
            r#"{"serviceId":"chat","mode":"singleton","instances":[{"members":[{"kind":"state","name":"text","sequence":0,"ops":[]}]}]}"#,
        );
        assert!(parse_service_subscription_snapshot(&member_zero).is_ok());
        let member_negative_zero = json(
            r#"{"serviceId":"chat","mode":"singleton","instances":[{"members":[{"kind":"state","name":"text","sequence":-0,"ops":[]}]}]}"#,
        );
        let negative_zero = parse_service_subscription_snapshot(&member_negative_zero)
            .expect("negative zero is a source-admitted member sequence");
        let ServiceMemberSnapshot::State { sequence, .. } = &negative_zero.instances[0].members[0] else {
            panic!("state member");
        };
        assert!(sequence.as_f64().is_sign_negative());
        let member_fractional = json(
            r#"{"serviceId":"chat","mode":"singleton","instances":[{"members":[{"kind":"state","name":"text","sequence":0.5,"ops":[]}]}]}"#,
        );
        assert_eq!(
            parse_service_subscription_snapshot(&member_fractional).expect_err("fractional member sequence"),
            WireError::Invalid {
                description: "service state snapshot",
            }
        );

        // Generations follow the same rule: 2^64 is admitted.
        let huge_generation = json(
            r#"{"serviceId":"chat","member":"post","args":[],"instance":{"key":"room","generation":18446744073709551616}}"#,
        );
        let call = parse_service_call(&huge_generation).expect("2^64 generation is source-admitted");
        assert_eq!(
            call.instance,
            Some(ServiceInstanceAddress {
                key: JsString::from_utf8("room"),
                generation: integer(18_446_744_073_709_551_616.0),
            })
        );
    }

    #[test]
    fn snapshots_encode_source_field_names() {
        let snapshot = ServiceSubscriptionSnapshot::<DeltaOp> {
            service_id: JsString::from_utf8("chat"),
            mode: ServiceMode::Singleton,
            instances: vec![ServiceInstanceSnapshot {
                instance: Some(ServiceInstanceAddress {
                    key: JsString::from_utf8("room"),
                    generation: integer(1.0),
                }),
                members: vec![
                    ServiceMemberSnapshot::Method {
                        name: JsString::from_utf8("post"),
                    },
                    ServiceMemberSnapshot::State {
                        name: JsString::from_utf8("counter"),
                        sequence: integer(2.0),
                        ops: vec![delta_op(r#"["s",["count"],1]"#)],
                    },
                ],
            }],
        };
        assert_eq!(
            snapshot.into_json(),
            json(
                r#"{"serviceId":"chat","mode":"singleton","instances":[{"instance":{"key":"room","generation":1},"members":[{"kind":"method","name":"post"},{"kind":"state","name":"counter","sequence":2,"ops":[["s",["count"],1]]}]}]}"#,
            ),
        );
        let update = ServiceProviderUpdate::<DeltaOp>::State {
            instance: None,
            member: JsString::from_utf8("counter"),
            sequence: integer(1.0),
            ops: vec![delta_op(r#"["s",["count"],1]"#)],
        };
        assert_eq!(
            update.into_json(),
            json(r#"{"type":"state","member":"counter","sequence":1,"ops":[["s",["count"],1]]}"#),
        );
        assert_eq!(
            WireServiceProviderUpdate::Unavailable.into_json(),
            json(r#"{"type":"unavailable"}"#),
        );
    }
}
