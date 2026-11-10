//! Standard A2A v1 JSON-RPC transport. Protocol parsing and serialization are
//! supplied by the official A2A Rust SDK; domain decisions stay in Gateway.
use std::{collections::HashMap, sync::Arc};

use ::a2a::*;
use a2a_server::{RequestHandler, StaticAgentCard, middleware::ServiceParams};
use async_trait::async_trait;
use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use futures::stream::BoxStream;
use serde_json::{Value, json};

#[async_trait]
pub trait Gateway: Send + Sync + 'static {
    async fn send_message(&self, caller: &str, message: Value) -> Result<Value, String>;
    async fn get_task(&self, caller: &str, id: &str) -> Result<Value, String>;
    async fn list_tasks(&self, caller: &str) -> Result<Vec<Value>, String>;
    async fn cancel_task(&self, caller: &str, id: &str) -> Result<Value, String>;
}

struct Handler {
    gateway: Arc<dyn Gateway>,
    credentials: Arc<HashMap<String, String>>,
}

fn bearer(value: &str) -> Option<&str> {
    value.strip_prefix("Bearer ").filter(|s| !s.is_empty())
}

async fn authenticate(
    State(credentials): State<Arc<HashMap<String, String>>>,
    request: Request,
    next: Next,
) -> Response {
    let authenticated = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer)
        .is_some_and(|token| credentials.contains_key(token));
    if !authenticated {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "Authentication required",
        )
            .into_response();
    }
    next.run(request).await
}

impl Handler {
    fn caller<'a>(&'a self, params: &ServiceParams) -> Result<&'a str, A2AError> {
        params
            .get("authorization")
            .and_then(|v| v.first())
            .and_then(|v| bearer(v))
            .and_then(|t| self.credentials.get(t))
            .map(String::as_str)
            .ok_or_else(|| A2AError::invalid_request("Authentication required"))
    }
}

