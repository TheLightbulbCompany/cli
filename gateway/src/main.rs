mod a2a;
mod agents;
mod policy;
mod store;

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Html,
    routing::{get, post},
};
use clap::Parser;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use store::{Approval, LabState, Store, Task};
use uuid::Uuid;

#[derive(Parser, Clone)]
#[command(about = "Lightbulb: a local A2A governance proof of concept")]
struct Args {
    #[command(subcommand)]
    command: Option<ClientCommand>,
    #[arg(long, global = true, default_value = "gateway")]
    role: String,
    #[arg(long, global = true, default_value_t = 8080)]
    port: u16,
    #[arg(long, global = true, default_value = "data/gateway")]
    data_dir: PathBuf,
    #[arg(long, global = true, default_value = "http://127.0.0.1:8081")]
    supplier_url: String,
    #[arg(long, global = true, default_value = "127.0.0.1")]
    host: String,
}
#[derive(clap::Subcommand, Clone)]
enum ClientCommand {
    /// Run the gateway/supplier server (also the default when no command is given).
    Serve,
    /// Inspect task state and approvals on a running gateway.
    Tasks {
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        url: String,
    },
    /// Start a synthetic business task.
    Demo {
        #[arg(long, default_value = "approval")]
        scenario: String,
        #[arg(long, default_value = "scripted")]
        mode: String,
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        url: String,
    },
    /// Approve exact terms using the named owner's credential.
    Approve {
        id: String,
        #[arg(long)]
        owner: String,
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        url: String,
    },
    /// Decline a pending proposal.
    Deny {
        id: String,
        #[arg(long)]
        owner: String,
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        url: String,
    },
    /// Reconcile an uncertain commit against the supplier's durable ledger.
    Retry {
        id: String,
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        url: String,
    },
    /// Export actual tasks, messages, policy decisions and outcomes as JSON.
    Evidence {
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        url: String,
    },
    /// Reset only the local synthetic demonstration data.
    Reset {
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        url: String,
    },
}
async fn client_command(command: ClientCommand) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let mut output = None;
    let approving = matches!(&command, ClientCommand::Approve { .. });
    let request = match command {
        ClientCommand::Tasks { url } => client.get(format!("{url}/api/state")),
        ClientCommand::Demo {
            scenario,
            mode,
            url,
        } => client
            .post(format!("{url}/api/demo"))
            .json(&json!({"scenario":scenario,"mode":mode})),
        ClientCommand::Approve { id, owner, url } | ClientCommand::Deny { id, owner, url } => {
            let decision = if approving { "approve" } else { "deny" };
            let token = match owner.as_str() {
                "buyer" => env("LIGHTBULB_BUYER_OWNER_TOKEN", "demo-buyer"),
                "supplier" => env("LIGHTBULB_SUPPLIER_OWNER_TOKEN", "demo-supplier"),
                _ => return Err("owner must be buyer or supplier".into()),
            };
            client
                .post(format!("{url}/api/approvals/{id}"))
                .header("X-Owner-Token", token)
                .json(&json!({"owner":owner,"decision":decision}))
        }
        ClientCommand::Retry { id, url } => client
            .post(format!("{url}/api/tasks/{id}/retry"))
            .header("X-Owner-Token", env("LIGHTBULB_OWNER_TOKEN", "demo-owner"))
            .json(&json!({})),
        ClientCommand::Evidence { url, output: path } => {
            output = path;
            client.get(format!("{url}/api/state"))
        }
        ClientCommand::Reset { url } => client
            .post(format!("{url}/api/reset"))
            .header("X-Owner-Token", env("LIGHTBULB_OWNER_TOKEN", "demo-owner"))
            .json(&json!({})),
        ClientCommand::Serve => return Ok(()),
    };
    let response = request.send().await?;
    let status = response.status();
    let value: Value = response.json().await?;
    if !status.is_success() {
        return Err(format!("gateway returned {status}: {value}").into());
    }
    let rendered = serde_json::to_string_pretty(&value)?;
    if let Some(path) = output {
        std::fs::write(path, rendered)?;
    } else {
        println!("{rendered}");
    }
    Ok(())
}
struct App {
    args: Args,
    core: Mutex<Store>,
    active: Mutex<HashSet<String>>,
    owner_token: String,
    buyer_owner_token: String,
    supplier_owner_token: String,
    message_lock: Mutex<()>,
    resetting: AtomicBool,
    supplier_token: String,
    execution_secret: String,
}
fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.into())
}
fn uid() -> String {
    Uuid::new_v4().to_string()
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<_, _> =
                map.iter().map(|(k, v)| (k.clone(), canonical(v))).collect();
            serde_json::to_value(sorted).expect("canonical map")
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        Value::Number(n) => {
            if let Some(f) = n.as_f64()
                && f.is_finite()
                && f.fract() == 0.0
                && f.abs() <= 9_007_199_254_740_991.0
            {
                return json!(f as i64);
            }
            v.clone()
        }
        _ => v.clone(),
    }
}
fn hash(v: &Value) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(canonical(v).to_string().as_bytes())
    )
}
fn error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error":message.into()})))
}
type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;
fn wire_task(t: &Task) -> Value {
    let status = match t.status.as_str() {
        "completed" => "TASK_STATE_COMPLETED",
        "auth_required" => "TASK_STATE_AUTH_REQUIRED",
        "rejected" => "TASK_STATE_REJECTED",
        "failed" => "TASK_STATE_FAILED",
        "canceled" => "TASK_STATE_CANCELED",
        _ => "TASK_STATE_WORKING",
    };
    let mut value =
        json!({"id":t.id,"contextId":t.id,"status":{"state":status,"timestamp":t.updated_at}});
    if let Some(result) = &t.result {
        value["artifacts"] = json!([{"artifactId":format!("{}-result",t.id),"name":"Business result","parts":[{"data":result}]}]);
    }
    if let Some(e) = &t.error {
        value["status"]["message"] =
            json!({"messageId":uid(),"role":"ROLE_AGENT","parts":[{"text":e}],"contextId":t.id});
    }
    value
}
fn new_task(caller: &str, scenario: &str, mode: &str) -> Task {
    let time = chrono::Utc::now().to_rfc3339();
    Task {
        id: uid(),
        caller: caller.into(),
        status: "working".into(),
        scenario: scenario.into(),
        mode: mode.into(),
        proposal: None,
        result: None,
        error: None,
        phase: 0,
        execution_state: None,
        created_at: time.clone(),
        updated_at: time,
    }
}
fn data(message: &Value) -> Value {
    message["parts"]
        .as_array()
        .and_then(|ps| ps.iter().find_map(|p| p.get("data").cloned()))
        .unwrap_or_else(|| json!({}))
}
fn validate_proposal(p: &Value, task: &str) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Proposal {
        task_id: String,
        buyer: String,
        supplier: String,
        sku: String,
        quantity: i64,
        delivery: String,
        currency: String,
        total_cents: i64,
        substitution: bool,
        discount_percent: i64,
        quote_version: i64,
        supplier_epoch: String,
    }
    let q: Proposal = serde_json::from_value(canonical(p))
        .map_err(|e| format!("invalid structured proposal: {e}"))?;
    if q.task_id != task
        || q.buyer != "buyer"
        || q.supplier != "supplier"
        || q.quantity != 20
        || q.delivery != "Friday"
        || q.currency != "USD"
        || q.total_cents < 0
        || q.total_cents > 1_000_000
        || !(0..=100).contains(&q.discount_percent)
        || q.quote_version < 1
        || q.supplier_epoch.is_empty()
        || !matches!(q.sku.as_str(), "ORIGINAL-CASE" | "ALTERNATIVE-CASE")
    {
        return Err("proposal violates the simulated business contract".into());
    }
    if (q.sku == "ALTERNATIVE-CASE") != q.substitution {
        return Err("proposal substitution flag disagrees with SKU".into());
    }
    Ok(())
}
fn allowed_scenario(s: &str) -> bool {
    matches!(
        s,
        "automatic" | "approval" | "supplier" | "disclosure" | "changed_terms"
    )
}

