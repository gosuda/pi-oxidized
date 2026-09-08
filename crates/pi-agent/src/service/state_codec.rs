//! Stateful per-(instance, member) operation codecs for one service
//! subscription, plus consumer-side replicated state validation.
//!
//! Mirrors the TypeScript `packages/chord/src/services/state-codec.ts` and the
//! validation core of `services/state.ts` over the canonical value domain.
//! Dictionary ownership follows the provider update lifecycle: snapshots and
//! `replaced`/`unavailable` reset every codec, `spawned` registers codecs for
//! the arriving instance's state members, `closed` drops the instance's
//! codecs, `state` updates reuse the codec registered for their (instance,
//! member) pair, and method members are no-ops.  Encoding and decoding reuse
//! [`DeltaEncoder`]/[`DeltaDecoder`]; this module adds no dictionary algorithm
//! of its own.
//!
//! Registry keys and member names are [`JsString`] and generations and
//! sequences are [`JsInteger`], so source-admitted UTF-16 and unbounded
//! integral doubles behave exactly as they do for a JavaScript replica.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use thiserror::Error;

use super::delta::{apply_immutable, is_base, DeltaDecoder, DeltaEncoder, DeltaError, DeltaOp, WireOp};
use super::value::{js_number_to_string, JsInteger, JsString, JsonValue};
use super::wire::{
    ServiceInstanceAddress, ServiceInstanceSnapshot, ServiceMemberSnapshot, ServiceProviderUpdate,
    ServiceSubscriptionSnapshot, WireServiceInstanceSnapshot, WireServiceProviderUpdate, WireServiceSubscriptionSnapshot,
};

/// A stateful operation encoder for every replicated state in one service
/// subscription.
#[derive(Clone, Debug, Default)]
pub struct ServiceStateEncoder {
    codecs: StateCodecRegistry<DeltaEncoder>,
}

/// A stateful operation decoder for every replicated state in one service
/// subscription.
#[derive(Clone, Debug, Default)]
pub struct ServiceStateDecoder {
    codecs: StateCodecRegistry<DeltaDecoder>,
}

/// Failures raised while moving between the decoded and wire operation domains.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ServiceCodecError {
    /// Two state members in one instance claim the same (instance, member)
    /// codec.
    #[error("Duplicate service state {0}")]
    DuplicateServiceState(String),
    /// A state update references an (instance, member) pair without a
    /// registered codec.
    #[error("Unknown service state {0}")]
    UnknownServiceState(String),
    /// The underlying delta encoder or decoder rejected an operation batch.
    #[error(transparent)]
    Delta(#[from] DeltaError),
}

/// Consumer-side replicated state failures, mirroring the source replica's
/// error messages.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ServiceStateError {
    /// The hydration batch does not begin with a complete replacement.
    #[error("Replicated state snapshot is not a base operation batch")]
    NotBaseBatch,
    /// An incremental update arrived before hydration.
    #[error("Replicated state received an update before hydration")]
    UpdateBeforeHydration,
    /// An incremental update skipped a sequence.
    #[error("Replicated state update sequence has a gap")]
    SequenceGap,
    /// The delta applier rejected an operation batch.
    #[error(transparent)]
    Delta(#[from] DeltaError),
}

/// Registry of one operation codec per replicated (instance, member) state.
#[derive(Clone, Debug, Default)]
struct StateCodecRegistry<C> {
    entries: HashMap<(Option<ServiceInstanceAddress>, JsString), C>,
}

impl<C: Default> StateCodecRegistry<C> {
    /// Registers a fresh codec, rejecting a duplicate (instance, member) pair.
    fn add(
        &mut self,
        instance: Option<&ServiceInstanceAddress>,
        member: &JsString,
    ) -> Result<&mut C, ServiceCodecError> {
        let key = (instance.cloned(), member.clone());
        match self.entries.entry(key) {
            Entry::Occupied(_) => Err(ServiceCodecError::DuplicateServiceState(describe_state(instance, member))),
            Entry::Vacant(entry) => Ok(entry.insert(C::default())),
        }
    }

    /// Returns the registered codec for one (instance, member) pair.
    fn get(&mut self, instance: Option<&ServiceInstanceAddress>, member: &JsString) -> Result<&mut C, ServiceCodecError> {
        let key = (instance.cloned(), member.clone());
        self.entries
            .get_mut(&key)
            .ok_or_else(|| ServiceCodecError::UnknownServiceState(describe_state(instance, member)))
    }

