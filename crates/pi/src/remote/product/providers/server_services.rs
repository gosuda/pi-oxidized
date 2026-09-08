//! Source-C experimental server services.
//!
//! Mirrors `packages/coding-agent/src/experimental/services/server.ts`:
//! a per-connection `RoutedServerServiceHost` that publishes the session
//! directory as a mutable replicated state, exposes session-management and
//! presentation-plugin methods, and serializes every mutating method through
//! one asynchronous tail.

use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::future::BoxFuture;
use pi_agent::context::Context;
use pi_agent::service::error::{RemoteServiceErrorCode, ServiceError};
use pi_agent::service::provider::{
    RemoteServiceProvider, ServiceDefinition, ServiceImplementation, ServiceMember, ServiceMethod,
    ServiceMode,
};
use pi_agent::service::replicated::MutableReplicatedState;
use pi_agent::service::value::{JsInteger, JsString, JsonValue};
use tokio::sync::oneshot;

use crate::remote::product::providers::attachment::ProviderAttachment;
use crate::remote::product::services::plugins::{
    PrepareSessionRequest, PRESENTATION_PLUGINS_ID, PRESENTATION_PLUGINS_PREPARE_SESSION_MEMBER,
    PRESENTATION_PLUGINS_RELOAD_MEMBER,
};
use crate::remote::product::services::sessions::{
    SessionCreateOptions, SessionDirectoryState, SessionSummary, SESSION_DIRECTORY_ID,
    SESSION_DIRECTORY_STATE_MEMBER, SESSION_MANAGEMENT_ATTACH_MEMBER,
    SESSION_MANAGEMENT_CREATE_MEMBER, SESSION_MANAGEMENT_DETACH_MEMBER, SESSION_MANAGEMENT_ID,
    SESSION_MANAGEMENT_REMOVE_MEMBER,
};
use crate::remote::product::services::ProductJsonConvert;
use crate::remote::server::errors::{duplicate_host_error, HostError};
use crate::remote::server::host::{
    RoutedServerPresentation, RoutedServerServiceAttachment, RoutedServerServiceHost,
};

/// Host ports supplied by the native server owner for the three server-scoped
/// service families.
pub struct ServerServiceCallbacks {
    /// Returns the current session list, sorted by `sessionId` then `createdAt`.
    pub list: Arc<
        dyn Fn(Context) -> BoxFuture<'static, Result<Vec<SessionSummary>, HostError>>
            + Send
            + Sync,
    >,
    /// Creates a new session and returns its summary.
    pub create: Arc<
        dyn Fn(SessionCreateOptions, Context) -> BoxFuture<'static, Result<SessionSummary, HostError>>
            + Send
            + Sync,
    >,
    /// Removes an existing session.
    pub remove: Arc<
        dyn Fn(String, Context) -> BoxFuture<'static, Result<(), HostError>> + Send + Sync,
    >,
    /// Resolves and validates plugin packages for a session.
    pub prepare_session_plugins: Arc<
        dyn Fn(
                String,
                Option<Vec<String>>,
                Context,
            ) -> BoxFuture<'static, Result<PreparedSessionPlugins, HostError>>
            + Send
            + Sync,
    >,
    /// Reloads presentation plugin bundles from a prepared selection.
    pub reload_presentation_plugins: Arc<
        dyn Fn(Vec<String>, Context) -> BoxFuture<'static, Result<JsonValue, HostError>>
            + Send
            + Sync,
    >,
}

/// Result of preparing session plugins.
pub struct PreparedSessionPlugins {
    /// Canonical package paths that were selected for this session.
    pub package_paths: Vec<String>,
    /// Opaque presentation plugin artifacts returned by the plugin host.
    pub presentation_plugins: JsonValue,
}

/// The server-scoped service composition used by `ExperimentalServerHost`.
pub struct ExperimentalServerServices {
    callbacks: Arc<ServerServiceCallbacks>,
    directory: Arc<MutableReplicatedState>,
    attachments: Arc<Mutex<Vec<Arc<ProviderAttachment>>>>,
    mutation_tail: MutationTail,
    revision: Arc<Mutex<JsInteger>>,
}