impl App {
    fn record(
        &self,
        id: &str,
        actor: &str,
        kind: &str,
        message: &str,
        data: Value,
    ) -> Result<(), String> {
        let mut core = self.core.lock().map_err(|_| "store unavailable")?;
        core.event(id, actor, kind, message, data);
        core.save()
    }
    fn create(&self, caller: &str, scenario: &str, mode: &str) -> Result<Task, String> {
        if !allowed_scenario(scenario) {
            return Err("unknown scenario".into());
        }
        if !matches!(mode, "scripted" | "live") {
            return Err("unknown runner mode".into());
        }
        if mode == "live" && !agents::live_ready() {
            return Err(
                "Live mode requires a configured model and provider credentials on both services"
                    .into(),
            );
        }
        let t = new_task(caller, scenario, mode);
        let mut core = self.core.lock().map_err(|_| "store unavailable")?;
        if self.resetting.load(Ordering::SeqCst) {
            return Err("reset in progress".into());
        }
        core.state.tasks.push(t.clone());
        core.event(
            &t.id,
            "buyer.procurement",
            "task",
            "Order 20 cases for Friday. Auto-spend ≤ $500. Substitutions need buyer approval.",
            json!({"scenario":scenario,"mode":mode,"budget_cents":50000}),
        );
        core.save()?;
        Ok(t)
    }
    fn engine(&self, task: &str, owner: &str) -> Result<policy::PolicyEngine, String> {
        std::fs::create_dir_all(self.args.data_dir.join("policies").join(task))
            .map_err(|e| e.to_string())?;
        policy::PolicyEngine::open(
            &self.args.data_dir.join("policies").join(task).join(owner),
            owner,
            task,
            50000,
        )
    }
    fn sign_permit(&self, value: &Value) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(self.execution_secret.as_bytes()).expect("HMAC key");
        mac.update(canonical(value).to_string().as_bytes());
        format!("{:x}", mac.finalize().into_bytes())
    }
    fn verify_permit(&self, permit: &Value, proposal: &Value) -> Result<(), String> {
        let claims = &permit["claims"];
        let sig = permit["signature"]
            .as_str()
            .ok_or("missing execution permit")?;
        if claims["proposal_hash"] != hash(proposal)
            || claims["task_id"] != proposal["task_id"]
            || claims["supplier_epoch"] != proposal["supplier_epoch"]
            || claims["expires_at"].as_i64().is_none_or(|e| e < now())
        {
            return Err("execution permit scope or expiry invalid".into());
        }
        let raw = (0..sig.len())
            .step_by(2)
            .map(|i| {
                sig.get(i..i + 2)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or("invalid signature")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut mac = Hmac::<Sha256>::new_from_slice(self.execution_secret.as_bytes())
            .map_err(|_| "invalid secret")?;
        mac.update(canonical(claims).to_string().as_bytes());
        mac.verify_slice(&raw)
            .map_err(|_| "invalid execution permit".into())
    }
    async fn supplier_call(&self, task: &str, action: Value) -> Result<Value, String> {
        let request = json!({"jsonrpc":"2.0","id":uid(),"method":"SendMessage","params":{"message":{"messageId":uid(),"contextId":task,"role":"ROLE_USER","parts":[{"data":action}]}}});
        self.record(
            task,
            "lightbulb",
            "a2a_request",
            "Forwarded a standard A2A request to the supplier service",
            request.clone(),
        )?;
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(100))
            .build()
            .map_err(|e| e.to_string())?
            .post(format!(
                "{}/a2a",
                self.args.supplier_url.trim_end_matches('/')
            ))
            .bearer_auth(&self.supplier_token)
            .header("A2A-Version", "1.0")
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("supplier connection failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("supplier HTTP {}", response.status()));
        }
        let reply: Value = response.json().await.map_err(|e| e.to_string())?;
        self.record(
            task,
            "supplier.sales",
            "a2a_response",
            "Received supplier A2A response",
            reply.clone(),
        )?;
        if let Some(e) = reply.get("error") {
            return Err(format!("supplier refused: {e}"));
        }
        let task = reply
            .pointer("/result/task")
            .ok_or("supplier result missing task")?;
        if task.pointer("/status/state").and_then(Value::as_str) != Some("TASK_STATE_COMPLETED") {
            return Err("supplier task did not complete".into());
        }
        task.pointer("/artifacts/0/parts/0/data")
            .map(canonical)
            .ok_or_else(|| "supplier result missing structured artifact".into())
    }
    fn schedule(app: Arc<Self>, id: String) {
        {
            let mut active = app.active.lock().expect("active lock");
            if !active.insert(id.clone()) {
                return;
            }
        }
        tokio::spawn(async move {
            if let Err(e) = app.run(&id).await {
                let _ = app.record(&id, "lightbulb", "error", &e, json!({}));
                if let Ok(mut c) = app.core.lock() {
                    let _ = c.update(&id, |t| {
                        if t.status != "canceled" {
                            t.status = if t.execution_state.as_deref() == Some("dispatched") {
                                "unknown"
                            } else {
                                "failed"
                            }
                            .into();
                            t.error = Some(e);
                        }
                    });
                }
            }
            app.active.lock().expect("active lock").remove(&id);
            // An owner may answer immediately after the hold was persisted, while
            // the previous runner is still finishing. Do not lose that wake-up.
            let resume = app
                .core
                .lock()
                .ok()
                .and_then(|c| c.task(&id).ok())
                .is_some_and(|t| t.status == "working");
            if resume {
                App::schedule(app.clone(), id);
            }
        });
    }
    async fn run(&self, id: &str) -> Result<(), String> {
        let task = self
            .core
            .lock()
            .map_err(|_| "store unavailable")?
            .task(id)?;
        if task.execution_state.as_deref() == Some("dispatched") && task.status == "working" {
            let found = self
                .supplier_call(id, json!({"action":"get_order","task_id":id}))
                .await?;
            if found["found"] == true {
                let order = found["order"].clone();
                if task.proposal.as_ref() != Some(&order["proposal"]) {
                    return Err("supplier recovery terms differ from dispatched proposal".into());
                }
                return self.finish(id, order);
            }
        }
        if task.status != "working" {
            return Ok(());
        }
        if task.proposal.is_none() {
            let observation =
                json!({"task_id":id,"quantity":20,"delivery":"Friday","scenario":task.scenario});
            let step = agents::step(
                &task.mode,
                "buyer Procurement",
                observation.clone(),
                "request_quote",
            )
            .await?;
            self.record(
                id,
                "buyer.procurement",
                "agent_action",
                "Requested availability and a quote",
                step,
            )?;
            let quote=self.supplier_call(id,json!({"action":"request_quote","task_id":id,"scenario":task.scenario,"mode":task.mode})).await?;
            self.record(
                id,
                "supplier.fulfillment",
                "inventory",
                "Supplier fulfillment checked its own stock and delivery fixture",
                quote.clone(),
            )?;
            if task.scenario == "disclosure" {
                let verdict = self.engine(id, "supplier")?.evaluate_export(true)?;
                self.record(id,"lightbulb","policy_deny","Supplier policy denied disclosure of confidential internal cost data",json!({"owner":"supplier","decision":verdict,"field":"internal_cost","data_released":false}))?;
                self.core
                    .lock()
                    .map_err(|_| "store unavailable")?
                    .update(id, |t| {
                        t.status = "rejected".into();
                        t.error =
                            Some("Supplier owner forbids confidential cost disclosure".into());
                    })?;
                return Ok(());
            }
            let proposal = quote["proposal"].clone();
            if !proposal.is_object() {
                return Err("supplier returned no proposal".into());
            }
            let step = agents::step(
                &task.mode,
                "buyer Operations",
                proposal.clone(),
                "propose_purchase",
            )
            .await?;
            self.record(
                id,
                "buyer.operations",
                "agent_action",
                "Checked delivery and quantity; proposed the supplier's exact terms",
                step,
            )?;
            self.core
                .lock()
                .map_err(|_| "store unavailable")?
                .update(id, |t| t.proposal = Some(proposal))?;
        }
        self.advance(id).await
    }
    async fn advance(&self, id: &str) -> Result<(), String> {
        // One active runner per task. This lock protects task/approval transitions; no I/O await while held.
        let (proposal, proposal_hash) = {
            let mut core = self.core.lock().map_err(|_| "store unavailable")?;
            let task = core.task(id)?;
            if task.status != "working" {
                return Ok(());
            }
            let proposal = task.proposal.ok_or("missing proposal")?;
            let proposal_hash = hash(&proposal);
            validate_proposal(&proposal, id)?;
            let amount = proposal["total_cents"]
                .as_i64()
                .ok_or("invalid quote total")?;
            let quantity = proposal["quantity"].as_i64().ok_or("invalid quantity")?;
            if amount < 0
                || quantity != 20
                || proposal["buyer"] != "buyer"
                || proposal["supplier"] != "supplier"
                || proposal["task_id"] != id
            {
                return Err("proposal failed business validation".into());
            }
            let buyer = self.engine(id, "buyer")?.evaluate_commit(
                &proposal_hash,
                amount,
                proposal["substitution"]
                    .as_bool()
                    .ok_or("substitution must be a boolean")?,
            )?;
            let seller = self.engine(id, "supplier")?.evaluate_discount(
                &proposal_hash,
                proposal["discount_percent"]
                    .as_i64()
                    .ok_or("discount must be integer")?,
            )?;
            let mut held = false;
            for (owner, decision) in [("buyer", buyer), ("supplier", seller)] {
                core.event(
                    id,
                    "lightbulb",
                    if decision.allowed {
                        "policy_allow"
                    } else if decision.requires_approval {
                        "policy_hold"
                    } else {
                        "policy_deny"
                    },
                    &format!(
                        "{owner} policy: {}",
                        if decision.allowed {
                            "allow"
                        } else if decision.requires_approval {
                            "owner approval required"
                        } else {
                            "deny"
                        }
                    ),
                    json!({"owner":owner,"proposal_hash":proposal_hash,"decision":decision}),
                );
                if !decision.allowed {
                    if !decision.requires_approval {
                        core.update(id, |t| {
                            t.status = "rejected".into();
                            t.error = Some(format!("{owner} policy denied the action"));
                        })?;
                        return Ok(());
                    }
                    held = true;
                    let exists = core.state.approvals.iter().any(|a| {
                        a.task_id == id
                            && a.owner == owner
                            && a.proposal_hash == proposal_hash
                            && a.status == "pending"
                    });
                    if !exists {
                        core.state.approvals.push(Approval {
                            id: uid(),
                            task_id: id.into(),
                            owner: owner.into(),
                            status: "pending".into(),
                            proposal: proposal.clone(),
                            proposal_hash: proposal_hash.clone(),
                            expires_at: now() + 900,
                        });
                    }
                }
            }
            if held {
                core.update(id, |t| t.status = "auth_required".into())?;
                return Ok(());
            }
            core.update(id, |t| t.execution_state = Some("dispatched".into()))?;
            core.event(
                id,
                "lightbulb",
                "execution",
                "Both owner policies allow. Issuing an exact-proposal execution permit.",
                json!({"proposal_hash":proposal_hash}),
            );
            core.save()?;
            (proposal, proposal_hash)
        };
        let claims = json!({"task_id":id,"proposal_hash":proposal_hash,"supplier_epoch":proposal["supplier_epoch"],"expires_at":now()+300});
        let permit = json!({"signature":self.sign_permit(&claims),"claims":claims});
        let result = self
            .supplier_call(
                id,
                json!({"action":"commit_order","task_id":id,"proposal":proposal,"permit":permit}),
            )
            .await?;
        if std::env::var("LIGHTBULB_INJECT_LOST_COMMIT_RESPONSE").as_deref() == Ok("1") {
            let path = self.args.data_dir.join(format!("lost-response-{id}"));
            if std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .is_ok()
            {
                return Err("Injected lost commit response: outcome unknown; retry to reconcile supplier ledger".into());
            }
        }
        self.finish(id, result)
    }
    fn finish(&self, id: &str, result: Value) -> Result<(), String> {
        let mut core = self.core.lock().map_err(|_| "store unavailable")?;
        let task = core.task(id)?;
        let proposal = task.proposal.ok_or("missing recovery proposal")?;
        self.engine(id, "buyer")?.record_commit_result(
            &hash(&proposal),
            proposal["total_cents"].as_i64().ok_or("invalid amount")?,
            proposal["substitution"]
                .as_bool()
                .ok_or("invalid substitution")?,
            true,
        )?;
        if !core.state.orders.iter().any(|o| o["task_id"] == id) {
            core.state.orders.push(result.clone());
        }
        core.event(
            id,
            "supplier.fulfillment",
            "order_confirmed",
            "Supplier confirmed one order; duplicate requests reuse the supplier ledger",
            result.clone(),
        );
        core.update(id, |t| {
            t.status = "completed".into();
            t.execution_state = Some("confirmed".into());
            t.error = None;
            t.result = Some(result);
        })?;
        Ok(())
    }
    async fn supplier_message(&self, caller: &str, message: Value) -> Result<Value, String> {
        if caller != "gateway" {
            return Err("forbidden supplier caller".into());
        }
        let request = canonical(&data(&message));
        let action = request["action"].as_str().ok_or("action required")?;
        let task_id = request["task_id"].as_str().ok_or("task_id required")?;
        if message["contextId"].as_str().is_some_and(|c| c != task_id) {
            return Err("message context differs from business task".into());
        }
        let mut t = new_task(caller, action, "scripted");
        t.id = task_id.into();
        let result = match action {
            "request_quote" => {
                let scenario = request["scenario"].as_str().unwrap_or("approval");
                if !allowed_scenario(scenario) {
                    return Err("unknown scenario".into());
                }
                let mode = request["mode"].as_str().unwrap_or("scripted");
                let obs = json!({"task_id":task_id,"scenario":scenario,"quantity":20,"delivery":"Friday"});
                let sales =
                    agents::step(mode, "supplier Sales", obs.clone(), "check_inventory").await?;
                self.record(
                    task_id,
                    "supplier.sales",
                    "agent_action",
                    "Sales asked fulfillment for availability",
                    sales,
                )?;
                let fulfillment =
                    agents::step(mode, "supplier Fulfillment", obs, "return_quote").await?;
                self.record(
                    task_id,
                    "supplier.fulfillment",
                    "agent_action",
                    "Fulfillment checked stock and prepared an available offer",
                    fulfillment,
                )?;
                let (sku, total, substitution, discount) = match scenario {
                    "automatic" => ("ORIGINAL-CASE", 40000, false, 0),
                    "supplier" => ("ORIGINAL-CASE", 36000, false, 15),
                    _ => ("ALTERNATIVE-CASE", 52000, true, 0),
                };
                json!({"available_original":if scenario=="automatic"||scenario=="supplier"{20}else{12},"proposal":{"task_id":task_id,"buyer":"buyer","supplier":"supplier","sku":sku,"quantity":20,"delivery":"Friday","currency":"USD","total_cents":total,"substitution":substitution,"discount_percent":discount,"quote_version":1,"supplier_epoch":self.core.lock().map_err(|_|"store unavailable")?.state.epoch},"source":"synthetic supplier inventory"})
            }
            "get_order" => {
                let core = self.core.lock().map_err(|_| "store unavailable")?;
                match core.state.orders.iter().find(|o| o["task_id"] == task_id) {
                    Some(order) => json!({"found":true,"order":order}),
                    None => json!({"found":false}),
                }
            }
            "commit_order" => {
                let proposal = &request["proposal"];
                validate_proposal(proposal, task_id)?;
                self.verify_permit(&request["permit"], proposal)?;
                let mut core = self.core.lock().map_err(|_| "store unavailable")?;
                if request["permit"]["claims"]["supplier_epoch"] != core.state.epoch
                    || proposal["supplier_epoch"] != core.state.epoch
                {
                    return Err("execution permit belongs to a previous reset generation".into());
                }
                if let Some(order) = core.state.orders.iter().find(|o| o["task_id"] == task_id) {
                    if order["proposal"] != *proposal {
                        return Err("task already committed with different terms".into());
                    }
                    order.clone()
                } else {
                    let order = json!({"id":format!("order-{task_id}"),"task_id":task_id,"proposal":proposal,"status":"confirmed","simulated":true,"reserved_quantity":20});
                    core.state.orders.push(order.clone());
                    core.event(
                        task_id,
                        "supplier.fulfillment",
                        "order_confirmed",
                        "Inventory reservation and order committed atomically in supplier ledger",
                        order.clone(),
                    );
                    core.save()?;
                    order
                }
            }
            _ => return Err("unsupported supplier action".into()),
        };
        t.status = "completed".into();
        t.result = Some(result);
        t.updated_at = chrono::Utc::now().to_rfc3339();
        let mut core = self.core.lock().map_err(|_| "store unavailable")?;
        if let Some(old) = core.state.tasks.iter_mut().find(|x| x.id == t.id) {
            *old = t.clone();
        } else {
            core.state.tasks.push(t.clone());
        }
        core.save()?;
        Ok(wire_task(&t))
    }
}

