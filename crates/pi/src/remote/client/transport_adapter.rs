use std::sync::Arc;

use futures::future::BoxFuture;
use pi_agent::context::Context;
use pi_agent::service::delta::DeltaOp;
use pi_agent::service::error::ServiceError;
use pi_agent::service::transport::{
    RemoteServiceTransport, ServiceSubscription as AgentServiceSubscription,
    ServiceUpdateListener as AgentUpdateListener,
};
use pi_agent::service::value::{JsString, JsonValue};
use pi_agent::service::wire::{ServiceCall, ServiceMode, ServiceSubscriptionSnapshot};

use super::{CancelToken, Client, ServiceSubscription};
use crate::remote::schemas::RpcTarget;

/// Adapts a lazily resolved remote route to the native service binding seam.
pub fn create_client_service_transport(
    client: Arc<Client>,
    target: impl Fn() -> Option<RpcTarget> + Send + Sync + 'static,
) -> Arc<dyn RemoteServiceTransport> {
    Arc::new(ClientServiceTransport {
        client,
        target: Arc::new(target),
    })
}

struct ClientServiceTransport {
    client: Arc<Client>,
    target: Arc<dyn Fn() -> Option<RpcTarget> + Send + Sync>,
}

impl RemoteServiceTransport for ClientServiceTransport {
    fn invoke(
        &self,
        call: ServiceCall,
        context: Context,
    ) -> BoxFuture<'_, Result<Option<JsonValue>, ServiceError>> {
        let client = Arc::clone(&self.client);
        let target = Arc::clone(&self.target);
        Box::pin(async move {
            let target = (target)()
                .ok_or_else(|| ServiceError::local("Remote service target is unavailable"))?;
            let cancel = cancel_from(&context);
            client
                .request(target, call, cancel.as_ref())
                .await
                .map_err(ServiceError::transport)
        })
    }

    fn subscribe(
        &self,
        service_id: JsString,
        mode: ServiceMode,
        listener: AgentUpdateListener,
        context: Context,
    ) -> BoxFuture<'_, Result<Arc<dyn AgentServiceSubscription>, ServiceError>> {
        let client = Arc::clone(&self.client);
        let target = Arc::clone(&self.target);
        Box::pin(async move {
            let target = (target)()
                .ok_or_else(|| ServiceError::local("Remote service target is unavailable"))?;
            let callback: super::ServiceUpdateListener = Arc::new(move |update| {
                listener(update, &Context::background());
            });
            let cancel = cancel_from(&context);
            let subscription = client
                .subscribe_service(target, service_id, mode, callback, cancel.as_ref())
                .await
                .map_err(ServiceError::transport)?;
            Ok(Arc::new(AdapterSubscription { subscription }) as Arc<dyn AgentServiceSubscription>)
        })
    }
}

struct AdapterSubscription {
    subscription: ServiceSubscription,
}

impl AgentServiceSubscription for AdapterSubscription {
    fn snapshot(&self) -> &ServiceSubscriptionSnapshot<DeltaOp> {
        self.subscription.snapshot()
    }

    fn activate(&self) {
        self.subscription.start();
    }

    fn close(&self, context: Context) -> BoxFuture<'_, Result<(), ServiceError>> {
        Box::pin(async move {
            let result = context
                .race(self.subscription.dispose())
                .await
                .map_err(ServiceError::from)?;
            result.map_err(ServiceError::transport)
        })
    }
}

fn cancel_from(context: &Context) -> Option<CancelToken> {
    CancelToken::from_context(context)
}