    /// Drops every registered codec.
    fn reset(&mut self) {
        self.entries.clear();
    }

    /// Drops the codecs of one instance, leaving other instances untouched.
    fn remove_instance(&mut self, instance: &ServiceInstanceAddress) {
        self.entries
            .retain(|(entry_instance, _), _| entry_instance.as_ref() != Some(instance));
    }
}

impl ServiceStateEncoder {
    /// Creates an encoder with an empty codec registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Encodes one complete subscription snapshot, resetting dictionary
    /// ownership first.
    ///
    /// # Errors
    ///
    /// Returns `ServiceCodecError::DuplicateServiceState` if a snapshot
    /// contains a duplicate `(instance, member)` pair, or
    /// `ServiceCodecError::Delta` if the encoder rejects an operation batch.
    pub fn encode_snapshot(
        &mut self,
        snapshot: &ServiceSubscriptionSnapshot<DeltaOp>,
    ) -> Result<WireServiceSubscriptionSnapshot, ServiceCodecError> {
        self.codecs.reset();
        let mut instances = Vec::with_capacity(snapshot.instances.len());
        for instance in &snapshot.instances {
            instances.push(encode_instance(instance, &mut self.codecs)?);
        }
        Ok(WireServiceSubscriptionSnapshot {
            service_id: snapshot.service_id.clone(),
            mode: snapshot.mode,
            instances,
        })
    }

    /// Encodes one provider update, applying the dictionary lifecycle rules.
    ///
    /// # Errors
    ///
    /// Returns `ServiceCodecError::UnknownServiceState` for a state update
    /// whose `(instance, member)` pair has no registered encoder, or
    /// `ServiceCodecError::Delta` if the encoder rejects an operation batch.
    pub fn encode_update(
        &mut self,
        update: &ServiceProviderUpdate<DeltaOp>,
    ) -> Result<WireServiceProviderUpdate, ServiceCodecError> {
        match update {
            ServiceProviderUpdate::State {
                instance,
                member,
                sequence,
                ops,
            } => {
                let ops = self.codecs.get(instance.as_ref(), member)?.encode(ops)?;
                Ok(ServiceProviderUpdate::State {
                    instance: instance.clone(),
                    member: member.clone(),
                    sequence: *sequence,
                    ops,
                })
            }
            ServiceProviderUpdate::Unavailable => {
                self.codecs.reset();
                Ok(ServiceProviderUpdate::Unavailable)
            }
            ServiceProviderUpdate::Replaced { snapshot } => {
                self.codecs.reset();
                Ok(ServiceProviderUpdate::Replaced {
                    snapshot: encode_instance(snapshot, &mut self.codecs)?,
                })
            }
            ServiceProviderUpdate::Spawned { instance } => Ok(ServiceProviderUpdate::Spawned {
                instance: encode_instance(instance, &mut self.codecs)?,
            }),
            ServiceProviderUpdate::Closed { instance } => {
                self.codecs.remove_instance(instance);
                Ok(ServiceProviderUpdate::Closed {
                    instance: instance.clone(),
                })
            }
        }
    }
}

impl ServiceStateDecoder {
    /// Creates a decoder with an empty codec registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes one complete subscription snapshot, resetting dictionary
    /// ownership first.
    ///
    /// # Errors
    ///
    /// Returns `ServiceCodecError::DuplicateServiceState` for a snapshot
    /// containing a duplicate `(instance, member)` pair, or
    /// `ServiceCodecError::Delta` if the decoder rejects an operation batch.
    pub fn decode_snapshot(
        &mut self,
        snapshot: &WireServiceSubscriptionSnapshot,
    ) -> Result<ServiceSubscriptionSnapshot<DeltaOp>, ServiceCodecError> {
        self.codecs.reset();
        let mut instances = Vec::with_capacity(snapshot.instances.len());
        for instance in &snapshot.instances {
            instances.push(decode_instance(instance, &mut self.codecs)?);
        }
        Ok(ServiceSubscriptionSnapshot {
            service_id: snapshot.service_id.clone(),
            mode: snapshot.mode,
            instances,
        })
    }

