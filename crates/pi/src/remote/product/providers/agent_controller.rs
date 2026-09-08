use std::sync::Arc;

use pi_agent::context::Context;
use pi_agent::harness::api::{AgentLane, NavigateOptions, PromptInput, QueueInput};
use pi_agent::harness::result::{
    CancelQueuedKind, HarnessCall, HarnessError, HarnessException, OperationResultRecord,
    RunOutcome, SuspendedRun,
};
use pi_agent::service::error::{RemoteServiceErrorCode, ServiceError};
use pi_agent::service::provider::{ServiceImplementation, ServiceMember, ServiceMethod};
use pi_agent::service::value::{JsString, JsonValue};
use pi_agent::session::{EntryId, OperationId};

use crate::remote::product::services::agent_controller::{
    AgentCompactionRequest, AgentNavigationRequest, AgentOperationError, AgentOperationResponse,
    AgentPromptRequest, AgentQueueResponse, CancelQueuedOutcome,
    AGENT_CONTROLLER_CANCEL_QUEUED_MEMBER, AGENT_CONTROLLER_COMPACT_MEMBER,
    AGENT_CONTROLLER_FOLLOW_UP_MEMBER, AGENT_CONTROLLER_NAVIGATE_MEMBER,
    AGENT_CONTROLLER_NEXT_RUN_MEMBER, AGENT_CONTROLLER_PROMPT_MEMBER,
    AGENT_CONTROLLER_REQUEST_ABORT_MEMBER, AGENT_CONTROLLER_RESUME_MEMBER,
    AGENT_CONTROLLER_STEER_MEMBER,
};
use crate::remote::product::services::ProductJsonConvert;

/// Builds the singleton implementation of the presentation-safe agent facade.
///
/// The lane methods have two result layers: [`HarnessCall`] represents a thrown
/// infrastructure rejection, while each existing harness result alias retains
/// expected operation failures as its inner `HarnessError`.  The former becomes
/// a service failure; the latter follows the source provider's response or
/// throw behavior member by member.
pub fn agent_controller_implementation(lane: Arc<dyn AgentLane>) -> ServiceImplementation {
    let mut implementation = std::collections::BTreeMap::new();

    let prompt_lane = Arc::clone(&lane);
    implementation.insert(
        service_member(AGENT_CONTROLLER_PROMPT_MEMBER),
        ServiceMember::Method(method(move |args, context| {
            let lane = Arc::clone(&prompt_lane);
            async move {
                let request = decode_prompt(args, AGENT_CONTROLLER_PROMPT_MEMBER)?;
                let result = into_service_result(
                    lane.prompt(to_prompt_input(request), &context).await,
                )?;
                let response = match result {
                    Ok(outcome) => to_operation_response(&outcome),
                    Err(error) => rejected_response(operation_id(&error), &error),
                };
                Ok(Some(response.into_json()?))
            }
        })),
    );

    let abort_lane = Arc::clone(&lane);
    implementation.insert(
        service_member(AGENT_CONTROLLER_REQUEST_ABORT_MEMBER),
        ServiceMember::Method(method(move |args, context| {
            let lane = Arc::clone(&abort_lane);
            async move {
                let operation_id = decode_string(args, AGENT_CONTROLLER_REQUEST_ABORT_MEMBER)?;
                let operation_id = OperationId::new(operation_id);
                let result = into_service_result(
                    lane.request_abort(&operation_id, &context).await,
                )?;
                result.map_err(expected_harness_error_to_service_error)?;
                Ok(None)
            }
        })),
    );

    implementation.insert(
        service_member(AGENT_CONTROLLER_STEER_MEMBER),
        ServiceMember::Method(queue_method(Arc::clone(&lane), QueueOperation::Steer)),
    );
    implementation.insert(
        service_member(AGENT_CONTROLLER_FOLLOW_UP_MEMBER),
        ServiceMember::Method(queue_method(
            Arc::clone(&lane),
            QueueOperation::FollowUp,
        )),
    );
    implementation.insert(
        service_member(AGENT_CONTROLLER_NEXT_RUN_MEMBER),
        ServiceMember::Method(queue_method(Arc::clone(&lane), QueueOperation::NextRun)),
    );

    let cancel_lane = Arc::clone(&lane);
    implementation.insert(
        service_member(AGENT_CONTROLLER_CANCEL_QUEUED_MEMBER),
        ServiceMember::Method(method(move |args, context| {
            let lane = Arc::clone(&cancel_lane);
            async move {
                let entry_id = decode_string(args, AGENT_CONTROLLER_CANCEL_QUEUED_MEMBER)?;
                let entry_id = EntryId::new(entry_id);
                let result = into_service_result(
                    lane.cancel_queued(&entry_id, &context).await,
                )?;
                let outcome = result.map_err(expected_harness_error_to_service_error)?;
                let outcome = match outcome {
                    CancelQueuedKind::Cancelled => CancelQueuedOutcome::Cancelled,
                    CancelQueuedKind::AlreadyConsumed => CancelQueuedOutcome::AlreadyConsumed,
                    CancelQueuedKind::NotFound => CancelQueuedOutcome::NotFound,
                };
                Ok(Some(outcome.into_json()?))
            }
        })),
    );

    let resume_lane = Arc::clone(&lane);
    implementation.insert(
        service_member(AGENT_CONTROLLER_RESUME_MEMBER),
        ServiceMember::Method(method(move |args, context| {
            let lane = Arc::clone(&resume_lane);
            async move {
                expect_no_args(args, AGENT_CONTROLLER_RESUME_MEMBER)?;
                let result = into_service_result(lane.resume(&context).await)?;
                let response = match result {
                    Ok(outcome) => to_operation_response(&outcome),
                    Err(error) => rejected_response(None, &error),
                };
                Ok(Some(response.into_json()?))
            }
        })),
    );

    let compact_lane = Arc::clone(&lane);
    implementation.insert(
        service_member(AGENT_CONTROLLER_COMPACT_MEMBER),
        ServiceMember::Method(method(move |args, context| {
            let lane = Arc::clone(&compact_lane);
            async move {
                let request = decode_compaction(args, AGENT_CONTROLLER_COMPACT_MEMBER)?;
                let result = into_service_result(
                    lane.compact(request.custom_instructions.as_deref(), &context).await,
                )?;
                let response = match result {
                    Ok(outcome) => to_operation_response_record(&outcome.compaction),
                    Err(error) => rejected_response(operation_id(&error), &error),
                };
                Ok(Some(response.into_json()?))
            }
        })),
    );

    let navigate_lane = Arc::clone(&lane);
    implementation.insert(
        service_member(AGENT_CONTROLLER_NAVIGATE_MEMBER),
        ServiceMember::Method(method(move |args, context| {
            let lane = Arc::clone(&navigate_lane);
            async move {
                let request = decode_navigation(args, AGENT_CONTROLLER_NAVIGATE_MEMBER)?;
                let target = request.target_id.map(EntryId::new);
                let options = NavigateOptions {
                    summarize: Some(request.summarize),
                    label: request.label,
                    custom_instructions: request.custom_instructions,
                };
                let result = into_service_result(
                    lane.navigate_tree(target.as_ref(), options, &context).await,
                )?;
                let response = match result {
                    Ok(outcome) => to_operation_response_record(&outcome.navigation),
                    Err(error) => rejected_response(operation_id(&error), &error),
                };
                Ok(Some(response.into_json()?))
            }
        })),
    );

    implementation
}

