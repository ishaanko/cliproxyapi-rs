//! Service-level tests: startup registration and incremental reloads, driven through real files
//! and watchers with a fake [`ManagerPort`] (the conductor `Manager` is not implemented here).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cpa_auth::{Auth, Store};
use cpa_core::registry::{ModelRegistry, get_claude_models};
use parking_lot::Mutex;

use super::{ManagerPort, ServiceBuilder};
use crate::executor::{DynExecutor, ExecError, Executor, Options, Request, Response, StreamResult};

#[derive(Default)]
struct FakePort {
    auths: Mutex<BTreeMap<String, Auth>>,
    executors: Mutex<Vec<String>>,
    registered: Mutex<Vec<String>>,
}

#[async_trait]
impl ManagerPort for FakePort {
    fn register_executor(&self, executor: DynExecutor) {
        self.executors.lock().push(executor.identifier().to_string());
    }
    async fn update(&self, auth: Auth, persist: bool) -> Result<Auth, ExecError> {
        assert!(!persist, "the service never asks the manager to persist");
        self.auths.lock().insert(auth.id.clone(), auth.clone());
        Ok(auth)
    }
    async fn remove(&self, id: &str) {
        self.auths.lock().remove(id);
    }
    fn list(&self) -> Vec<Auth> {
        self.auths.lock().values().cloned().collect()
    }
    fn get(&self, id: &str) -> Option<Auth> {
        self.auths.lock().get(id).cloned()
    }
    async fn models_registered(&self, auth_id: &str) {
        self.registered.lock().push(auth_id.to_string());
    }
}

struct StubExecutor(&'static str);

#[async_trait]
impl Executor for StubExecutor {
    fn identifier(&self) -> &str {
        self.0
    }
    async fn execute(&self, _: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
        Err(ExecError::new(501, "stub"))
    }
    async fn execute_stream(&self, _: &Auth, _: Request, _: Options) -> Result<StreamResult, ExecError> {
        Err(ExecError::new(501, "stub"))
    }
    async fn refresh(&self, _: &Auth) -> Result<Auth, ExecError> {
        Err(ExecError::new(501, "stub"))
    }
    async fn count_tokens(&self, _: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
        Err(ExecError::new(501, "stub"))
    }
}

struct Env {
    dir: tempfile::TempDir,
    port: Arc<FakePort>,
    registry: &'static ModelRegistry,
}

impl Env {
    fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let env = Env { dir, port: Arc::default(), registry: Box::leak(Box::default()) };
        env.write_config(config);
        env
    }

    fn auth_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("auths")
    }

    fn write_config(&self, body: &str) {
        let yaml = format!("auth-dir: {}\n{body}", self.auth_dir().display());
        std::fs::write(self.dir.path().join("config.yaml"), yaml).unwrap();
    }

    fn write_config_raw(&self, yaml: &str) {
        std::fs::write(self.dir.path().join("config.yaml"), yaml).unwrap();
    }

    fn write_auth(&self, name: &str, json: &str) {
        std::fs::create_dir_all(self.auth_dir()).unwrap();
        std::fs::write(self.auth_dir().join(name), json).unwrap();
    }

    fn builder(&self) -> ServiceBuilder {
        ServiceBuilder::new(self.dir.path().join("config.yaml"))
            .dotenv_dir(None)
            .registry(self.registry)
            .manager_port(self.port.clone())
            .antigravity_probe(false)
    }

    fn model_ids(&self, client: &str) -> Vec<String> {
        self.registry.get_models_for_client(client).into_iter().map(|m| m.id).collect()
    }
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..300 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

const CLAUDE_KEY: &str = "claude-api-key:\n  - api-key: sk-1\n    models: [{name: claude-sonnet-4-5, alias: son}]\n";

fn config_auth_id(port: &FakePort) -> String {
    port.list().into_iter().find(|a| a.id.starts_with("claude:apikey:")).expect("config auth").id
}