    /// Decodes one provider update, applying the dictionary lifecycle rules.
    ///
    /// # Errors
    ///
    /// Returns `ServiceCodecError::UnknownServiceState` for a state update
    /// whose `(instance, member)` pair has no registered decoder, or
    /// `ServiceCodecError::Delta` if the decoder rejects an operation batch.
    pub fn decode_update(
        &mut self,
        update: &WireServiceProviderUpdate,
    ) -> Result<ServiceProviderUpdate<DeltaOp>, ServiceCodecError> {
        match update {
            ServiceProviderUpdate::State {
                instance,
                member,
                sequence,
                ops,
            } => {
                let ops = self.codecs.get(instance.as_ref(), member)?.decode(ops)?;
                Ok(ServiceProviderUpdate::State {
                    instance: instance.clone(),
                    member: member.clone(),
                    sequence: *sequence,
                    ops,
                })
            }
            ServiceProviderUpdate::Unavailable => {
                self.codecs.reset();
                Ok(ServiceProviderUpdate::Unavailable)
            }
            ServiceProviderUpdate::Replaced { snapshot } => {
                self.codecs.reset();
                Ok(ServiceProviderUpdate::Replaced {
                    snapshot: decode_instance(snapshot, &mut self.codecs)?,
                })
            }
            ServiceProviderUpdate::Spawned { instance } => Ok(ServiceProviderUpdate::Spawned {
                instance: decode_instance(instance, &mut self.codecs)?,
            }),
            ServiceProviderUpdate::Closed { instance } => {
                self.codecs.remove_instance(instance);
                Ok(ServiceProviderUpdate::Closed {
                    instance: instance.clone(),
                })
            }
        }
    }
}

/// Creates a state encoder with an empty per-state dictionary registry.
#[must_use]
pub fn create_service_state_encoder() -> ServiceStateEncoder {
    ServiceStateEncoder::new()
}

/// Creates a state decoder with an empty per-state dictionary registry.
#[must_use]
pub fn create_service_state_decoder() -> ServiceStateDecoder {
    ServiceStateDecoder::new()
}

fn encode_instance(
    instance: &ServiceInstanceSnapshot<DeltaOp>,
    codecs: &mut StateCodecRegistry<DeltaEncoder>,
) -> Result<ServiceInstanceSnapshot<WireOp>, ServiceCodecError> {
    let mut members = Vec::with_capacity(instance.members.len());
    for member in &instance.members {
        members.push(match member {
            ServiceMemberSnapshot::Method { name } => ServiceMemberSnapshot::Method { name: name.clone() },
            ServiceMemberSnapshot::State { name, sequence, ops } => ServiceMemberSnapshot::State {
                name: name.clone(),
                sequence: *sequence,
                ops: codecs.add(instance.instance.as_ref(), name)?.encode(ops)?,
            },
        });
    }
    Ok(ServiceInstanceSnapshot {
        instance: instance.instance.clone(),
        members,
    })
}

fn decode_instance(
    instance: &WireServiceInstanceSnapshot,
    codecs: &mut StateCodecRegistry<DeltaDecoder>,
) -> Result<ServiceInstanceSnapshot<DeltaOp>, ServiceCodecError> {
    let mut members = Vec::with_capacity(instance.members.len());
    for member in &instance.members {
        members.push(match member {
            ServiceMemberSnapshot::Method { name } => ServiceMemberSnapshot::Method { name: name.clone() },
            ServiceMemberSnapshot::State { name, sequence, ops } => ServiceMemberSnapshot::State {
                name: name.clone(),
                sequence: *sequence,
                ops: codecs.add(instance.instance.as_ref(), name)?.decode(ops)?,
            },
        });
    }
    Ok(ServiceInstanceSnapshot {
        instance: instance.instance.clone(),
        members,
    })
}

/// Describes one replicated state for an error message, mirroring the source's
/// `member` / `key@generation.member` shapes.  Names holding unpaired
/// surrogates render with replacement characters because a Rust `String`
/// cannot hold them; the registry keys themselves stay lossless.
fn describe_state(instance: Option<&ServiceInstanceAddress>, member: &JsString) -> String {
    let member = String::from_utf16_lossy(member.as_utf16());
    match instance {
        None => member,
        Some(address) => format!(
            "{}@{}.{}",
            String::from_utf16_lossy(address.key.as_utf16()),
            js_number_to_string(address.generation.as_f64()),
            member,
        ),
    }
}