fn domain_error(error: String, task_id: Option<&str>) -> A2AError {
    let lower = error.to_ascii_lowercase();
    if lower.contains("not_found") || lower.contains("not found") || lower.contains("forbidden") {
        A2AError::task_not_found(task_id.unwrap_or("unknown"))
    } else if lower.contains("not_cancelable") || lower.contains("terminal") {
        A2AError::task_not_cancelable(task_id.unwrap_or("unknown"))
    } else {
        A2AError::invalid_params(error)
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, A2AError> {
    serde_json::from_value(value).map_err(|_| A2AError::invalid_agent_response())
}

fn trim_history(task: &mut Task, length: Option<i32>) -> Result<(), A2AError> {
    if let Some(length) = length {
        if length < 0 {
            return Err(A2AError::invalid_params(
                "historyLength must be nonnegative",
            ));
        }
        if let Some(history) = &mut task.history {
            let keep = length as usize;
            if history.len() > keep {
                history.drain(..history.len() - keep);
            }
        }
    }
    Ok(())
}

#[async_trait]
impl RequestHandler for Handler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        if req.tenant.is_some() {
            return Err(A2AError::invalid_params(
                "This deployment does not declare a tenant interface",
            ));
        }
        if req
            .configuration
            .as_ref()
            .and_then(|c| c.task_push_notification_config.as_ref())
            .is_some()
        {
            return Err(A2AError::push_notification_not_supported());
        }
        let history = req.configuration.as_ref().and_then(|c| c.history_length);
        if history.is_some_and(|n| n < 0) {
            return Err(A2AError::invalid_params(
                "historyLength must be nonnegative",
            ));
        }
        if req
            .configuration
            .as_ref()
            .and_then(|c| c.accepted_output_modes.as_ref())
            .is_some_and(|modes| {
                !modes.is_empty()
                    && !modes
                        .iter()
                        .any(|m| matches!(m.as_str(), "text/plain" | "application/json"))
            })
        {
            return Err(A2AError::content_type_not_supported());
        }
        let message =
            serde_json::to_value(req.message).map_err(|_| A2AError::invalid_agent_response())?;
        let value = self
            .gateway
            .send_message(self.caller(params)?, message)
            .await
            .map_err(|e| domain_error(e, None))?;
        let mut task: Task = decode(value)?;
        trim_history(&mut task, history)?;
        Ok(SendMessageResponse::Task(task))
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        if req.tenant.is_some() {
            return Err(A2AError::invalid_params(
                "This deployment does not declare a tenant interface",
            ));
        }
        if req.history_length.is_some_and(|n| n < 0) {
            return Err(A2AError::invalid_params(
                "historyLength must be nonnegative",
            ));
        }
        let value = self
            .gateway
            .get_task(self.caller(params)?, &req.id)
            .await
            .map_err(|e| domain_error(e, Some(&req.id)))?;
        let mut task: Task = decode(value)?;
        trim_history(&mut task, req.history_length)?;
        Ok(task)
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        if req.tenant.is_some() {
            return Err(A2AError::invalid_params(
                "This deployment does not declare a tenant interface",
            ));
        }
        let page_size = req.page_size.unwrap_or(50);
        if !(1..=100).contains(&page_size) || req.history_length.is_some_and(|n| n < 0) {
            return Err(A2AError::invalid_params(
                "Invalid pageSize or historyLength",
            ));
        }
        let values = self
            .gateway
            .list_tasks(self.caller(params)?)
            .await
            .map_err(|e| domain_error(e, None))?;
        let mut tasks: Vec<Task> = values.into_iter().map(decode).collect::<Result<_, _>>()?;
        tasks.retain(|t| {
            req.context_id.as_ref().is_none_or(|c| &t.context_id == c)
                && req.status.as_ref().is_none_or(|s| &t.status.state == s)
                && req
                    .status_timestamp_after
                    .as_ref()
                    .is_none_or(|after| t.status.timestamp.as_ref().is_some_and(|ts| ts > after))
        });
        tasks.sort_by(|a, b| {
            b.status
                .timestamp
                .cmp(&a.status.timestamp)
                .then_with(|| b.id.cmp(&a.id))
        });
        let total_size = tasks.len().min(i32::MAX as usize) as i32;
        // Cursor is the last task's stable timestamp/id pair, not an offset.
        if let Some(token) = req.page_token.as_deref().filter(|s| !s.is_empty()) {
            let cursor: (Option<chrono::DateTime<chrono::Utc>>, String) =
                serde_json::from_str(token)
                    .map_err(|_| A2AError::invalid_params("Invalid pageToken"))?;
            tasks.retain(|t| (&t.status.timestamp, &t.id) < (&cursor.0, &cursor.1));
        }
        let more = tasks.len() > page_size as usize;
        tasks.truncate(page_size as usize);
        let next_page_token = if more {
            let last = tasks.last().expect("nonempty page");
            serde_json::to_string(&(last.status.timestamp, &last.id))
                .map_err(|_| A2AError::invalid_agent_response())?
        } else {
            String::new()
        };
        for task in &mut tasks {
            trim_history(task, req.history_length)?;
            if !req.include_artifacts.unwrap_or(false) {
                task.artifacts = None;
            }
        }
        Ok(ListTasksResponse {
            tasks,
            next_page_token,
            page_size,
            total_size,
        })
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        if req.tenant.is_some() {
            return Err(A2AError::invalid_params(
                "This deployment does not declare a tenant interface",
            ));
        }
        let value = self
            .gateway
            .cancel_task(self.caller(params)?, &req.id)
            .await
            .map_err(|e| domain_error(e, Some(&req.id)))?;
        decode(value)
    }

    async fn send_streaming_message(
        &self,
        _: &ServiceParams,
        _: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        Err(A2AError::unsupported_operation(
            "Streaming is not supported; poll GetTask",
        ))
    }
    async fn subscribe_to_task(
        &self,
        _: &ServiceParams,
        _: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        Err(A2AError::unsupported_operation(
            "Streaming is not supported; poll GetTask",
        ))
    }
    async fn create_push_config(
        &self,
        _: &ServiceParams,
        _: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Err(A2AError::push_notification_not_supported())
    }
    async fn get_push_config(
        &self,
        _: &ServiceParams,
        _: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Err(A2AError::push_notification_not_supported())
    }
    async fn list_push_configs(
        &self,
        _: &ServiceParams,
        _: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        Err(A2AError::push_notification_not_supported())
    }
    async fn delete_push_config(
        &self,
        _: &ServiceParams,
        _: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        Err(A2AError::push_notification_not_supported())
    }
    async fn get_extended_agent_card(
        &self,
        _: &ServiceParams,
        _: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        Err(A2AError::unsupported_operation(
            "Extended Agent Cards are not supported",
        ))
    }
}