#[derive(Clone, Copy)]
enum QueueOperation {
    Steer,
    FollowUp,
    NextRun,
}

fn queue_method(lane: Arc<dyn AgentLane>, operation: QueueOperation) -> ServiceMethod {
    method(move |args, context| {
        let lane = Arc::clone(&lane);
        async move {
            let request = decode_prompt(args, queue_member(operation))?;
            let input = to_queue_input(request);
            let result = into_service_result(match operation {
                QueueOperation::Steer => lane.steer(input, &context).await,
                QueueOperation::FollowUp => lane.follow_up(input, &context).await,
                QueueOperation::NextRun => lane.next_run(input, &context).await,
            })?;
            let response = match result {
                Ok(entry_id) => AgentQueueResponse {
                    accepted: true,
                    entry_id: Some(entry_id.to_string()),
                    error: None,
                },
                Err(error) => AgentQueueResponse {
                    accepted: false,
                    entry_id: None,
                    error: Some(to_agent_error(&error)),
                },
            };
            Ok(Some(response.into_json()?))
        }
    })
}

fn method<F, Fut>(handler: F) -> ServiceMethod
where
    F: Fn(Vec<JsonValue>, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Option<JsonValue>, ServiceError>> + Send + 'static,
{
    Arc::new(move |args, context| Box::pin(handler(args, context)))
}

fn to_prompt_input(request: AgentPromptRequest) -> PromptInput {
    PromptInput::Text {
        text: request.message,
        images: to_images(request.images),
    }
}

fn to_queue_input(request: AgentPromptRequest) -> QueueInput {
    QueueInput::Text {
        text: request.message,
        images: to_images(request.images),
    }
}

fn to_images(images: Option<Vec<crate::remote::product::services::agent_controller::AgentPromptImage>>) -> Vec<pi_ai::ImageContent> {
    images
        .unwrap_or_default()
        .into_iter()
        .map(|image| pi_ai::ImageContent::new(image.data, image.mime_type))
        .collect()
}

pub(crate) fn to_operation_response(value: &RunOutcome) -> AgentOperationResponse {
    match value {
        RunOutcome::Settled(record) => to_operation_response_record(record),
        RunOutcome::Suspended(suspended) => to_operation_response_suspended(suspended),
    }
}

fn to_operation_response_record(value: &OperationResultRecord) -> AgentOperationResponse {
    let error = if matches!(value.status, pi_agent::session::TerminalStatus::Failed) {
        value.error.as_ref().map(|error| AgentOperationError {
            code: error.code.clone(),
            message: error.message.clone(),
        })
    } else {
        None
    };
    AgentOperationResponse {
        accepted: true,
        operation_id: Some(value.operation_id.to_string()),
        error,
    }
}

fn to_operation_response_suspended(value: &SuspendedRun) -> AgentOperationResponse {
    AgentOperationResponse {
        accepted: true,
        operation_id: Some(value.operation_id.to_string()),
        error: None,
    }
}

fn rejected_response(operation_id: Option<String>, error: &HarnessError) -> AgentOperationResponse {
    AgentOperationResponse {
        accepted: false,
        operation_id,
        error: Some(to_agent_error(error)),
    }
}

pub(crate) fn to_agent_error(error: &HarnessError) -> AgentOperationError {
    let code = match error {
        HarnessError::LaneBusy { .. } => "lane_busy",
        HarnessError::InvalidMessage { .. } => "invalid_message",
        HarnessError::UnknownSkill { .. } => "unknown_skill",
        HarnessError::UnknownTemplate { .. } => "unknown_template",
        HarnessError::NothingToCompact { .. } => "nothing_to_compact",
        HarnessError::NothingToResume { .. } => "nothing_to_resume",
        HarnessError::InvalidNavigation { .. } => "invalid_navigation",
        HarnessError::UnknownTarget { .. } => "unknown_target",
        HarnessError::Closed { .. } => "closed",
        _ => "operation_failed",
    };
    AgentOperationError {
        code: code.to_owned(),
        message: error.to_string(),
    }
}

fn operation_id(error: &HarnessError) -> Option<String> {
    match error {
        HarnessError::LaneBusy { operation_id, .. } => Some(operation_id.to_string()),
        _ => None,
    }
}

fn into_service_result<T>(result: HarnessCall<T>) -> Result<T, ServiceError> {
    result.map_err(harness_exception_to_service_error)
}


fn harness_exception_to_service_error(error: HarnessException) -> ServiceError {
    ServiceError::handler(error)
}

fn expected_harness_error_to_service_error(error: HarnessError) -> ServiceError {
    ServiceError::handler(error)
}

fn decode_prompt(args: Vec<JsonValue>, member: &str) -> Result<AgentPromptRequest, ServiceError> {
    AgentPromptRequest::from_json(one_arg(args, member)?).map_err(|error| invalid_value(error.to_string()))
}

fn decode_compaction(
    args: Vec<JsonValue>,
    member: &str,
) -> Result<AgentCompactionRequest, ServiceError> {
    AgentCompactionRequest::from_json(one_arg(args, member)?).map_err(|error| invalid_value(error.to_string()))
}

fn decode_navigation(
    args: Vec<JsonValue>,
    member: &str,
) -> Result<AgentNavigationRequest, ServiceError> {
    AgentNavigationRequest::from_json(one_arg(args, member)?).map_err(|error| invalid_value(error.to_string()))
}

fn decode_string(args: Vec<JsonValue>, member: &str) -> Result<String, ServiceError> {
    let value = one_arg(args, member)?;
    let Some(value) = value.as_str() else {
        return Err(invalid_value(format!("{member} expects one string argument")));
    };
    value
        .try_to_utf8()
        .map_err(|error| invalid_value(format!("{member} argument is not valid UTF-8: {error}")))
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

fn invalid_value(message: impl Into<String>) -> ServiceError {
    ServiceError::remote(RemoteServiceErrorCode::ServiceInvalidValue, message)
}

fn service_member(name: &str) -> JsString {
    JsString::from_utf8(name)
}

fn queue_member(operation: QueueOperation) -> &'static str {
    match operation {
        QueueOperation::Steer => AGENT_CONTROLLER_STEER_MEMBER,
        QueueOperation::FollowUp => AGENT_CONTROLLER_FOLLOW_UP_MEMBER,
        QueueOperation::NextRun => AGENT_CONTROLLER_NEXT_RUN_MEMBER,
    }
}