/// A cold consumer-side replicated state with source-exact base-batch and
/// sequence validation.  Hydration requires a batch beginning with a complete
/// replacement; updates require the exact previous sequence plus one — in
/// binary64 arithmetic, matching the source's `sequence !== previous + 1` —
/// and a gap clears the replica.
#[derive(Clone, Debug, Default)]
pub struct ReplicatedStateReplica {
    value: Option<JsonValue>,
    sequence: Option<JsInteger>,
}

impl ReplicatedStateReplica {
    /// Creates a cold replica awaiting hydration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current value, or `None` until hydration and after a gap.
    #[must_use]
    pub fn value(&self) -> Option<&JsonValue> {
        self.value.as_ref()
    }

    /// Returns the last applied sequence, or `None` until hydration.
    #[must_use]
    pub const fn sequence(&self) -> Option<JsInteger> {
        self.sequence
    }

    /// Hydrates from a complete base batch.
    ///
    /// # Errors
    ///
    /// Returns `ServiceStateError::NotBaseBatch` if the operation batch does
    /// not begin with a complete replacement, or `ServiceStateError::Delta`
    /// if the applier rejects the batch.
    pub fn hydrate(&mut self, sequence: JsInteger, ops: &[DeltaOp]) -> Result<(), ServiceStateError> {
        if !is_base(ops) {
            return Err(ServiceStateError::NotBaseBatch);
        }
        self.value = apply_immutable(None, ops)?;
        self.sequence = Some(sequence);
        Ok(())
    }

    /// Applies one incremental batch at exactly the next sequence.
    ///
    /// # Errors
    ///
    /// Returns `ServiceStateError::UpdateBeforeHydration` if the replica has
    /// not been hydrated, `ServiceStateError::SequenceGap` if the incoming
    /// sequence is not the previous sequence plus one, or
    /// `ServiceStateError::Delta` if the applier rejects the batch.
    pub fn update(&mut self, sequence: JsInteger, ops: &[DeltaOp]) -> Result<(), ServiceStateError> {
        let (Some(previous), Some(_)) = (self.sequence, self.value.as_ref()) else {
            return Err(ServiceStateError::UpdateBeforeHydration);
        };
        if previous.next() != sequence {
            self.clear();
            return Err(ServiceStateError::SequenceGap);
        }
        self.value = apply_immutable(self.value.as_ref(), ops)?;
        self.sequence = Some(sequence);
        Ok(())
    }

    /// Discards the hydrated value, matching the source replica's gap recovery.
    pub fn clear(&mut self) {
        self.value = None;
        self.sequence = None;
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "test fixtures and assertions use expect and panic for irrecoverable failures"
)]
mod tests {
    use super::*;
    use crate::service::delta::{PathRef, PathSegment, StatePath};
    use crate::service::value::parse_json;
    use crate::service::wire::{
        parse_wire_service_provider_update, parse_wire_service_subscription_snapshot, ServiceMode,
        WireServiceMemberSnapshot,
    };

    fn json(text: &str) -> JsonValue {
        parse_json(text).expect("test JSON parses")
    }

    fn integer(value: f64) -> JsInteger {
        JsInteger::new(value).expect("test integer")
    }

    fn js(text: &str) -> JsString {
        JsString::from_utf8(text)
    }

    fn delta_op(text: &str) -> DeltaOp {
        DeltaOp::from_json(&json(text)).expect("test op parses")
    }

    fn address(key: &str, generation: f64) -> ServiceInstanceAddress {
        ServiceInstanceAddress {
            key: js(key),
            generation: integer(generation),
        }
    }