impl ExperimentalServerServices {
    /// Creates the service composition after loading the initial directory.
    pub async fn create(callbacks: ServerServiceCallbacks) -> Result<Arc<Self>, HostError> {
        let initial = callbacks.list(Context::background()).await?;
        let initial_state = SessionDirectoryState {
            revision: JsInteger::one(),
            sessions: initial,
        }
        .into_json()
        .map_err(|error| HostError::from(map_product_error(error)))?;
        Ok(Self::new(callbacks, initial_state))
    }

    /// Creates the service composition with an already-constructed directory
    /// state.
    #[must_use]
    pub fn new(callbacks: ServerServiceCallbacks, initial_state: JsonValue) -> Arc<Self> {
        Arc::new(Self {
            callbacks: Arc::new(callbacks),
            directory: MutableReplicatedState::new(initial_state),
            attachments: Arc::new(Mutex::new(Vec::new())),
            mutation_tail: MutationTail::new(),
            revision: Arc::new(Mutex::new(JsInteger::one())),
        })
    }

    /// Refreshes the session directory state.
    pub async fn refresh(&self, cx: Context) -> Result<(), HostError> {
        let callbacks = Arc::clone(&self.callbacks);
        let directory = Arc::clone(&self.directory);
        let revision = Arc::clone(&self.revision);
        let mutation_tail = self.mutation_tail.clone();

        mutation_tail
            .run(move || {
                Box::pin(async move {
                    refresh_now_impl(callbacks, directory, revision, cx).await
                })
            })
            .await
    }

    /// Disposes every attachment and waits for the mutation tail to drain.
    pub async fn dispose(&self) -> Result<(), HostError> {
        let attachments = {
            let mut guard = lock(&self.attachments);
            std::mem::take(&mut *guard)
        };

        let cx = Context::background();
        let mut errors = Vec::new();
        for attachment in attachments {
            if let Err(error) = attachment.release(cx.clone()).await {
                errors.push(error);
            }
        }

        self.mutation_tail.drain().await;

        match errors.len() {
            0 => Ok(()),
            1 => Err(duplicate_host_error(&errors[0])),
            _ => Err(HostError::Other(Box::new(AggregateReleaseError(errors)))),
        }
    }
}

impl RoutedServerServiceHost for ExperimentalServerServices {
    fn attach_client(
        &self,
        presentation: Arc<dyn RoutedServerPresentation>,
        cx: Context,
    ) -> BoxFuture<'_, Result<Arc<dyn RoutedServerServiceAttachment>, HostError>> {
        let callbacks = Arc::clone(&self.callbacks);
        let directory = Arc::clone(&self.directory);
        let revision = Arc::clone(&self.revision);
        let mutation_tail = self.mutation_tail.clone();
        let attachments = Arc::clone(&self.attachments);

