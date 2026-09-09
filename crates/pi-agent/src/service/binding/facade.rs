use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use futures::future::{BoxFuture, FutureExt, ready};

use crate::context::Context;

use super::super::delta::DeltaOp;
use super::super::error::{RemoteServiceErrorCode, ServiceError};
use super::super::replicated::{ReplicatedState, ReplicatedStateListener};
use super::super::transport::RemoteServiceTransport;
use super::super::value::{JsInteger, JsString, JsonValue, is_json_value};
use super::super::wire::{
    ServiceCall, ServiceInstanceAddress, ServiceInstanceSnapshot, ServiceMemberSnapshot,
};
use super::lifecycle::{ErrorReporter, Lifecycle, await_with_lifetime};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MemberKind {
    Method,
    State,
}

impl MemberKind {
    fn from_snapshot<Op>(member: &ServiceMemberSnapshot<Op>) -> Self
    where
        Op: super::super::wire::ServiceOp,
    {
        match member {
            ServiceMemberSnapshot::Method { .. } => Self::Method,
            ServiceMemberSnapshot::State { .. } => Self::State,
        }
    }
}

/// A stable native facade for one singleton or keyed service instance.
#[derive(Clone)]
pub struct RemoteServiceFacade {
    pub(crate) inner: Arc<FacadeInner>,
}

pub(crate) struct FacadeInner {
    pub(crate) service_id: JsString,
    pub(crate) address: Option<ServiceInstanceAddress>,
    pub(crate) transport: Arc<dyn RemoteServiceTransport>,
    pub(crate) lifecycle: Arc<Lifecycle>,
    active: AtomicBool,
    members: Mutex<BTreeMap<JsString, Arc<RemoteServiceMember>>>,
    descriptions: Mutex<BTreeMap<JsString, MemberKind>>,
}

impl FacadeInner {
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
            && self.lifecycle.is_bound()
            && !self.lifecycle.is_disposed()
    }

    pub(crate) fn assert_live(&self) -> Result<(), ServiceError> {
        if !self.is_active() {
            return Err(ServiceError::remote(
                RemoteServiceErrorCode::ServiceStaleInstance,
                format!(
                    "Remote service {} binding is closed",
                    display_js(&self.service_id)
                ),
            ));
        }
        Ok(())
    }
}