    /// Reads one field from a canonical object produced by `into_json`.
    fn object_field<'a>(value: &'a JsonValue, name: &str) -> &'a JsonValue {
        match value {
            JsonValue::Object(map) => map.get(&JsString::from_utf8(name)).expect("field present"),
            _ => panic!("expected object"),
        }
    }

    fn counter_snapshot() -> ServiceSubscriptionSnapshot<DeltaOp> {
        ServiceSubscriptionSnapshot {
            service_id: js("chat"),
            mode: ServiceMode::Singleton,
            instances: vec![ServiceInstanceSnapshot {
                instance: Some(address("room", 1.0)),
                members: vec![
                    ServiceMemberSnapshot::Method { name: js("post") },
                    ServiceMemberSnapshot::State {
                        name: js("counter"),
                        sequence: integer(3.0),
                        ops: vec![delta_op(r#"["s",["count"],5]"#)],
                    },
                ],
            }],
        }
    }

    fn state_update(
        instance: Option<ServiceInstanceAddress>,
        member: &str,
        sequence: f64,
        ops: Vec<DeltaOp>,
    ) -> ServiceProviderUpdate<DeltaOp> {
        ServiceProviderUpdate::State {
            instance,
            member: js(member),
            sequence: integer(sequence),
            ops,
        }
    }

    fn counter_instance(key: &str, generation: f64, sequence: f64) -> ServiceInstanceSnapshot<DeltaOp> {
        ServiceInstanceSnapshot {
            instance: Some(address(key, generation)),
            members: vec![ServiceMemberSnapshot::State {
                name: js("counter"),
                sequence: integer(sequence),
                ops: vec![delta_op(r#"["s",["count"],1]"#)],
            }],
        }
    }

    #[test]
    fn snapshot_round_trips_through_the_wire_grammar() {
        let mut encoder = ServiceStateEncoder::new();
        let wire = encoder.encode_snapshot(&counter_snapshot()).expect("encode snapshot");
        // First use of a path stays inline; the dictionary only interns on
        // the second use.
        assert_eq!(
            wire.clone().into_json(),
            json(
                r#"{"serviceId":"chat","mode":"singleton","instances":[{"instance":{"key":"room","generation":1},"members":[{"kind":"method","name":"post"},{"kind":"state","name":"counter","sequence":3,"ops":[["s",["count"],5]]}]}]}"#,
            ),
        );
        let parsed = parse_wire_service_subscription_snapshot(&wire.into_json()).expect("parse wire snapshot");
        let mut decoder = ServiceStateDecoder::new();
        let round_tripped = decoder.decode_snapshot(&parsed).expect("decode snapshot");
        assert_eq!(round_tripped, counter_snapshot());
    }

    #[test]
    fn state_updates_reuse_their_registered_codec() {
        // Three independent replicated states: `counter` and `label` on
        // `room@1`, and `counter` again on `room@2`. The states whose first
        // update must still encode its path inline are registered with empty
        // op batches, so their codecs exist without having interned anything
        // yet.
        let snapshot = ServiceSubscriptionSnapshot {
            service_id: js("chat"),
            mode: ServiceMode::Keyed,
            instances: vec![
                ServiceInstanceSnapshot {
                    instance: Some(address("room", 1.0)),
                    members: vec![
                        ServiceMemberSnapshot::State {
                            name: js("counter"),
                            sequence: integer(3.0),
                            ops: vec![delta_op(r#"["s",["count"],5]"#)],
                        },
                        ServiceMemberSnapshot::State {
                            name: js("label"),
                            sequence: integer(0.0),
                            ops: vec![],
                        },
                    ],
                },
                ServiceInstanceSnapshot {
                    instance: Some(address("room", 2.0)),
                    members: vec![ServiceMemberSnapshot::State {
                        name: js("counter"),
                        sequence: integer(0.0),
                        ops: vec![],
                    }],
                },
            ],
        };
        let mut encoder = ServiceStateEncoder::new();
        encoder.encode_snapshot(&snapshot).expect("snapshot");
        let first = encoder
            .encode_update(&state_update(Some(address("room", 1.0)), "counter", 4.0, vec![
                delta_op(r#"["s",["count"],6]"#),
            ]))
            .expect("first update");
        // The second use of the "count" path interns it as dictionary id 0.
        assert_eq!(
            object_field(&first.into_json(), "ops"),
            &json(r##"[["#",0,["count"]],["s",0,6]]"##),
        );
        // A different member owns a separate codec, so its path stays inline.
        let other_member = encoder
            .encode_update(&state_update(Some(address("room", 1.0)), "label", 1.0, vec![
                delta_op(r#"["s",["x"],1]"#),
            ]))
            .expect("other member");
        assert_eq!(
            object_field(&other_member.into_json(), "ops"),
            &json(r#"[["s",["x"],1]]"#),
        );
        // The same member name under another instance owns another codec.
        let other_instance = encoder
            .encode_update(&state_update(Some(address("room", 2.0)), "counter", 1.0, vec![
                delta_op(r#"["s",["count"],1]"#),
            ]))
            .expect("other instance");
        assert_eq!(
            object_field(&other_instance.into_json(), "ops"),
            &json(r#"[["s",["count"],1]]"#),
        );
    }

    #[test]
    fn state_registry_preserves_lone_surrogate_member_names() {
        let member = JsString::from_utf16(vec![0xD800]);
        let snapshot = ServiceSubscriptionSnapshot {
            service_id: js("chat"),
            mode: ServiceMode::Singleton,
            instances: vec![ServiceInstanceSnapshot {
                instance: None,
                members: vec![ServiceMemberSnapshot::State {
                    name: member.clone(),
                    sequence: integer(0.0),
                    ops: vec![],
                }],
            }],
        };
        let mut encoder = ServiceStateEncoder::new();
        encoder.encode_snapshot(&snapshot).expect("register UTF-16 state");
        let update = ServiceProviderUpdate::State {
            instance: None,
            member,
            sequence: integer(1.0),
            ops: vec![],
        };
        let wire_update = encoder.encode_update(&update).expect("lookup UTF-16 state");
        let ServiceProviderUpdate::State { member, ops, .. } = wire_update else {
            panic!("state update");
        };
        assert_eq!(member.as_utf16(), &[0xD800]);
        assert!(ops.is_empty());
    }

    #[test]
    fn unavailable_resets_the_dictionary() {
        let mut encoder = ServiceStateEncoder::new();
        encoder.encode_snapshot(&counter_snapshot()).expect("snapshot");
        encoder
            .encode_update(&ServiceProviderUpdate::Unavailable)
            .expect("unavailable");
        // After the reset every previously registered state is unknown,
        // exactly like the source registry.
        assert_eq!(
            encoder
                .encode_update(&state_update(Some(address("room", 1.0)), "counter", 4.0, vec![
                    delta_op(r#"["s",["count"],6]"#),
                ]))
                .expect_err("reset codecs are unknown"),
            ServiceCodecError::UnknownServiceState("room@1.counter".to_owned())
        );
    }

    #[test]
    fn replaced_resets_and_reregisters() {
        let mut encoder = ServiceStateEncoder::new();
        encoder.encode_snapshot(&counter_snapshot()).expect("snapshot");
        let snapshot = counter_snapshot();
        let replacement = snapshot.instances.into_iter().next().expect("instance");
        let replaced = encoder
            .encode_update(&ServiceProviderUpdate::Replaced {
                snapshot: replacement,
            })
            .expect("replaced");
        let replaced_json = replaced.into_json();
        assert_eq!(
            object_field(object_field(&replaced_json, "snapshot"), "members"),
            &json(r#"[{"kind":"method","name":"post"},{"kind":"state","name":"counter","sequence":3,"ops":[["s",["count"],5]]}]"#),
        );
        let after = encoder
            .encode_update(&state_update(Some(address("room", 1.0)), "counter", 4.0, vec![
                delta_op(r#"["s",["count"],6]"#),
            ]))
            .expect("state after replaced");
        assert_eq!(
            object_field(&after.into_json(), "ops"),
            &json(r##"[["#",0,["count"]],["s",0,6]]"##),
        );
    }

    #[test]
    fn spawned_adds_and_closed_removes() {
        let mut encoder = ServiceStateEncoder::new();
        encoder.encode_snapshot(&counter_snapshot()).expect("snapshot");
        encoder
            .encode_update(&ServiceProviderUpdate::Spawned {
                instance: counter_instance("lobby", 2.0, 1.0),
            })
            .expect("spawned");
        let lobby = encoder
            .encode_update(&state_update(Some(address("lobby", 2.0)), "counter", 2.0, vec![
                delta_op(r#"["s",["count"],2]"#),
            ]))
            .expect("lobby state");
        assert_eq!(
            object_field(&lobby.into_json(), "ops"),
            &json(r##"[["#",0,["count"]],["s",0,2]]"##),
        );
        encoder
            .encode_update(&ServiceProviderUpdate::Closed {
                instance: address("lobby", 2.0),
            })
            .expect("closed");
        assert_eq!(
            encoder
                .encode_update(&state_update(Some(address("lobby", 2.0)), "counter", 3.0, vec![
                    delta_op(r#"["s",["count"],3]"#),
                ]))
                .expect_err("closed codec removed"),
            ServiceCodecError::UnknownServiceState("lobby@2.counter".to_owned())
        );
        // A different instance under the same key keeps its own codec.
        let room = encoder
            .encode_update(&state_update(Some(address("room", 1.0)), "counter", 4.0, vec![
                delta_op(r#"["s",["count"],6]"#),
            ]))
            .expect("room state survives close");
        assert_eq!(
            object_field(&room.into_json(), "ops"),
            &json(r##"[["#",0,["count"]],["s",0,6]]"##),
        );
    }

    #[test]
    fn method_members_do_not_touch_the_dictionary() {
        let mut encoder = ServiceStateEncoder::new();
        encoder.encode_snapshot(&counter_snapshot()).expect("snapshot");
        encoder
            .encode_update(&ServiceProviderUpdate::Spawned {
                instance: ServiceInstanceSnapshot {
                    instance: Some(address("room", 3.0)),
                    members: vec![ServiceMemberSnapshot::Method { name: js("post") }],
                },
            })
            .expect("method-only spawned");
        // The counter codec survived the method-only spawn untouched, so its
        // path still resolves through the dictionary.
        let after = encoder
            .encode_update(&state_update(Some(address("room", 1.0)), "counter", 4.0, vec![
                delta_op(r#"["s",["count"],6]"#),
            ]))
            .expect("counter state");
        assert_eq!(
            object_field(&after.into_json(), "ops"),
            &json(r##"[["#",0,["count"]],["s",0,6]]"##),
        );
    }

    #[test]
    fn duplicate_state_members_are_rejected() {
        let mut encoder = ServiceStateEncoder::new();
        let duplicated = ServiceSubscriptionSnapshot {
            service_id: js("chat"),
            mode: ServiceMode::Singleton,
            instances: vec![ServiceInstanceSnapshot {
                instance: Some(address("room", 1.0)),
                members: vec![
                    ServiceMemberSnapshot::State {
                        name: js("counter"),
                        sequence: integer(1.0),
                        ops: vec![],
                    },
                    ServiceMemberSnapshot::State {
                        name: js("counter"),
                        sequence: integer(1.0),
                        ops: vec![],
                    },
                ],
            }],
        };
        assert_eq!(
            encoder.encode_snapshot(&duplicated).expect_err("duplicate member"),
            ServiceCodecError::DuplicateServiceState("room@1.counter".to_owned())
        );
    }

    #[test]
    fn decoder_mirrors_dictionary_ownership() {
        let mut encoder = ServiceStateEncoder::new();
        let mut decoder = ServiceStateDecoder::new();
        let wire_snapshot = encoder.encode_snapshot(&counter_snapshot()).expect("snapshot");
        let decoded_snapshot = decoder.decode_snapshot(&wire_snapshot).expect("decode snapshot");
        assert_eq!(decoded_snapshot, counter_snapshot());
        let wire_update = encoder
            .encode_update(&state_update(Some(address("room", 1.0)), "counter", 4.0, vec![
                delta_op(r#"["s",["count"],6]"#),
            ]))
            .expect("encode update");
        let wire_value = wire_update.into_json();
        let parsed_update = parse_wire_service_provider_update(&wire_value).expect("parse wire update");
        let decoded_update = decoder.decode_update(&parsed_update).expect("decode update");
        assert_eq!(
            decoded_update,
            state_update(Some(address("room", 1.0)), "counter", 4.0, vec![
                delta_op(r#"["s",["count"],6]"#),
            ])
        );
        decoder
            .decode_update(&WireServiceProviderUpdate::Closed {
                instance: address("room", 1.0),
            })
            .expect("decode closed");
        assert_eq!(
            decoder.decode_update(&parsed_update).expect_err("closed codec removed"),
            ServiceCodecError::UnknownServiceState("room@1.counter".to_owned())
        );
        decoder
            .decode_update(&WireServiceProviderUpdate::Spawned {
                instance: WireServiceInstanceSnapshot {
                    instance: Some(address("room", 1.0)),
                    members: vec![WireServiceMemberSnapshot::State {
                        name: js("counter"),
                        sequence: integer(1.0),
                        ops: vec![WireOp::from_json(&json(r#"["r",{"count":0}]"#)).expect("wire op")],
                    }],
                },
            })
            .expect("decode spawned");
        // After re-registration, an inline path is accepted by the fresh
        // codec; the old dictionary id is no longer required.
        let post_spawn_update = WireServiceProviderUpdate::State {
            instance: Some(address("room", 1.0)),
            member: js("counter"),
            sequence: integer(2.0),
            ops: vec![WireOp::Set(
                PathRef::Inline(StatePath::new([PathSegment::key("count")]).expect("path")),
                JsonValue::Number(1.0),
            )],
        };
        let hydrated = decoder
            .decode_update(&post_spawn_update)
            .expect("decode after spawned");
        assert_eq!(
            hydrated,
            state_update(Some(address("room", 1.0)), "counter", 2.0, vec![
                delta_op(r#"["s",["count"],1]"#),
            ])
        );
    }

    #[test]
    fn replica_requires_base_batch_for_hydration() {
        let mut replica = ReplicatedStateReplica::new();
        let incremental = vec![delta_op(r#"["s",["count"],1]"#)];
        assert_eq!(
            replica
                .hydrate(integer(1.0), &incremental)
                .expect_err("hydration needs a base batch"),
            ServiceStateError::NotBaseBatch
        );
        assert_eq!(replica.value(), None);
        assert_eq!(replica.sequence(), None);
    }

    #[test]
    fn replica_rejects_updates_before_hydration() {
        let mut replica = ReplicatedStateReplica::new();
        assert_eq!(
            replica
                .update(integer(1.0), &[delta_op(r#"["s",["count"],1]"#)])
                .expect_err("cold replica"),
            ServiceStateError::UpdateBeforeHydration
        );
    }

    #[test]
    fn replica_applies_exact_sequence_and_clears_on_gap() {
        let base = vec![delta_op(r#"["r",{"count":0}]"#)];
        let mut replica = ReplicatedStateReplica::new();
        replica.hydrate(integer(4.0), &base).expect("hydrate");
        assert_eq!(replica.sequence(), Some(integer(4.0)));
        let expected = json(r#"{"count":0}"#);
        assert_eq!(replica.value(), Some(&expected));
        assert_eq!(
            replica
                .update(integer(6.0), &[delta_op(r#"["s",["count"],1]"#)])
                .expect_err("sequence gap"),
            ServiceStateError::SequenceGap
        );
        // The gap clears the replica exactly like the source.
        assert_eq!(replica.sequence(), None);
        assert_eq!(replica.value(), None);
        replica.hydrate(integer(4.0), &base).expect("rehydrate");
        replica
            .update(integer(5.0), &[delta_op(r#"["s",["count"],1]"#)])
            .expect("exact sequence");
        assert_eq!(replica.sequence(), Some(integer(5.0)));
        let expected = json(r#"{"count":1}"#);
        assert_eq!(replica.value(), Some(&expected));
    }

    #[test]
    fn replica_sequence_sticks_at_binary64_limit() {
        // There is no u64 or 2^53 admission cap: hydration at 2^53 is legal.
        // At that magnitude `previous + 1` rounds back to `previous` in
        // binary64, so the source's `sequence !== previous + 1` check accepts
        // a repeated sequence — and so does the replica.
        let base = vec![delta_op(r#"["r",{"count":0}]"#)];
        let mut replica = ReplicatedStateReplica::new();
        let stuck = integer(9_007_199_254_740_992.0); // 2^53
        replica.hydrate(stuck, &base).expect("hydrate at 2^53");
        replica.update(stuck, &[]).expect("2^53 + 1 rounds to 2^53");
        assert_eq!(replica.sequence(), Some(stuck));
        // A genuinely different sequence still gaps and clears.
        assert_eq!(
            replica
                .update(integer(9_007_199_254_740_994.0), &[])
                .expect_err("2^53 + 2 is a gap"),
            ServiceStateError::SequenceGap
        );
        assert_eq!(replica.sequence(), None);
        assert_eq!(replica.value(), None);
    }

    #[test]
    fn replica_hydration_validates_operations() {
        let mut replica = ReplicatedStateReplica::new();
        // A valid path that would create an out-of-bounds array entry is
        // rejected by the applier before the replica stores the snapshot.
        let invalid_batch = vec![
            DeltaOp::Replace(json(r#"{"xs":[0]}"#)),
            DeltaOp::Set(
                StatePath::new([PathSegment::key("xs"), PathSegment::index(integer(5.0))])
                    .expect("path"),
                JsonValue::Number(1.0),
            ),
        ];
        assert!(matches!(
            replica.hydrate(integer(1.0), &invalid_batch),
            Err(ServiceStateError::Delta(_))
        ));
        assert_eq!(replica.value(), None);
        assert_eq!(replica.sequence(), None);
    }
}
