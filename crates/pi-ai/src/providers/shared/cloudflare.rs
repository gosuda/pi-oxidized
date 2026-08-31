//! Cloudflare provider URL-template resolution.

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::types::Model;

const AI_GATEWAY_PROVIDER: &str = "cloudflare-ai-gateway";
const WORKERS_AI_PROVIDER: &str = "cloudflare-workers-ai";
const ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
const GATEWAY_ID: &str = "CLOUDFLARE_GATEWAY_ID";
const ACCOUNT_ID_PLACEHOLDER: &str = "{CLOUDFLARE_ACCOUNT_ID}";
const GATEWAY_ID_PLACEHOLDER: &str = "{CLOUDFLARE_GATEWAY_ID}";

pub(crate) fn resolve_model<'a>(
    model: &'a Model,
    env: Option<&BTreeMap<String, String>>,
) -> Cow<'a, Model> {
    if !matches!(
        model.provider.as_str(),
        AI_GATEWAY_PROVIDER | WORKERS_AI_PROVIDER
    ) {
        return Cow::Borrowed(model);
    }
    let Some(env) = env else {
        return Cow::Borrowed(model);
    };
    let account_id = env
        .get(ACCOUNT_ID)
        .filter(|_| model.base_url.contains(ACCOUNT_ID_PLACEHOLDER));
    let gateway_id = env
        .get(GATEWAY_ID)
        .filter(|_| model.base_url.contains(GATEWAY_ID_PLACEHOLDER));
    if account_id.is_none() && gateway_id.is_none() {
        return Cow::Borrowed(model);
    }

    let mut resolved = model.clone();
    if let Some(account_id) = account_id {
        resolved.base_url = resolved
            .base_url
            .replace(ACCOUNT_ID_PLACEHOLDER, account_id);
    }
    if let Some(gateway_id) = gateway_id {
        resolved.base_url = resolved
            .base_url
            .replace(GATEWAY_ID_PLACEHOLDER, gateway_id);
    }
    Cow::Owned(resolved)
}