        Box::pin(async move {
            let provider = Arc::new(
                RemoteServiceProvider::new(vec![
                    ServiceDefinition {
                        id: JsString::from(SESSION_DIRECTORY_ID),
                        local: false,
                        mode: ServiceMode::Singleton,
                    },
                    ServiceDefinition {
                        id: JsString::from(SESSION_MANAGEMENT_ID),
                        local: false,
                        mode: ServiceMode::Singleton,
                    },
                    ServiceDefinition {
                        id: JsString::from(PRESENTATION_PLUGINS_ID),
                        local: false,
                        mode: ServiceMode::Singleton,
                    },
                ])
                .map_err(HostError::from)?,
            );

            // pi.session-directory: one shared mutable replicated state.
            let mut directory_impl = ServiceImplementation::new();
            directory_impl.insert(
                JsString::from(SESSION_DIRECTORY_STATE_MEMBER),
                ServiceMember::State(Arc::clone(&directory)),
            );
            provider
                .provide(&JsString::from(SESSION_DIRECTORY_ID), directory_impl)
                .map_err(HostError::from)?;

            // pi.presentation-plugins: per-attachment prepared package selection.
            let prepared_package_paths: Arc<Mutex<Option<Vec<String>>>> =
                Arc::new(Mutex::new(None));

            let mut plugins_impl = ServiceImplementation::new();
            let callbacks_prepare = Arc::clone(&callbacks);
            let mutation_tail_prepare = mutation_tail.clone();
            let prepared_package_paths_prepare = Arc::clone(&prepared_package_paths);
            plugins_impl.insert(
                JsString::from(PRESENTATION_PLUGINS_PREPARE_SESSION_MEMBER),
                ServiceMember::Method(method(move |args, cx| {
                    let callbacks = Arc::clone(&callbacks_prepare);
                    let mutation_tail = mutation_tail_prepare.clone();
                    let prepared_package_paths = Arc::clone(&prepared_package_paths_prepare);
                    async move {
                        mutation_tail
                            .run(move || {
                                Box::pin(async move {
                                    let request = PrepareSessionRequest::from_json(one_arg(
                                        args,
                                        PRESENTATION_PLUGINS_PREPARE_SESSION_MEMBER,
                                    )?)
                                    .map_err(map_product_error)?;
                                    let selected = callbacks
                                        .prepare_session_plugins(
                                            request.session_id,
                                            request.package_paths,
                                            cx,
                                        )
                                        .await
                                        .map_err(host_error_to_service_error)?;
                                    *lock(&prepared_package_paths) =
                                        Some(selected.package_paths.clone());
                                    Ok(Some(selected.presentation_plugins))
                                })
                            })
                            .await
                            .map_err(host_error_to_service_error)
                    }
                })),
            );

            let callbacks_reload = Arc::clone(&callbacks);
            let mutation_tail_reload = mutation_tail.clone();
            let prepared_package_paths_reload = Arc::clone(&prepared_package_paths);
            plugins_impl.insert(
                JsString::from(PRESENTATION_PLUGINS_RELOAD_MEMBER),
                ServiceMember::Method(method(move |args, cx| {
                    let callbacks = Arc::clone(&callbacks_reload);
                    let mutation_tail = mutation_tail_reload.clone();
                    let prepared_package_paths = Arc::clone(&prepared_package_paths_reload);
                    async move {
                        mutation_tail
                            .run(move || {
                                Box::pin(async move {
                                    expect_no_args(args, PRESENTATION_PLUGINS_RELOAD_MEMBER)?;
                                    let package_paths = lock(&prepared_package_paths).clone();
                                    let package_paths = package_paths.ok_or_else(|| {
                                        ServiceError::remote(
                                            RemoteServiceErrorCode::ServiceInvalidValue,
                                            "No Session plugin selection is prepared",
                                        )
                                    })?;
                                    let result = callbacks
                                        .reload_presentation_plugins(package_paths, cx)
                                        .await
                                        .map_err(host_error_to_service_error)?;
                                    Ok(Some(result))
                                })
                            })
                            .await
                            .map_err(host_error_to_service_error)
                    }
                })),
            );
            provider
                .provide(&JsString::from(PRESENTATION_PLUGINS_ID), plugins_impl)
                .map_err(HostError::from)?;

            // pi.session-management: create/remove/attach/detach.
            let mut management_impl = ServiceImplementation::new();

            let callbacks_create = Arc::clone(&callbacks);
            let directory_create = Arc::clone(&directory);
            let revision_create = Arc::clone(&revision);
            let mutation_tail_create = mutation_tail.clone();
            management_impl.insert(
                JsString::from(SESSION_MANAGEMENT_CREATE_MEMBER),
                ServiceMember::Method(method(move |args, cx| {
                    let callbacks = Arc::clone(&callbacks_create);
                    let directory = Arc::clone(&directory_create);
                    let revision = Arc::clone(&revision_create);
                    let mutation_tail = mutation_tail_create.clone();
                    async move {
                        mutation_tail
                            .run(move || {
                                Box::pin(async move {
                                    let options = SessionCreateOptions::from_json(one_arg(
                                        args,
                                        SESSION_MANAGEMENT_CREATE_MEMBER,
                                    )?)
                                    .map_err(map_product_error)?;
                                    let created = callbacks
                                        .create(options, cx.clone())
                                        .await
                                        .map_err(host_error_to_service_error)?;
                                    refresh_now_impl(
                                        Arc::clone(&callbacks),
                                        directory,
                                        revision,
                                        cx.clone(),
                                    )
                                    .await?;
                                    let result =
                                        created.into_json().map_err(map_product_error)?;
                                    Ok(Some(result))
                                })
                            })
                            .await
                            .map_err(host_error_to_service_error)
                    }
                })),
            );

            let callbacks_remove = Arc::clone(&callbacks);
            let directory_remove = Arc::clone(&directory);
            let revision_remove = Arc::clone(&revision);
            let mutation_tail_remove = mutation_tail.clone();
            let presentation_remove = Arc::clone(&presentation);
            management_impl.insert(
                JsString::from(SESSION_MANAGEMENT_REMOVE_MEMBER),
                ServiceMember::Method(method(move |args, cx| {
                    let callbacks = Arc::clone(&callbacks_remove);
                    let directory = Arc::clone(&directory_remove);
                    let revision = Arc::clone(&revision_remove);
                    let mutation_tail = mutation_tail_remove.clone();
                    let presentation = Arc::clone(&presentation_remove);
                    async move {
                        mutation_tail
                            .run(move || {
                                Box::pin(async move {
                                    let session_id = decode_string(
                                        one_arg(args, SESSION_MANAGEMENT_REMOVE_MEMBER)?,
                                        SESSION_MANAGEMENT_REMOVE_MEMBER,
                                    )?;
                                    presentation
                                        .prepare_session_removal(session_id.clone(), cx.clone())
                                        .await
                                        .map_err(host_error_to_service_error)?;
                                    callbacks
                                        .remove(session_id, cx.clone())
                                        .await
                                        .map_err(host_error_to_service_error)?;
                                    refresh_now_impl(
                                        Arc::clone(&callbacks),
                                        directory,
                                        revision,
                                        cx.clone(),
                                    )
                                    .await?;
                                    Ok(None)
                                })
                            })
                            .await
                            .map_err(host_error_to_service_error)
                    }
                })),
            );

            let mutation_tail_attach = mutation_tail.clone();
            let presentation_attach = Arc::clone(&presentation);
            management_impl.insert(
                JsString::from(SESSION_MANAGEMENT_ATTACH_MEMBER),
                ServiceMember::Method(method(move |args, cx| {
                    let mutation_tail = mutation_tail_attach.clone();
                    let presentation = Arc::clone(&presentation_attach);
                    async move {
                        mutation_tail
                            .run(move || {
                                Box::pin(async move {
                                    let session_id = decode_string(
                                        one_arg(args, SESSION_MANAGEMENT_ATTACH_MEMBER)?,
                                        SESSION_MANAGEMENT_ATTACH_MEMBER,
                                    )?;
                                    presentation
                                        .attach_session(session_id, cx)
                                        .await
                                        .map_err(host_error_to_service_error)?;
                                    Ok(None)
                                })
                            })
                            .await
                            .map_err(host_error_to_service_error)
                    }
                })),
            );

            let mutation_tail_detach = mutation_tail.clone();
            let presentation_detach = Arc::clone(&presentation);
            let prepared_package_paths_detach = Arc::clone(&prepared_package_paths);
            management_impl.insert(
                JsString::from(SESSION_MANAGEMENT_DETACH_MEMBER),
                ServiceMember::Method(method(move |args, cx| {
                    let mutation_tail = mutation_tail_detach.clone();
                    let presentation = Arc::clone(&presentation_detach);
                    let prepared_package_paths = Arc::clone(&prepared_package_paths_detach);
                    async move {
                        mutation_tail
                            .run(move || {
                                Box::pin(async move {
                                    expect_no_args(args, SESSION_MANAGEMENT_DETACH_MEMBER)?;
                                    presentation
                                        .detach_session(cx)
                                        .await
                                        .map_err(host_error_to_service_error)?;
                                    *lock(&prepared_package_paths) = None;
                                    Ok(None)
                                })
                            })
                            .await
                            .map_err(host_error_to_service_error)
                    }
                })),
            );
            provider
                .provide(&JsString::from(SESSION_MANAGEMENT_ID), management_impl)
                .map_err(HostError::from)?;

            // Wrap the provider in an attachment that removes itself on release.
            let attachment = Arc::new_cyclic(|weak| {
                let on_release = {
                    let weak = weak.clone();
                    let attachments = Arc::clone(&attachments);
                    move || {
                        if let Some(arc) = weak.upgrade() {
                            let mut guard = lock(&attachments);
                            guard.retain(|a| !Arc::ptr_eq(a, &arc));
                        }
                    }
                };
                ProviderAttachment::new_unwrapped(provider, on_release)
            });

            {
                let mut guard = lock(&attachments);
                guard.push(Arc::clone(&attachment));
            }

            let attachment: Arc<dyn RoutedServerServiceAttachment> = attachment;
            Ok(attachment)
        })
    }
}