#[tokio::test]
async fn start_registers_executors_auths_and_models() {
    let env = Env::new(&format!("{CLAUDE_KEY}openai-compatibility:\n  - name: Foo\n    base-url: https://foo.example\n    api-key-entries: [{{api-key: k}}]\n    models: [{{name: m, alias: fm}}]\n"));
    env.write_auth("claude-a.json", r#"{"type":"claude","email":"a@x","prefix":"team"}"#);
    env.write_auth("gemini-old.json", r#"{"type":"gemini"}"#);
    let service = env
        .builder()
        .executor(Arc::new(StubExecutor("claude")))
        .executor_factory(Arc::new(|key: &str| key.starts_with("openai-compatible-").then(|| Arc::new(StubExecutor("foo-compat")) as DynExecutor)))
        .watch(false)
        .build()
        .unwrap();
    service.start().await.unwrap();

    let ids: Vec<String> = env.port.list().into_iter().map(|a| a.id).collect();
    assert_eq!(ids.len(), 3, "{ids:?}");
    assert!(ids.contains(&"claude-a.json".to_string()));
    // Config key auth: its configured model only; file auth: catalog models plus the team/ copies.
    assert_eq!(env.model_ids(&config_auth_id(&env.port)), ["son"]);
    let file_models = env.model_ids("claude-a.json");
    let catalog = get_claude_models();
    assert_eq!(file_models.len(), catalog.len() * 2);
    assert!(file_models.contains(&format!("team/{}", catalog[0].id)));
    let compat = env.port.list().into_iter().find(|a| a.provider == "openai-compatible-foo").unwrap();
    assert_eq!(env.model_ids(&compat.id), ["fm"]);
    assert_eq!(*env.port.executors.lock(), ["claude", "foo-compat"]);
    assert!(env.port.registered.lock().contains(&"claude-a.json".to_string()));
    assert!(env.auth_dir().is_dir());
}

#[tokio::test]
async fn file_events_add_update_and_remove_auths() {
    let env = Env::new("");
    let service = env.builder().build().unwrap();
    service.start().await.unwrap();
    assert!(env.port.list().is_empty());

    env.write_auth("claude-b.json", r#"{"type":"claude","email":"b@x"}"#);
    wait_for("added auth", || env.port.get("claude-b.json").is_some()).await;
    let plain = env.model_ids("claude-b.json");
    assert_eq!(plain.len(), get_claude_models().len());

    env.write_auth("claude-b.json", r#"{"type":"claude","email":"b@x","prefix":"p","excluded_models":["claude-3-5*"]}"#);
    wait_for("prefix applied", || env.model_ids("claude-b.json").iter().any(|m| m.starts_with("p/"))).await;
    assert_eq!(env.port.get("claude-b.json").unwrap().prefix, "p");

    std::fs::remove_file(env.auth_dir().join("claude-b.json")).unwrap();
    wait_for("removed auth", || env.port.get("claude-b.json").is_none()).await;
    assert!(env.model_ids("claude-b.json").is_empty());
    service.shutdown();
}

#[tokio::test]
async fn config_reload_applies_the_reload_plan() {
    let env = Env::new(CLAUDE_KEY);
    env.write_auth("claude-a.json", r#"{"type":"claude","email":"a@x"}"#);
    let service = env.builder().build().unwrap();
    service.start().await.unwrap();
    let first = get_claude_models().into_iter().map(|m| m.id).find(|id| !id.contains("haiku")).unwrap();
    assert!(env.model_ids("claude-a.json").contains(&first));
    let key_auth = config_auth_id(&env.port);

    // An oauth-model-alias change forces every auth to re-register: the alias replaces the model.
    env.write_config(&format!(
        "{CLAUDE_KEY}oauth-model-alias:\n  claude:\n    - {{name: {first}, alias: zz-alias}}\n"
    ));
    assert!(service.reload_config().await);
    let models = env.model_ids("claude-a.json");
    assert!(models.contains(&"zz-alias".to_string()) && !models.contains(&first), "{models:?}");
    assert!(service.config().oauth_model_alias.contains_key("claude"));

    // Changed oauth-excluded-models rebuild the affected provider's registrations.
    env.write_config(&format!(
        "{CLAUDE_KEY}oauth-excluded-models:\n  claude: [\"*haiku*\"]\n"
    ));
    assert!(service.reload_config().await);
    assert!(env.model_ids("claude-a.json").iter().all(|m| !m.contains("haiku")));
    assert!(env.model_ids("claude-a.json").contains(&first));

    // Dropping the exclusion restores them; an invalid weight is refused; removing the key from the
    // config then deletes its auth and models.
    env.write_config(CLAUDE_KEY);
    assert!(service.reload_config().await);
    assert!(env.model_ids("claude-a.json").iter().any(|m| m.contains("haiku")));
    env.write_config("claude-api-key:\n  - api-key: sk-1\n    weight: 2000000\n");
    assert!(!service.reload_config().await);
    assert!(env.port.get(&key_auth).is_some());
    env.write_config("");
    assert!(service.reload_config().await);
    assert!(env.port.get(&key_auth).is_none());
    assert!(env.model_ids(&key_auth).is_empty());
    assert!(env.port.get("claude-a.json").is_some());
    service.shutdown();
}

#[tokio::test]
async fn auth_dir_change_rescans_and_rewatches_the_new_directory() {
    let env = Env::new("");
    env.write_auth("old.json", r#"{"type":"claude","email":"o@x"}"#);
    let service = env.builder().build().unwrap();
    service.start().await.unwrap();
    assert!(env.port.get("old.json").is_some());

    let other = env.dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("new.json"), r#"{"type":"claude","email":"n@x"}"#).unwrap();
    env.write_config_raw(&format!("auth-dir: {}\n", other.display()));
    assert!(service.reload_config().await);
    assert!(env.port.get("old.json").is_none());
    assert!(env.model_ids("old.json").is_empty());
    assert!(env.port.get("new.json").is_some());
    assert_eq!(service.store().base_dir().as_deref(), Some(other.as_path()));

    // The new directory is watched, the old one no longer is.
    std::fs::write(other.join("later.json"), r#"{"type":"claude","email":"l@x"}"#).unwrap();
    wait_for("auth in the new dir", || env.port.get("later.json").is_some()).await;
    env.write_auth("stale.json", r#"{"type":"claude"}"#);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(env.port.get("stale.json").is_none());
    service.shutdown();
}

#[tokio::test]
async fn rejected_config_reports_failure() {
    let env = Env::new(CLAUDE_KEY);
    let service = env.builder().build().unwrap();
    service.start().await.unwrap();
    // `load_config` already refuses an out-of-range weight, so the reload publishes nothing.
    env.write_config("claude-api-key:\n  - api-key: sk-1\n    weight: 2000000\n");
    assert!(!service.reload_config().await);
    assert_eq!(service.config().claude_key[0].weight, None);
    service.shutdown();
}

#[derive(Default)]
struct RecordingPersister {
    calls: Mutex<Vec<String>>,
}

impl super::StorePersister for RecordingPersister {
    fn persist_config(&self) -> Result<(), String> {
        self.calls.lock().push("config".into());
        Ok(())
    }
    fn persist_auth_files(&self, message: &str, _paths: &[String]) -> Result<(), String> {
        self.calls.lock().push(message.to_string());
        Ok(())
    }
}

struct EmptyStore;

impl Store for EmptyStore {
    fn list(&self) -> Result<Vec<Auth>, cpa_auth::store::StoreError> {
        Ok(Vec::new())
    }
    fn save(&self, _: &mut Auth, _: cpa_auth::SaveOptions) -> Result<Option<std::path::PathBuf>, cpa_auth::store::StoreError> {
        Ok(None)
    }
    fn delete(&self, _: &str) -> Result<(), cpa_auth::store::StoreError> {
        Ok(())
    }
}

/// A remote store backend owns the auth dir (its spool) and receives every spool change.
#[tokio::test]
async fn store_backend_owns_auth_dir_and_receives_spool_changes() {
    let env = Env::new("");
    let spool = env.dir.path().join("spool-auths");
    std::fs::create_dir_all(&spool).unwrap();
    let persister = Arc::new(RecordingPersister::default());
    let backend = super::StoreBackend {
        store: Arc::new(EmptyStore),
        persister: persister.clone(),
        auth_dir: spool.clone(),
        cooldown: None,
    };
    let service = env.builder().store_backend(backend).build().unwrap();
    service.start().await.unwrap();
    // The config names another auth-dir; the backend's spool wins at build time and on reload.
    assert_eq!(std::path::Path::new(&service.config().auth_dir), spool);

    std::fs::write(spool.join("claude-s.json"), r#"{"type":"claude","email":"s@x"}"#).unwrap();
    wait_for("synced auth", || persister.calls.lock().iter().any(|c| c == "Sync auth claude-s.json")).await;
    assert!(env.port.get("claude-s.json").is_some());

    env.write_config("debug: true\n");
    assert!(service.reload_config().await);
    wait_for("persisted config", || persister.calls.lock().iter().any(|c| c == "config")).await;
    assert_eq!(std::path::Path::new(&service.config().auth_dir), spool);

    std::fs::remove_file(spool.join("claude-s.json")).unwrap();
    wait_for("removed auth", || persister.calls.lock().iter().any(|c| c == "Remove auth claude-s.json")).await;
    service.shutdown();
}