struct Endpoint(Arc<App>);
#[async_trait]
impl a2a::Gateway for Endpoint {
    async fn send_message(&self, caller: &str, message: Value) -> Result<Value, String> {
        if self.0.args.role == "supplier" {
            return self.0.supplier_message(caller, message).await;
        }
        if let Some(id) = message.get("taskId").and_then(Value::as_str) {
            let t = self
                .0
                .core
                .lock()
                .map_err(|_| "store unavailable")?
                .task(id)?;
            if t.caller != caller {
                return Err("forbidden task".into());
            }
            return Ok(wire_task(&t));
        }
        let _message_guard = self
            .0
            .message_lock
            .lock()
            .map_err(|_| "message ledger unavailable")?;
        let d = data(&message);
        let scenario = d["scenario"].as_str().unwrap_or("approval");
        let mode = d["mode"].as_str().unwrap_or("scripted");
        // messageId is only transport idempotency; order idempotency is enforced separately.
        let message_id = message["messageId"].as_str().ok_or("messageId required")?;
        let key = format!("message:{caller}:{message_id}");
        let message_hash = hash(&message);
        {
            let c = self.0.core.lock().map_err(|_| "store unavailable")?;
            if let Some(e) = c
                .state
                .events
                .iter()
                .find(|e| e["kind"] == "a2a_dedup" && e["data"]["key"] == key)
            {
                if e["data"]["message_hash"] != message_hash {
                    return Err("messageId reused with different content".into());
                }
                let t = c.task(e["task_id"].as_str().ok_or("invalid ledger")?)?;
                return Ok(wire_task(&t));
            }
        }
        let t = self.0.create(caller, scenario, mode)?;
        self.0.record(
            &t.id,
            "lightbulb",
            "a2a_dedup",
            "Accepted external A2A task",
            json!({"key":key,"message_hash":message_hash,"message":message}),
        )?;
        App::schedule(self.0.clone(), t.id.clone());
        Ok(wire_task(&t))
    }
    async fn get_task(&self, caller: &str, id: &str) -> Result<Value, String> {
        let t = self
            .0
            .core
            .lock()
            .map_err(|_| "store unavailable")?
            .task(id)?;
        if t.caller != caller {
            return Err("forbidden task".into());
        }
        Ok(wire_task(&t))
    }
    async fn list_tasks(&self, caller: &str) -> Result<Vec<Value>, String> {
        Ok(self
            .0
            .core
            .lock()
            .map_err(|_| "store unavailable")?
            .state
            .tasks
            .iter()
            .filter(|t| t.caller == caller)
            .map(wire_task)
            .collect())
    }
    async fn cancel_task(&self, caller: &str, id: &str) -> Result<Value, String> {
        let mut c = self.0.core.lock().map_err(|_| "store unavailable")?;
        let t = c.task(id)?;
        if t.caller != caller {
            return Err("forbidden task".into());
        }
        if t.status != "auth_required" {
            return Err("task not_cancelable at this execution stage".into());
        }
        c.update(id, |t| t.status = "canceled".into())?;
        for a in &mut c.state.approvals {
            if a.task_id == id && a.status == "pending" {
                a.status = "denied".into();
            }
        }
        c.save()?;
        Ok(wire_task(&c.task(id)?))
    }
}
fn owner(app: &App, headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    if headers.get("X-Owner-Token").and_then(|v| v.to_str().ok()) != Some(&app.owner_token) {
        return Err(error(StatusCode::UNAUTHORIZED, "owner credential required"));
    }
    Ok(())
}
async fn health(State(app): State<Arc<App>>) -> Json<Value> {
    Json(
        json!({"ok":true,"role":app.args.role,"protocol":"A2A 1.0","policy":"Dogwood Local Engine"}),
    )
}
async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}
async fn state(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    if app.args.role == "supplier"
        && headers.get("Authorization").and_then(|h| h.to_str().ok())
            != Some(format!("Bearer {}", app.supplier_token).as_str())
    {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "supplier credential required",
        ));
    }
    let core = app
        .core
        .lock()
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable"))?;
    let mut value = serde_json::to_value(&core.state)
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    value["mode"] = json!("scripted");
    value["live_available"] = json!(agents::live_ready());
    value["role"] = json!(app.args.role);
    value["agents"] = json!([{"id":"buyer.procurement","name":"Procurement","owner":"buyer"},{"id":"buyer.operations","name":"Operations","owner":"buyer"},{"id":"supplier.sales","name":"Sales","owner":"supplier"},{"id":"supplier.fulfillment","name":"Fulfillment","owner":"supplier"}]);
    value["policies"] = json!([{"owner":"buyer","description":"Auto-commit up to $500; substitutions require exact-proposal approval"},{"owner":"supplier","description":"Discounts over 10% require supplier approval; confidential costs cannot be exported"}]);
    Ok(Json(value))
}
async fn demo(State(app): State<Arc<App>>, Json(body): Json<Value>) -> ApiResult {
    if app.args.role != "gateway" {
        return Err(error(StatusCode::NOT_FOUND, "gateway only"));
    }
    let t = app
        .create(
            "demo",
            body["scenario"].as_str().unwrap_or("approval"),
            body["mode"].as_str().unwrap_or("scripted"),
        )
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
    App::schedule(app, t.id.clone());
    Ok(Json(json!({"task_id":t.id})))
}
async fn approve(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ApiResult {
    let credential = headers
        .get("X-Owner-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if credential != app.owner_token
        && credential != app.buyer_owner_token
        && credential != app.supplier_owner_token
    {
        return Err(error(StatusCode::UNAUTHORIZED, "owner credential required"));
    }
    let decision = body["decision"]
        .as_str()
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "decision required"))?;
    if !matches!(decision, "approve" | "deny") {
        return Err(error(StatusCode::BAD_REQUEST, "invalid decision"));
    }
    let task_id = {
        let mut c = app
            .core
            .lock()
            .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable"))?;
        let a = c
            .state
            .approvals
            .iter()
            .find(|a| a.id == id)
            .cloned()
            .ok_or_else(|| error(StatusCode::NOT_FOUND, "approval not found"))?;
        if app.resetting.load(Ordering::SeqCst) {
            return Err(error(StatusCode::CONFLICT, "reset in progress"));
        }
        let required = if a.owner == "buyer" {
            &app.buyer_owner_token
        } else {
            &app.supplier_owner_token
        };
        if credential != app.owner_token && credential != required {
            return Err(error(
                StatusCode::FORBIDDEN,
                "credential belongs to a different owner",
            ));
        }
        if body["owner"] != a.owner {
            return Err(error(
                StatusCode::FORBIDDEN,
                "only the proposal's owner can decide",
            ));
        }
        if a.status != "pending" {
            return Err(error(StatusCode::CONFLICT, "approval already resolved"));
        }
        if a.expires_at <= now() {
            c.state
                .approvals
                .iter_mut()
                .find(|x| x.id == id)
                .unwrap()
                .status = "expired".into();
            c.update(&a.task_id, |t| {
                t.status = "rejected".into();
                t.error = Some("Approval expired; start a new task for current terms".into());
            })
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            c.save()
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            return Err(error(StatusCode::CONFLICT, "approval expired"));
        }
        let task = c
            .task(&a.task_id)
            .map_err(|e| error(StatusCode::NOT_FOUND, e))?;
        if task.status != "auth_required"
            || task.proposal.as_ref().map(hash).as_deref() != Some(a.proposal_hash.as_str())
        {
            return Err(error(StatusCode::CONFLICT, "proposal is no longer current"));
        }
        app.engine(&a.task_id, &a.owner)
            .and_then(|mut p| {
                if decision == "approve" {
                    p.approve(&a.owner, &a.proposal_hash)
                } else {
                    p.revoke(&a.owner, &a.proposal_hash)
                }
            })
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        c.state
            .approvals
            .iter_mut()
            .find(|x| x.id == id)
            .unwrap()
            .status = if decision == "approve" {
            "approved"
        } else {
            "denied"
        }
        .into();
        c.event(
            &a.task_id,
            &format!("{}.owner", a.owner),
            "approval",
            &format!(
                "Owner {} the exact proposal",
                if decision == "approve" {
                    "approved"
                } else {
                    "declined"
                }
            ),
            json!({"approval_id":id,"proposal_hash":a.proposal_hash,"proposal":a.proposal}),
        );
        if decision == "deny" {
            c.update(&a.task_id, |t| t.status = "rejected".into())
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        } else {
            c.update(&a.task_id, |t| {
                t.status = "working".into();
                if t.scenario == "changed_terms" && t.phase == 0 {
                    t.phase = 1;
                    if let Some(p) = &mut t.proposal {
                        p["total_cents"] = json!(54500);
                        p["quote_version"] = json!(2);
                    }
                }
            })
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            if task.scenario == "changed_terms" && task.phase == 0 {
                c.event(&a.task_id,"supplier.sales","quote_changed","Failure-injection fixture changed the total after approval. Previous consent must not authorize these terms.",json!({"old_hash":a.proposal_hash,"new_total_cents":54500}));
                c.save()
                    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            }
        }
        a.task_id
    };
    if decision == "approve" {
        App::schedule(app, task_id.clone());
    }
    Ok(Json(json!({"task_id":task_id,"decision":decision})))
}
struct ResetGuard<'a>(&'a AtomicBool);
impl Drop for ResetGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
async fn retry(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult {
    owner(&app, &headers)?;
    {
        let mut c = app
            .core
            .lock()
            .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "store unavailable"))?;
        let t = c.task(&id).map_err(|e| error(StatusCode::NOT_FOUND, e))?;
        if t.status != "unknown" {
            return Err(error(
                StatusCode::CONFLICT,
                "only an unknown commit outcome can be reconciled",
            ));
        }
        c.update(&id, |t| t.status = "working".into())
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    App::schedule(app, id.clone());
    Ok(Json(json!({"task_id":id})))
}
async fn reset(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    if app.args.role == "supplier" {
        if headers.get("Authorization").and_then(|h| h.to_str().ok())
            != Some(format!("Bearer {}", app.supplier_token).as_str())
        {
            return Err(error(
                StatusCode::UNAUTHORIZED,
                "supplier credential required",
            ));
        }
    } else {
        owner(&app, &headers)?;
    }
    if app
        .resetting
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(error(StatusCode::CONFLICT, "reset in progress"));
    }
    let _reset_guard = ResetGuard(&app.resetting);
    let working = app
        .core
        .lock()
        .unwrap()
        .state
        .tasks
        .iter()
        .any(|t| t.status == "working");
    if working || !app.active.lock().unwrap().is_empty() {
        return Err(error(
            StatusCode::CONFLICT,
            "wait for active tasks before resetting",
        ));
    }
    if app.args.role == "gateway" {
        let response = reqwest::Client::new()
            .post(format!("{}/api/reset", app.args.supplier_url))
            .bearer_auth(&app.supplier_token)
            .send()
            .await
            .map_err(|e| error(StatusCode::BAD_GATEWAY, e.to_string()))?;
        if !response.status().is_success() {
            return Err(error(StatusCode::BAD_GATEWAY, "supplier reset failed"));
        }
    }
    let mut c = app.core.lock().unwrap();
    c.state = LabState::default();
    c.save()
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({"ok":true})))
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if let Some(command) = args.command.clone()
        && !matches!(command, ClientCommand::Serve)
    {
        return client_command(command).await;
    }
    if !matches!(args.role.as_str(), "gateway" | "supplier") {
        return Err("role must be gateway or supplier".into());
    }
    if args.host != "127.0.0.1" {
        return Err("This local proof of concept binds only 127.0.0.1; public deployment needs a separate security review".into());
    }
    let app = Arc::new(App {
        core: Mutex::new(Store::open(&args.data_dir.join("state.sqlite"))?),
        active: Mutex::new(HashSet::new()),
        owner_token: env("LIGHTBULB_OWNER_TOKEN", "demo-owner"),
        buyer_owner_token: env("LIGHTBULB_BUYER_OWNER_TOKEN", "demo-buyer"),
        supplier_owner_token: env("LIGHTBULB_SUPPLIER_OWNER_TOKEN", "demo-supplier"),
        message_lock: Mutex::new(()),
        resetting: AtomicBool::new(false),
        supplier_token: env("LIGHTBULB_SUPPLIER_TOKEN", "internal-demo"),
        execution_secret: env("LIGHTBULB_EXECUTION_SECRET", "local-demo-secret"),
        args: args.clone(),
    });
    let credentials = if args.role == "supplier" {
        HashMap::from([(app.supplier_token.clone(), "gateway".into())])
    } else {
        HashMap::from([
            (
                env("LIGHTBULB_BUYER_TOKEN", "buyer-demo"),
                "buyer-agent".into(),
            ),
            (
                env("LIGHTBULB_OTHER_TOKEN", "other-demo"),
                "other-agent".into(),
            ),
        ])
    };
    let base = format!("http://{}:{}", args.host, args.port);
    let api = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/api/state", get(state))
        .route("/api/demo", post(demo))
        .route("/api/approvals/{id}", post(approve))
        .route("/api/reset", post(reset))
        .route("/api/tasks/{id}/retry", post(retry))
        .with_state(app.clone());
    let router = api.merge(a2a::router(
        Arc::new(Endpoint(app.clone())),
        &format!("Lightbulb {}", args.role),
        "Governed synthetic procurement agents",
        &base,
        credentials,
    ));
    let listener = tokio::net::TcpListener::bind(format!("{}:{}", args.host, args.port)).await?;
    println!(
        "Lightbulb {} listening at {base} (local synthetic business systems; scripted default)",
        args.role
    );
    if args.role == "gateway" {
        let pending = {
            let mut c = app.core.lock().unwrap();
            for t in &mut c.state.tasks {
                if t.status == "unknown" {
                    t.status = "working".into();
                }
            }
            c.save()?;
            c.state
                .tasks
                .iter()
                .filter(|t| t.status == "working")
                .map(|t| t.id.clone())
                .collect::<Vec<_>>()
        };
        for id in pending {
            App::schedule(app.clone(), id);
        }
    }
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