impl RemoteServiceFacade {
    pub(crate) fn new(
        service_id: JsString,
        address: Option<ServiceInstanceAddress>,
        transport: Arc<dyn RemoteServiceTransport>,
        lifecycle: Arc<Lifecycle>,
    ) -> Self {
        Self {
            inner: Arc::new(FacadeInner {
                service_id,
                address,
                transport,
                lifecycle,
                active: AtomicBool::new(true),
                members: Mutex::new(BTreeMap::new()),
                descriptions: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Stable service identifier carried by this facade.
    #[must_use]
    pub fn service_id(&self) -> &JsString {
        &self.inner.service_id
    }

    /// Keyed address carried by this facade, or `None` for a singleton.
    #[must_use]
    pub fn address(&self) -> Option<&ServiceInstanceAddress> {
        self.inner.address.as_ref()
    }

    /// Returns a stable member handle. Handles remain usable across singleton replacement.
    ///
    /// # Errors
    /// Returns an error if access is denied or if the member's known kind conflicts with its previous use.
    pub fn member(
        &self,
        name: impl Into<JsString>,
    ) -> Result<Arc<RemoteServiceMember>, ServiceError> {
        self.inner.lifecycle.assert_access()?;
        let name = name.into();
        let mut members = lock(&self.inner.members);
        if let Some(member) = members.get(&name) {
            return Ok(Arc::clone(member));
        }
        let member = Arc::new(RemoteServiceMember::new(
            Arc::downgrade(&self.inner),
            name.clone(),
            Arc::clone(&self.inner.lifecycle.report_error),
        ));
        if let Some(kind) = lock(&self.inner.descriptions).get(&name).copied() {
            member.set_description(kind)?;
        }
        members.insert(name, Arc::clone(&member));
        Ok(member)
    }

    /// Invokes a method member with strict-JSON arguments.
    pub fn invoke(
        &self,
        member: impl Into<JsString>,
        args: Vec<JsonValue>,
        context: Context,
    ) -> BoxFuture<'static, Result<Option<JsonValue>, ServiceError>> {
        match self.member(member) {
            Ok(member) => member.invoke(args, context),
            Err(error) => ready(Err(error)).boxed(),
        }
    }

    /// Returns a replicated state member handle, cold until its first snapshot.
    ///
    /// # Errors
    /// Returns an error if access is denied or if the member is not a state member.
    pub fn state(&self, member: impl Into<JsString>) -> Result<Arc<ReplicatedState>, ServiceError> {
        self.member(member)?.state()
    }

    /// Installs a complete snapshot before the subscription is activated.
    ///
    /// # Errors
    /// Returns an error if the snapshot address or member descriptions do not match the
    /// facade, if a member slot cannot be created, or if state hydration fails.
    pub(crate) fn install(
        &self,
        snapshot: &ServiceInstanceSnapshot<DeltaOp>,
        context: &Context,
    ) -> Result<(), ServiceError> {
        if !same_address(snapshot.instance.as_ref(), self.inner.address.as_ref()) {
            return Err(ServiceError::local(
                "Remote service snapshot has the wrong address",
            ));
        }
        let members = validate_members(&snapshot.members)?;
        let existing: Vec<(JsString, Arc<RemoteServiceMember>)> = {
            let slots = lock(&self.inner.members);
            for name in slots.keys() {
                if !members.iter().any(|(candidate, _, _)| candidate == name) {
                    return Err(ServiceError::remote(
                        RemoteServiceErrorCode::ServiceMemberNotFound,
                        format!(
                            "Unknown remote service member {}.{}",
                            display_js(&self.inner.service_id),
                            display_js(name),
                        ),
                    ));
                }
            }
            slots
                .iter()
                .map(|(name, member)| (name.clone(), Arc::clone(member)))
                .collect()
        };

        {
            let mut descriptions = lock(&self.inner.descriptions);
            descriptions.clear();
            for (name, kind, _) in &members {
                descriptions.insert(name.clone(), *kind);
            }
        }

        for (name, kind, snapshot_member) in members {
            let slot = existing
                .iter()
                .find_map(|(candidate, member)| (candidate == &name).then(|| Arc::clone(member)))
                .or_else(|| {
                    let mut slots = lock(&self.inner.members);
                    if let Some(member) = slots.get(&name) {
                        return Some(Arc::clone(member));
                    }
                    let member = Arc::new(RemoteServiceMember::new(
                        Arc::downgrade(&self.inner),
                        name.clone(),
                        Arc::clone(&self.inner.lifecycle.report_error),
                    ));
                    slots.insert(name.clone(), Arc::clone(&member));
                    Some(member)
                })
                .ok_or_else(|| {
                    ServiceError::internal("Remote service member slot was not created")
                })?;
            slot.set_description(kind)?;
            if let ServiceMemberSnapshot::State { sequence, ops, .. } = snapshot_member {
                slot.hydrate(*sequence, ops, context)?;
            }
        }
        self.inner.active.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn update(
        &self,
        member: &JsString,
        sequence: JsInteger,
        ops: &[DeltaOp],
        context: &Context,
    ) -> Result<(), ServiceError> {
        let kind = lock(&self.inner.descriptions).get(member).copied();
        if kind != Some(MemberKind::State) {
            return Err(ServiceError::local(format!(
                "Remote service update targets non-state member {}.{}",
                display_js(&self.inner.service_id),
                display_js(member),
            )));
        }
        self.member(member.clone())?.update(sequence, ops, context)
    }

    pub(crate) fn clear(&self) {
        let members: Vec<Arc<RemoteServiceMember>> =
            lock(&self.inner.members).values().cloned().collect();
        for member in members {
            member.clear();
        }
    }

    pub(crate) fn deactivate(&self) {
        self.inner.active.store(false, Ordering::Release);
        self.clear();
    }
}

/// One explicitly addressed remote member. The same handle is reused by a stable facade.
pub struct RemoteServiceMember {
    facade: Weak<FacadeInner>,
    name: JsString,
    kind: Mutex<MemberState>,
    state: Arc<ReplicatedState>,
}

struct MemberState {
    actual: Option<MemberKind>,
    expected: Option<MemberKind>,
}

impl RemoteServiceMember {
    fn new(facade: Weak<FacadeInner>, name: JsString, report_error: ErrorReporter) -> Self {
        Self {
            facade,
            name,
            kind: Mutex::new(MemberState {
                actual: None,
                expected: None,
            }),
            state: Arc::new(ReplicatedState::new(report_error)),
        }
    }

    /// Member name.
    #[must_use]
    pub fn name(&self) -> &JsString {
        &self.name
    }

    /// Invokes this member as a remote method.
    pub fn invoke(
        self: &Arc<Self>,
        args: Vec<JsonValue>,
        context: Context,
    ) -> BoxFuture<'static, Result<Option<JsonValue>, ServiceError>> {
        let Some(facade) = self.facade.upgrade() else {
            return ready(Err(ServiceError::disposed("Remote service facade is gone"))).boxed();
        };
        if let Err(error) = facade.lifecycle.assert_access() {
            return ready(Err(error)).boxed();
        }
        if let Err(error) = self.expect(MemberKind::Method) {
            return ready(Err(error)).boxed();
        }
        if let Err(error) = facade.assert_live() {
            return ready(Err(error)).boxed();
        }
        if args.iter().any(|value| !is_json_value(value)) {
            return ready(Err(ServiceError::remote(
                RemoteServiceErrorCode::ServiceInvalidValue,
                format!(
                    "Remote service method {}.{} received a non-JSON value",
                    display_js(&facade.service_id),
                    display_js(&self.name)
                ),
            )))
            .boxed();
        }
        let call = ServiceCall {
            service_id: facade.service_id.clone(),
            instance: facade.address.clone(),
            member: self.name.clone(),
            args,
        };
        let transport = Arc::clone(&facade.transport);
        let calls = Arc::clone(&facade.lifecycle.calls);
        let lifetime = calls.cancellation();
        Box::pin(async move {
            let permit = calls.begin()?;
            let future = transport.invoke(call, context.clone());
            let result = await_with_lifetime(&context, lifetime, future).await;
            drop(permit);
            result
        })
    }

    /// Returns the canonical consumer-side state replica for this member.
    ///
    /// # Errors
    /// Returns an error if the facade is gone, access is denied, or this member is not a state member.
    pub fn state(&self) -> Result<Arc<ReplicatedState>, ServiceError> {
        let Some(facade) = self.facade.upgrade() else {
            return Err(ServiceError::disposed("Remote service facade is gone"));
        };
        facade.lifecycle.assert_access()?;
        self.expect(MemberKind::State)?;
        Ok(Arc::clone(&self.state))
    }

    /// Subscribes to immutable state revisions. Hydration is delivered immediately if present.
    ///
    /// # Errors
    /// Returns an error if the facade is gone, access is denied, this member is not a state member,
    /// or state subscription/hydration fails.
    pub fn subscribe_state(
        &self,
        listener: ReplicatedStateListener,
    ) -> Result<Arc<dyn Fn() + Send + Sync>, ServiceError> {
        let Some(facade) = self.facade.upgrade() else {
            return Err(ServiceError::disposed("Remote service facade is gone"));
        };
        facade.lifecycle.assert_access()?;
        self.expect(MemberKind::State)?;
        self.state.subscribe(listener)
    }

    pub(crate) fn set_description(&self, kind: MemberKind) -> Result<(), ServiceError> {
        let mut state = lock(&self.kind);
        if let Some(actual) = state.actual
            && actual != kind
        {
            return Err(ServiceError::local(format!(
                "Remote service member {} changed kind",
                display_js(&self.name),
            )));
        }
        if let Some(expected) = state.expected
            && expected != kind
        {
            return Err(ServiceError::remote(
                RemoteServiceErrorCode::ServiceMemberMismatch,
                format!(
                    "Remote service member {} is {:?}, not {:?}",
                    display_js(&self.name),
                    kind,
                    expected
                ),
            ));
        }
        state.actual = Some(kind);
        Ok(())
    }

    pub(crate) fn hydrate(
        &self,
        sequence: JsInteger,
        ops: &[DeltaOp],
        context: &Context,
    ) -> Result<(), ServiceError> {
        self.set_description(MemberKind::State)?;
        self.state.hydrate(sequence, ops, context)
    }

    pub(crate) fn update(
        &self,
        sequence: JsInteger,
        ops: &[DeltaOp],
        context: &Context,
    ) -> Result<(), ServiceError> {
        self.set_description(MemberKind::State)?;
        self.state.update(sequence, ops, context)
    }

    pub(crate) fn clear(&self) {
        self.state.clear();
    }

    fn expect(&self, expected: MemberKind) -> Result<(), ServiceError> {
        let mut state = lock(&self.kind);
        if let Some(previous) = state.expected
            && previous != expected
        {
            return Err(ServiceError::remote(
                RemoteServiceErrorCode::ServiceMemberMismatch,
                format!(
                    "Remote service member {} was used as two different kinds",
                    display_js(&self.name)
                ),
            ));
        }
        state.expected = Some(expected);
        if let Some(actual) = state.actual
            && actual != expected
        {
            return Err(ServiceError::remote(
                RemoteServiceErrorCode::ServiceMemberMismatch,
                format!(
                    "Remote service member {} is {:?}, not {:?}",
                    display_js(&self.name),
                    actual,
                    expected
                ),
            ));
        }
        Ok(())
    }
}

type ValidatedMember<'a, Op> = (JsString, MemberKind, &'a ServiceMemberSnapshot<Op>);

fn validate_members<Op>(
    members: &[ServiceMemberSnapshot<Op>],
) -> Result<Vec<ValidatedMember<'_, Op>>, ServiceError>
where
    Op: super::super::wire::ServiceOp,
{
    let mut result = Vec::with_capacity(members.len());
    for member in members {
        let name = match member {
            ServiceMemberSnapshot::Method { name } | ServiceMemberSnapshot::State { name, .. } => {
                name
            }
        };
        if name.as_utf16().is_empty() || result.iter().any(|(known, _, _)| known == name) {
            return Err(ServiceError::local(
                "Remote service has invalid member descriptions",
            ));
        }
        result.push((name.clone(), MemberKind::from_snapshot(member), member));
    }
    Ok(result)
}

fn same_address(
    left: Option<&ServiceInstanceAddress>,
    right: Option<&ServiceInstanceAddress>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn display_js(value: &JsString) -> String {
    String::from_utf16_lossy(value.as_utf16())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