pub fn router(
    gateway: Arc<dyn Gateway>,
    name: &str,
    description: &str,
    base_url: &str,
    credentials: HashMap<String, String>,
) -> Router {
    let credentials = Arc::new(credentials);
    let handler = Arc::new(Handler {
        gateway,
        credentials: credentials.clone(),
    });
    let card: AgentCard = serde_json::from_value(json!({
        "name": name, "description": description, "version": "0.1.0",
        "supportedInterfaces": [{"url": format!("{}/a2a", base_url.trim_end_matches('/')), "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}],
        "capabilities": {"streaming": false, "pushNotifications": false, "extendedAgentCard": false},
        "defaultInputModes": ["text/plain", "application/json"],
        "defaultOutputModes": ["text/plain", "application/json"],
        "skills": [{"id": "procurement", "name": "Governed procurement", "description": description, "tags": ["procurement", "governance"]}],
        "securitySchemes": {"bearer": {"httpAuthSecurityScheme": {"scheme": "bearer", "bearerFormat": "opaque"}}},
        "securityRequirements": [{"bearer": []}]
    })).expect("static A2A AgentCard must match SDK schema");
    let protocol = a2a_server::jsonrpc::jsonrpc_router(handler)
        .layer(middleware::from_fn_with_state(credentials, authenticate));
    Router::new()
        .nest("/a2a", protocol)
        .merge(a2a_server::agent_card::agent_card_router(Arc::new(
            StaticAgentCard::new(card),
        )))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Example;
    fn task() -> Value {
        json!({"id":"task-1","contextId":"context-1","status":{"state":"TASK_STATE_AUTH_REQUIRED","timestamp":"2026-11-09T12:00:00Z"}})
    }
    #[async_trait]
    impl Gateway for Example {
        async fn send_message(&self, caller: &str, _: Value) -> Result<Value, String> {
            assert_eq!(caller, "buyer");
            Ok(task())
        }
        async fn get_task(&self, _: &str, _: &str) -> Result<Value, String> {
            Ok(task())
        }
        async fn list_tasks(&self, _: &str) -> Result<Vec<Value>, String> {
            Ok(vec![task()])
        }
        async fn cancel_task(&self, _: &str, _: &str) -> Result<Value, String> {
            Err("not_cancelable".into())
        }
    }

    #[tokio::test]
    async fn official_transport_card_auth_and_task_shapes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(
            Arc::new(Example),
            "Example",
            "Test",
            &base,
            HashMap::from([("secret".into(), "buyer".into())]),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let card: Value = client
            .get(format!("{base}/.well-known/agent-card.json"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let endpoint = card["supportedInterfaces"][0]["url"].as_str().unwrap();
        assert_eq!(card["supportedInterfaces"][0]["protocolVersion"], "1.0");
        assert_eq!(
            card["securitySchemes"]["bearer"]["httpAuthSecurityScheme"]["scheme"],
            "bearer"
        );
        assert_eq!(
            client.post(endpoint).send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let send = json!({"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{"message":{"messageId":"message-1","role":"ROLE_USER","parts":[{"text":"hello"}]}}});
        let response: Value = client
            .post(endpoint)
            .bearer_auth("secret")
            .header("A2A-Version", "1.0")
            .json(&send)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            response["result"]["task"]["status"]["state"], "TASK_STATE_AUTH_REQUIRED",
            "{response}"
        );
        let unsupported: Value = client
            .post(endpoint)
            .bearer_auth("secret")
            .header("A2A-Version", "0.3")
            .json(&send)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(unsupported["error"]["code"], -32009);
        let list: Value = client
            .post(endpoint)
            .bearer_auth("secret")
            .header("A2A-Version", "1.0")
            .json(&json!({"jsonrpc":"2.0","id":2,"method":"ListTasks","params":{}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["result"]["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(list["result"]["nextPageToken"], "");
        server.abort();
    }
}