/// A single serialized mutation queue.  Each `run` returns a future that
/// resolves after the queued operation, while the internal tail advances
/// regardless of whether the caller awaits the result.
#[derive(Clone)]
struct MutationTail {
    previous: Arc<Mutex<Option<BoxFuture<'static, ()>>>>,
}

impl MutationTail {
    fn new() -> Self {
        Self {
            previous: Arc::new(Mutex::new(None)),
        }
    }

    async fn run<T, F>(&self, operation: F) -> Result<T, HostError>
    where
        F: FnOnce() -> BoxFuture<'static, Result<T, HostError>> + Send + 'static,
        T: Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();

        let mut guard = lock(&self.previous);
        let previous = std::mem::take(&mut *guard);

        let handle = tokio::spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            let result = operation().await;
            let _ = result_tx.send(result);
        });

        let new_tail: BoxFuture<'static, ()> =
            Box::pin(async move { let _ = handle.await; });
        *guard = Some(new_tail);

        result_rx
            .await
            .map_err(|_| HostError::Service(ServiceError::local("mutation tail cancelled")))?
    }

    async fn drain(&self) {
        loop {
            let tail = {
                let mut guard = lock(&self.previous);
                std::mem::take(&mut *guard)
            };
            match tail {
                None => return,
                Some(tail) => tail.await,
            }
        }
    }
}

