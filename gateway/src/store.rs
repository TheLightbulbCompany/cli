use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
pub struct LabState {
    #[serde(default = "new_epoch")]
    pub epoch: String,
    pub tasks: Vec<Task>,
    pub approvals: Vec<Approval>,
    pub events: Vec<Value>,
    pub orders: Vec<Value>,
}
fn new_epoch() -> String {
    uuid::Uuid::new_v4().to_string()
}
impl Default for LabState {
    fn default() -> Self {
        Self {
            epoch: new_epoch(),
            tasks: vec![],
            approvals: vec![],
            events: vec![],
            orders: vec![],
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub caller: String,
    pub status: String,
    pub scenario: String,
    pub mode: String,
    pub proposal: Option<Value>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub phase: u32,
    #[serde(default)]
    pub execution_state: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub task_id: String,
    pub owner: String,
    pub status: String,
    pub proposal: Value,
    pub proposal_hash: String,
    pub expires_at: i64,
}
pub struct Store {
    pub state: LabState,
    db: Connection,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let db = Connection::open(path).map_err(|e| e.to_string())?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS lab_state (id INTEGER PRIMARY KEY CHECK(id=1), value TEXT NOT NULL);").map_err(|e| e.to_string())?;
        let state = match db.query_row("SELECT value FROM lab_state WHERE id=1", [], |r| {
            r.get::<_, String>(0)
        }) {
            Ok(raw) => {
                serde_json::from_str(&raw).map_err(|e| format!("invalid persisted state: {e}"))?
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => LabState::default(),
            Err(e) => return Err(e.to_string()),
        };
        Ok(Self { state, db })
    }
    pub fn save(&self) -> Result<(), String> {
        let raw = serde_json::to_string(&self.state).map_err(|e| e.to_string())?;
        self.db.execute("INSERT INTO lab_state(id,value) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET value=excluded.value", params![raw]).map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn event(&mut self, task: &str, actor: &str, kind: &str, message: &str, data: Value) {
        self.state.events.push(serde_json::json!({"id": uuid::Uuid::new_v4().to_string(), "task_id":task,"actor":actor,"kind":kind,"message":message,"data":data,"created_at":chrono::Utc::now().to_rfc3339()}));
    }
    pub fn task(&self, id: &str) -> Result<Task, String> {
        self.state
            .tasks
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .ok_or_else(|| "task not found".into())
    }
    pub fn update(&mut self, id: &str, f: impl FnOnce(&mut Task)) -> Result<(), String> {
        let task = self
            .state
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or("task not found")?;
        f(task);
        task.updated_at = chrono::Utc::now().to_rfc3339();
        self.save()
    }
}