async fn refresh_now_impl(
    callbacks: Arc<ServerServiceCallbacks>,
    directory: Arc<MutableReplicatedState>,
    revision: Arc<Mutex<JsInteger>>,
    cx: Context,
) -> Result<(), HostError> {
    let sessions = callbacks.list(cx.clone()).await?;
    let revision = {
        let mut guard = lock(&revision);
        *guard = guard.next();
        guard.clone()
    };
    let state = SessionDirectoryState { revision, sessions }
        .into_json()
        .map_err(|error| HostError::from(map_product_error(error)))?;
    directory.with_state_mut(|value| *value = state);
    directory.publish(cx).map_err(HostError::from)?;
    Ok(())
}

fn method<F, Fut>(handler: F) -> ServiceMethod
where
    F: Fn(Vec<JsonValue>, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Option<JsonValue>, ServiceError>> + Send + 'static,
{
    Arc::new(move |args, cx| Box::pin(handler(args, cx)))
}

fn one_arg(mut args: Vec<JsonValue>, member: &str) -> Result<JsonValue, ServiceError> {
    if args.len() != 1 {
        return Err(invalid_value(format!(
            "{member} expects one argument, got {}",
            args.len()
        )));
    }
    args.pop()
        .ok_or_else(|| invalid_value(format!("{member} expects one argument")))
}

fn expect_no_args(args: Vec<JsonValue>, member: &str) -> Result<(), ServiceError> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(invalid_value(format!("{member} expects no arguments")))
    }
}

fn decode_string(value: JsonValue, member: &str) -> Result<String, ServiceError> {
    let value = value.as_str().ok_or_else(|| {
        invalid_value(format!("{member} expects one string argument"))
    })?;
    value
        .try_to_utf8()
        .map_err(|error| invalid_value(format!("{member} argument is not valid UTF-8: {error}")))
}

fn invalid_value(message: impl Into<String>) -> ServiceError {
    ServiceError::remote(RemoteServiceErrorCode::ServiceInvalidValue, message)
}

fn map_product_error(error: ServiceError) -> ServiceError {
    ServiceError::remote(RemoteServiceErrorCode::ServiceInvalidValue, error.to_string())
}

fn host_error_to_service_error(error: HostError) -> ServiceError {
    match error {
        HostError::Service(error) => error,
        other => ServiceError::handler(other),
    }
}

#[derive(Debug)]
struct AggregateReleaseError(Vec<HostError>);

impl fmt::Display for AggregateReleaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Failed to release server service attachments: ")?;
        let messages: Vec<String> = self.0.iter().map(|error| error.to_string()).collect();
        formatter.write_str(&messages.join("; "))
    }
}

impl std::error::Error for AggregateReleaseError {}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
