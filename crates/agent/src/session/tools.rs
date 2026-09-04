use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock, Weak};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::events::{OutstandingItem, ToolOutcome};
use super::ports::{Clock, FilePage, FileSystem, ProcessRequest, ProcessSpawner, SpawnedProcess};

pub const PROVIDER_CALL_NAME: &str = "call";

pub fn provider_call_definition() -> crate::session::model::ToolDefinition {
    crate::session::model::ToolDefinition {
        name: PROVIDER_CALL_NAME.into(),
        description: "Call one zork-agent logical tool. Use the tool's complete name and put all tool-specific parameters in arguments.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "tool": {"type": "string", "minLength": 1},
                "arguments": {"type": "object"}
            },
            "required": ["tool", "arguments"],
            "additionalProperties": false
        }),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DynamicCall {
    pub tool: String,
    pub arguments: Value,
}

impl DynamicCall {
    pub fn from_value(value: Value) -> Result<Self, DynamicCallError> {
        #[derive(Deserialize)]
        struct WireCall {
            tool: String,
            arguments: Map<String, Value>,
        }

        let call: WireCall = serde_json::from_value(value)
            .map_err(|error| DynamicCallError::Invalid(error.to_string()))?;
        if call.tool.trim().is_empty() {
            return Err(DynamicCallError::EmptyTool);
        }
        Ok(Self {
            tool: call.tool,
            arguments: Value::Object(call.arguments),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DynamicCallError {
    #[error("invalid dynamic call: {0}")]
    Invalid(String),
    #[error("dynamic call tool must not be empty")]
    EmptyTool,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ToolVersion(String);

impl ToolVersion {
    pub fn new(value: impl Into<String>) -> Result<Self, ToolDefinitionError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ToolDefinitionError::EmptyVersion);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolContract {
    pub name: String,
    pub version: ToolVersion,
    pub initial_description: String,
    pub detailed_description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolContext {
    pub session_id: String,
    pub invocation_id: String,
    pub workspace: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolExecution {
    pub outcome: ToolOutcome,
    pub data: Value,
    pub result_schema_version: u32,
    pub knowledge: Option<ToolKnowledge>,
}

impl ToolExecution {
    pub fn success(data: impl Into<Value>) -> Self {
        Self {
            outcome: ToolOutcome::Succeeded,
            data: data.into(),
            result_schema_version: 1,
            knowledge: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolKnowledge {
    Current { name: String, version: ToolVersion },
    Removed { name: String },
}

pub trait ToolImplementation: Send + Sync {
    fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: &'a Value,
    ) -> Pin<Box<dyn Future<Output = ToolExecution> + Send + 'a>>;
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolState {
    pub schema_version: u32,
    pub value: Value,
}

pub trait ToolCompatibility: Send + Sync {
    fn migrate_result(&self, schema_version: u32, value: Value) -> Result<Value, String>;

    fn migrate_state(&self, state: ToolState) -> Result<ToolState, String>;

    fn initial_state(&self) -> Option<ToolState>;

    fn fold(&self, state: Option<&ToolState>, result: &Value) -> Result<Option<ToolState>, String>;

    fn outstanding(&self, state: Option<&ToolState>) -> Vec<OutstandingItem>;
}

pub struct NoToolState;

impl ToolCompatibility for NoToolState {
    fn migrate_result(&self, schema_version: u32, value: Value) -> Result<Value, String> {
        if schema_version == 1 {
            Ok(value)
        } else {
            Err(format!(
                "unsupported result schema version {schema_version}"
            ))
        }
    }

    fn migrate_state(&self, state: ToolState) -> Result<ToolState, String> {
        Ok(state)
    }

    fn initial_state(&self) -> Option<ToolState> {
        None
    }

    fn fold(
        &self,
        _state: Option<&ToolState>,
        _result: &Value,
    ) -> Result<Option<ToolState>, String> {
        Ok(None)
    }

    fn outstanding(&self, _state: Option<&ToolState>) -> Vec<OutstandingItem> {
        Vec::new()
    }
}

pub struct ToolInstance {
    contract: ToolContract,
    implementation: Arc<dyn ToolImplementation>,
    compatibility: Arc<dyn ToolCompatibility>,
}

impl ToolInstance {
    pub fn new(
        contract: ToolContract,
        implementation: Arc<dyn ToolImplementation>,
        compatibility: Arc<dyn ToolCompatibility>,
    ) -> Result<Self, ToolDefinitionError> {
        validate_contract(&contract)?;
        Ok(Self {
            contract,
            implementation,
            compatibility,
        })
    }

    pub fn contract(&self) -> &ToolContract {
        &self.contract
    }

    pub fn compatibility(&self) -> Arc<dyn ToolCompatibility> {
        self.compatibility.clone()
    }

    pub fn prepare_arguments(&self, mut arguments: Value) -> Result<Value, String> {
        discard_unknown_fields(&mut arguments, &self.contract.input_schema);
        let validator = jsonschema::validator_for(&self.contract.input_schema)
            .map_err(|error| error.to_string())?;
        validator
            .validate(&arguments)
            .map_err(|error| error.to_string())?;
        Ok(arguments)
    }

    pub fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: &'a Value,
    ) -> Pin<Box<dyn Future<Output = ToolExecution> + Send + 'a>> {
        self.implementation.execute(context, arguments)
    }
}

fn discard_unknown_fields(value: &mut Value, schema: &Value) {
    match value {
        Value::Object(fields) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                fields.retain(|name, _| properties.is_some_and(|known| known.contains_key(name)));
            }
            for (name, field) in fields {
                let field_schema = properties.and_then(|known| known.get(name)).or_else(|| {
                    schema
                        .get("additionalProperties")
                        .filter(|value| value.is_object())
                });
                if let Some(field_schema) = field_schema {
                    discard_unknown_fields(field, field_schema);
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for item in items {
                    discard_unknown_fields(item, item_schema);
                }
            }
        }
        _ => {}
    }
}

impl fmt::Debug for ToolInstance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolInstance")
            .field("contract", &self.contract)
            .finish_non_exhaustive()
    }
}

struct RegistryEntry {
    current: Option<Arc<ToolInstance>>,
    compatibility: Arc<dyn ToolCompatibility>,
}

#[derive(Default)]
pub struct ToolRegistry {
    entries: RwLock<BTreeMap<String, RegistryEntry>>,
}

impl ToolRegistry {
    pub fn register(&self, instance: Arc<ToolInstance>) {
        let name = instance.contract().name.clone();
        let compatibility = instance.compatibility();
        self.entries
            .write()
            .expect("tool registry lock poisoned")
            .insert(
                name,
                RegistryEntry {
                    current: Some(instance),
                    compatibility,
                },
            );
    }

    pub fn remove(&self, name: &str) -> bool {
        let mut entries = self.entries.write().expect("tool registry lock poisoned");
        let Some(entry) = entries.get_mut(name) else {
            return false;
        };
        entry.current.take().is_some()
    }

    pub fn resolve(&self, name: &str, known: Option<&ToolVersion>) -> ToolResolution {
        let entries = self.entries.read().expect("tool registry lock poisoned");
        let Some(instance) = entries.get(name).and_then(|entry| entry.current.as_ref()) else {
            return ToolResolution::Unavailable;
        };
        if known == Some(&instance.contract().version) {
            ToolResolution::Ready(instance.clone())
        } else {
            ToolResolution::VersionChanged {
                current: instance.contract().version.clone(),
            }
        }
    }

    pub fn compatibility(&self, name: &str) -> Option<Arc<dyn ToolCompatibility>> {
        self.entries
            .read()
            .expect("tool registry lock poisoned")
            .get(name)
            .map(|entry| entry.compatibility.clone())
    }

    pub fn current_contract(&self, name: &str) -> Option<ToolContract> {
        self.entries
            .read()
            .expect("tool registry lock poisoned")
            .get(name)
            .and_then(|entry| entry.current.as_ref())
            .map(|instance| instance.contract().clone())
    }

    pub fn initial_catalog(&self) -> Vec<ToolIntroduction> {
        self.entries
            .read()
            .expect("tool registry lock poisoned")
            .values()
            .filter_map(|entry| entry.current.as_ref())
            .map(|instance| ToolIntroduction {
                name: instance.contract().name.clone(),
                version: instance.contract().version.clone(),
                description: instance.contract().initial_description.clone(),
            })
            .collect()
    }

    pub fn changes(&self, known: &BTreeMap<String, ToolVersion>) -> Vec<ToolChange> {
        let entries = self.entries.read().expect("tool registry lock poisoned");
        let mut changes = Vec::new();
        for (name, entry) in entries.iter() {
            let Some(instance) = &entry.current else {
                if known.contains_key(name) {
                    changes.push(ToolChange::Removed { name: name.clone() });
                }
                continue;
            };
            match known.get(name) {
                None => changes.push(ToolChange::Added {
                    name: name.clone(),
                    version: instance.contract().version.clone(),
                }),
                Some(version) if version != &instance.contract().version => {
                    changes.push(ToolChange::Updated {
                        name: name.clone(),
                        version: instance.contract().version.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        for name in known.keys() {
            if !entries.contains_key(name) {
                changes.push(ToolChange::Removed { name: name.clone() });
            }
        }
        changes.sort_by(|left, right| left.name().cmp(right.name()));
        changes
    }
}

pub fn tool_help_instance(
    registry: &Arc<ToolRegistry>,
    version: ToolVersion,
) -> Result<Arc<ToolInstance>, ToolDefinitionError> {
    Ok(Arc::new(ToolInstance::new(
        ToolContract {
            name: "tool.help".into(),
            version,
            initial_description: "Use tool.help when you need the current detailed usage of a specific logical tool.".into(),
            detailed_description: "Return the latest detailed usage and current version for one exact logical tool name. This does not search or recommend tools.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"tool": {"type": "string", "minLength": 1}},
                "required": ["tool"],
                "additionalProperties": false
            }),
        },
        Arc::new(ToolHelp {
            registry: Arc::downgrade(registry),
        }),
        Arc::new(NoToolState),
    )?))
}

struct ToolHelp {
    registry: Weak<ToolRegistry>,
}

impl ToolImplementation for ToolHelp {
    fn execute<'a>(
        &'a self,
        _context: &'a ToolContext,
        arguments: &'a Value,
    ) -> Pin<Box<dyn Future<Output = ToolExecution> + Send + 'a>> {
        Box::pin(async move {
            let Some(name) = arguments.get("tool").and_then(Value::as_str) else {
                return ToolExecution {
                    outcome: ToolOutcome::Failed,
                    data: error_data("tool.help requires an exact tool name."),
                    result_schema_version: 1,
                    knowledge: None,
                };
            };
            let Some(registry) = self.registry.upgrade() else {
                return ToolExecution {
                    outcome: ToolOutcome::Failed,
                    data: error_data("Tool registry is unavailable."),
                    result_schema_version: 1,
                    knowledge: None,
                };
            };
            let Some(contract) = registry.current_contract(name) else {
                return ToolExecution {
                    outcome: ToolOutcome::Failed,
                    data: serde_json::json!({
                        "error": format!("Tool {name} is unavailable."),
                        "tool": name,
                    }),
                    result_schema_version: 1,
                    knowledge: Some(ToolKnowledge::Removed {
                        name: name.to_owned(),
                    }),
                };
            };
            let ToolContract {
                name: tool_name,
                version,
                detailed_description,
                ..
            } = contract;
            ToolExecution {
                outcome: ToolOutcome::Succeeded,
                data: serde_json::json!({
                    "tool": tool_name,
                    "version": version.clone(),
                    "description": detailed_description,
                }),
                result_schema_version: 1,
                knowledge: Some(ToolKnowledge::Current {
                    name: name.to_owned(),
                    version,
                }),
            }
        })
    }
}

#[derive(Clone, Debug)]
pub enum ToolResolution {
    Ready(Arc<ToolInstance>),
    VersionChanged { current: ToolVersion },
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolIntroduction {
    pub name: String,
    pub version: ToolVersion,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolChange {
    Added { name: String, version: ToolVersion },
    Updated { name: String, version: ToolVersion },
    Removed { name: String },
}

impl ToolChange {
    pub fn name(&self) -> &str {
        match self {
            Self::Added { name, .. } | Self::Updated { name, .. } | Self::Removed { name } => name,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolDefinitionError {
    #[error("tool name must not be empty")]
    EmptyName,
    #[error("tool version must not be empty")]
    EmptyVersion,
    #[error("tool initial description must not be empty")]
    EmptyInitialDescription,
    #[error("tool detailed description must not be empty")]
    EmptyDetailedDescription,
    #[error("invalid tool input schema: {0}")]
    InvalidInputSchema(String),
}

fn validate_contract(contract: &ToolContract) -> Result<(), ToolDefinitionError> {
    if contract.name.trim().is_empty() {
        return Err(ToolDefinitionError::EmptyName);
    }
    if contract.initial_description.trim().is_empty() {
        return Err(ToolDefinitionError::EmptyInitialDescription);
    }
    if contract.detailed_description.trim().is_empty() {
        return Err(ToolDefinitionError::EmptyDetailedDescription);
    }
    if !contract.input_schema.is_object() {
        return Err(ToolDefinitionError::InvalidInputSchema(
            "schema must be a JSON object".into(),
        ));
    }
    jsonschema::validator_for(&contract.input_schema)
        .map_err(|error| ToolDefinitionError::InvalidInputSchema(error.to_string()))?;
    Ok(())
}

#[derive(Clone)]
pub struct BuiltinToolDependencies {
    pub environment: BTreeMap<String, String>,
    pub query: Arc<dyn super::query::SessionQuery>,
    pub clock: Arc<dyn Clock>,
    pub files: Arc<dyn FileSystem>,
    pub processes: Arc<dyn ProcessSpawner>,
}

pub fn register_builtin_tools(
    registry: &Arc<ToolRegistry>,
    dependencies: BuiltinToolDependencies,
) -> Result<(), ToolDefinitionError> {
    for (kind, contract) in builtin_contracts()? {
        registry.register(Arc::new(ToolInstance::new(
            contract,
            Arc::new(BuiltinTool {
                kind,
                dependencies: dependencies.clone(),
            }),
            Arc::new(NoToolState),
        )?));
    }
    registry.register(tool_help_instance(
        registry,
        ToolVersion::new("builtin-1")?,
    )?);
    Ok(())
}

#[derive(Clone, Copy)]
enum BuiltinKind {
    End,
    Wait,
    ToolCancel,
    HistoryList,
    FileRead,
    FileWrite,
    FileEdit,
    ShellRun,
}

fn builtin_contracts() -> Result<Vec<(BuiltinKind, ToolContract)>, ToolDefinitionError> {
    let version = || ToolVersion::new("builtin-1");
    Ok(vec![
        (
            BuiltinKind::End,
            ToolContract {
                name: super::events::END_TOOL_NAME.into(),
                version: version()?,
                initial_description: "Finish the current turn when the requested work is complete. Unfinished work is checked before the turn ends.".into(),
                detailed_description: "Call end when this turn is complete. If the runtime reports unfinished items, resolve them or call end again with acknowledge_outstanding=true to explicitly finish while leaving them outstanding.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"acknowledge_outstanding": {"type": "boolean"}},
                    "additionalProperties": false
                }),
            },
        ),
        (
            BuiltinKind::Wait,
            ToolContract {
                name: super::events::WAIT_TOOL_NAME.into(),
                version: version()?,
                initial_description: "Pause until a duration elapses or new mailbox input arrives.".into(),
                detailed_description: "Pause this session for seconds unless new mailbox input arrives first. Tool completions do not by themselves end this explicit wait.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "seconds": {"type": "number", "exclusiveMinimum": 0},
                        "reason": {"type": "string"}
                    },
                    "required": ["seconds"],
                    "additionalProperties": false
                }),
            },
        ),
        (
            BuiltinKind::ToolCancel,
            ToolContract {
                name: super::events::TOOL_CANCEL_NAME.into(),
                version: version()?,
                initial_description: "Request cancellation of one tool invocation by its zork-agent invocation ID.".into(),
                detailed_description: "Request cancellation of invocation_id. The request is persisted before cancellation is signalled. A cancellation request is not proof of cancellation; the target's later ToolResult is authoritative.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"invocation_id": {"type": "string", "minLength": 1}},
                    "required": ["invocation_id"],
                    "additionalProperties": false
                }),
            },
        ),
        (
            BuiltinKind::HistoryList,
            ToolContract {
                name: super::events::HISTORY_LIST_NAME.into(),
                version: version()?,
                initial_description: "List bounded durable session history pages. Use file.read on a returned zork://history URI for full event content.".into(),
                detailed_description: "List at most limit visible events strictly before before_event_id, newest page first in durable order. Omit before_event_id for the most recent page. Read a specific returned event with file.read(path=\"zork://history/<event-id>\").".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "before_event_id": {"type": "string", "minLength": 16, "maxLength": 16},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 200}
                    },
                    "additionalProperties": false
                }),
            },
        ),
        (
            BuiltinKind::FileRead,
            ToolContract {
                name: super::events::FILE_READ_NAME.into(),
                version: version()?,
                initial_description: "Read files or zork://history/<event-id> with {path, offset?, limit?}. offset is a zero-based byte offset (default 0), limit is bytes (default 65536). Continue from the returned next_offset. start/end and line numbers are not read parameters.".into(),
                detailed_description: "Read at most limit bytes beginning at zero-based byte offset. Relative filesystem paths resolve from the session workspace. The same offset/limit contract applies to ordinary files and durable history events. Continue from next_offset until it is absent.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "minLength": 1},
                        "offset": {"type": "integer", "minimum": 0},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1048576}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            },
        ),
        (
            BuiltinKind::FileWrite,
            ToolContract {
                name: super::events::FILE_WRITE_NAME.into(),
                version: version()?,
                initial_description: "Write complete file content, creating parent directories.".into(),
                detailed_description: "Replace path with content, creating parent directories as needed. Relative paths resolve from the session workspace; absolute paths and parent traversal are allowed because the workspace is not a sandbox.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "minLength": 1},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }),
            },
        ),
        (
            BuiltinKind::FileEdit,
            ToolContract {
                name: super::events::FILE_EDIT_NAME.into(),
                version: version()?,
                initial_description: "Apply exact text replacements with {path, edits:[{old_text, new_text}]}. Each old_text must occur exactly once; replacements cannot overlap.".into(),
                detailed_description: "Each edits[].old_text must occur exactly once in the original file. All matches are resolved against the original content, replacements must not overlap, and the file is written once after validation.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "minLength": 1},
                        "edits": {
                            "type": "array", "minItems": 1,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "old_text": {"type": "string"},
                                    "new_text": {"type": "string"}
                                },
                                "required": ["old_text", "new_text"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["path", "edits"],
                    "additionalProperties": false
                }),
            },
        ),
        (
            BuiltinKind::ShellRun,
            ToolContract {
                name: super::events::SHELL_RUN_NAME.into(),
                version: version()?,
                initial_description: "Run a shell command in the session workspace. Full output is retained in a live file.".into(),
                detailed_description: "Run command through /bin/sh in the session workspace with the agent process environment plus configured overrides. The workspace is not a sandbox. stdout and stderr are combined and streamed to .zork/live-<invocation-id>.log. The same file remains available after completion; the result contains its path and a bounded tail. timeout_seconds is optional.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "minLength": 1},
                        "timeout_seconds": {"type": "number", "exclusiveMinimum": 0}
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }),
            },
        ),
    ])
}

struct BuiltinTool {
    kind: BuiltinKind,
    dependencies: BuiltinToolDependencies,
}

impl ToolImplementation for BuiltinTool {
    fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: &'a Value,
    ) -> Pin<Box<dyn Future<Output = ToolExecution> + Send + 'a>> {
        let kind = self.kind;
        let dependencies = self.dependencies.clone();
        let context = context.clone();
        let arguments = arguments.clone();
        Box::pin(async move {
            match kind {
                BuiltinKind::End => success(serde_json::json!({
                    "acknowledge_outstanding": arguments
                        .get("acknowledge_outstanding")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })),
                BuiltinKind::Wait => wait_result(dependencies.clock.as_ref(), &arguments),
                BuiltinKind::ToolCancel => success(serde_json::json!({
                    "invocation_id": arguments.get("invocation_id").cloned().unwrap_or(Value::Null)
                })),
                BuiltinKind::HistoryList => {
                    history_list(dependencies.query.as_ref(), &context.session_id, &arguments)
                }
                BuiltinKind::FileRead => file_read(&dependencies, &context, &arguments).await,
                BuiltinKind::FileWrite => {
                    blocking(move || file_write(&dependencies, &context, &arguments)).await
                }
                BuiltinKind::FileEdit => {
                    blocking(move || file_edit(&dependencies, &context, &arguments)).await
                }
                BuiltinKind::ShellRun => shell_run(&dependencies, &context, &arguments).await,
            }
        })
    }
}

fn success(data: Value) -> ToolExecution {
    ToolExecution {
        outcome: super::events::ToolOutcome::Succeeded,
        data,
        result_schema_version: 1,
        knowledge: None,
    }
}

fn failed(message: impl Into<String>) -> ToolExecution {
    ToolExecution {
        outcome: super::events::ToolOutcome::Failed,
        data: error_data(message),
        result_schema_version: 1,
        knowledge: None,
    }
}

fn error_data(message: impl Into<String>) -> Value {
    serde_json::json!({"error": message.into()})
}

async fn blocking(work: impl FnOnce() -> ToolExecution + Send + 'static) -> ToolExecution {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) => failed(format!("Blocking tool task failed: {error}")),
    }
}

fn wait_result(clock: &dyn Clock, arguments: &Value) -> ToolExecution {
    let seconds = arguments
        .get("seconds")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let now_ms = clock.now_ms();
    let duration_ms = (seconds * 1000.0).ceil().min(i64::MAX as f64) as i64;
    success(serde_json::json!({
        "until_ms": now_ms.saturating_add(duration_ms),
        "reason": arguments.get("reason").cloned().unwrap_or(Value::Null),
    }))
}

fn history_list(
    query: &dyn super::query::SessionQuery,
    session_id: &str,
    input: &Value,
) -> ToolExecution {
    let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
    let before = input.get("before_event_id").and_then(Value::as_str);
    match query.before(session_id, before, limit) {
        Ok(events) => {
            let items = events
                .iter()
                .map(|event| {
                    serde_json::json!({
                        "event_id": event.event_id,
                        "kind": event.event.kind(),
                        "uri": format!("zork://history/{}", event.event_id),
                    })
                })
                .collect::<Vec<_>>();
            let data = serde_json::json!({
                "events": items,
                "next_before_event_id": events.first().map(|event| event.event_id.clone()),
            });
            success(data)
        }
        Err(error) => failed(format!("History query failed: {error}")),
    }
}

async fn file_read(
    dependencies: &BuiltinToolDependencies,
    context: &ToolContext,
    input: &Value,
) -> ToolExecution {
    let path = input
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(64 * 1024) as usize;

    let page = if let Some(event_id) = path.strip_prefix("zork://history/") {
        match dependencies.query.event(&context.session_id, event_id) {
            Ok(Some(event)) => match serde_json::to_vec_pretty(&event) {
                Ok(bytes) => page_bytes(bytes, offset, limit),
                Err(error) => {
                    return failed(format!("History event serialization failed: {error}"))
                }
            },
            Ok(None) => return failed(format!("History event {event_id} was not found.")),
            Err(error) => return failed(format!("History event read failed: {error}")),
        }
    } else {
        let path = resolve_path(&context.workspace, path);
        match dependencies.files.read_page(&path, offset, limit) {
            Ok(page) => page,
            Err(error) => return failed(format!("Failed to read {}: {error}", path.display())),
        }
    };

    let content = String::from_utf8_lossy(&page.bytes).into_owned();
    ToolExecution {
        outcome: super::events::ToolOutcome::Succeeded,
        data: serde_json::json!({
            "content": content,
            "offset": page.offset,
            "total_size": page.total_size,
            "next_offset": page.next_offset,
        }),
        result_schema_version: 1,
        knowledge: None,
    }
}

fn page_bytes(bytes: Vec<u8>, offset: u64, limit: usize) -> FilePage {
    let total_size = bytes.len() as u64;
    let start = offset.min(total_size) as usize;
    let end = start.saturating_add(limit).min(bytes.len());
    FilePage {
        bytes: bytes[start..end].to_vec(),
        offset,
        total_size,
        next_offset: (end < bytes.len()).then_some(end as u64),
    }
}

fn resolve_path(workspace: &str, path: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        std::path::Path::new(workspace).join(path)
    }
}

fn file_write(
    dependencies: &BuiltinToolDependencies,
    context: &ToolContext,
    input: &Value,
) -> ToolExecution {
    let path = resolve_path(
        &context.workspace,
        input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let content = input
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            dependencies.files.create_dir_all(parent)?;
        }
        dependencies.files.write(&path, content.as_bytes())
    })();
    match result {
        Ok(()) => success(serde_json::json!({"path": path, "bytes": content.len()})),
        Err(error) => failed(format!("Failed to write {}: {error}", path.display())),
    }
}

fn file_edit(
    dependencies: &BuiltinToolDependencies,
    context: &ToolContext,
    input: &Value,
) -> ToolExecution {
    let path = resolve_path(
        &context.workspace,
        input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let result = (|| -> Result<usize, String> {
        let original = dependencies
            .files
            .read_to_string(&path)
            .map_err(|error| error.to_string())?;
        let edits = input
            .get("edits")
            .and_then(Value::as_array)
            .ok_or_else(|| "edits must be an array".to_owned())?;
        let mut replacements = Vec::with_capacity(edits.len());
        for (index, edit) in edits.iter().enumerate() {
            let old = edit
                .get("old_text")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("edit {} has no old_text", index + 1))?;
            let new = edit
                .get("new_text")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("edit {} has no new_text", index + 1))?;
            let matches = original.match_indices(old).collect::<Vec<_>>();
            if matches.len() != 1 {
                return Err(format!(
                    "edit {} old_text matched {} locations",
                    index + 1,
                    matches.len()
                ));
            }
            let start = matches[0].0;
            replacements.push((start, start + old.len(), new.to_owned()));
        }
        replacements.sort_by_key(|replacement| replacement.0);
        if replacements.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err("edits overlap".into());
        }
        let mut updated = original;
        for (start, end, replacement) in replacements.iter().rev() {
            updated.replace_range(*start..*end, replacement);
        }
        dependencies
            .files
            .write(&path, updated.as_bytes())
            .map_err(|error| error.to_string())?;
        Ok(replacements.len())
    })();
    match result {
        Ok(count) => success(serde_json::json!({"path": path, "edits": count})),
        Err(error) => failed(format!("Failed to edit {}: {error}", path.display())),
    }
}

async fn shell_run(
    dependencies: &BuiltinToolDependencies,
    context: &ToolContext,
    input: &Value,
) -> ToolExecution {
    let command = input
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let timeout = input
        .get("timeout_seconds")
        .and_then(Value::as_f64)
        .map(std::time::Duration::from_secs_f64);
    let workspace = std::path::Path::new(&context.workspace);
    let relative = format!(".zork/live-{}.log", context.invocation_id);
    let live_path = workspace.join(&relative);
    if let Err(error) = dependencies.files.create_dir_all(&workspace.join(".zork")) {
        return failed(format!("Failed to create live output directory: {error}"));
    }
    let output = match dependencies.files.create(&live_path) {
        Ok(file) => file,
        Err(error) => return failed(format!("Failed to create live output file: {error}")),
    };
    let SpawnedProcess {
        output: mut process_output,
        handle,
    } = match dependencies.processes.spawn(ProcessRequest {
        command: command.to_owned(),
        current_dir: workspace.to_owned(),
        environment: dependencies.environment.clone(),
    }) {
        Ok(process) => process,
        Err(error) => return failed(format!("Failed to start command: {error}")),
    };
    let mut collectors = tokio::task::JoinSet::new();
    collectors.spawn(async move {
        use std::io::Write;
        let mut output = output;
        while let Some(chunk) = process_output.recv().await {
            let chunk = chunk?;
            output.write_all(&chunk)?;
        }
        output.flush()
    });

    let mut outcome = super::events::ToolOutcome::Succeeded;
    enum WaitOutcome {
        Finished(std::io::Result<super::ports::ProcessStatus>),
        TimedOut,
    }
    let waited = if let Some(timeout) = timeout {
        tokio::select! {
            status = handle.wait() => WaitOutcome::Finished(status),
            _ = dependencies.clock.sleep(timeout) => WaitOutcome::TimedOut,
        }
    } else {
        WaitOutcome::Finished(handle.wait().await)
    };
    let status = match waited {
        WaitOutcome::Finished(status) => status,
        WaitOutcome::TimedOut => {
            outcome = super::events::ToolOutcome::TimedOut;
            let _ = handle.kill().await;
            handle.wait().await
        }
    };
    match collectors.join_next().await {
        Some(Ok(Ok(()))) => {}
        Some(Ok(Err(error))) => {
            return failed(format!("Failed to persist command output: {error}"));
        }
        Some(Err(error)) => return failed(format!("Output collector task failed: {error}")),
        None => return failed("Output collector task ended without a result."),
    }
    match status {
        Ok(status) if status.success && outcome == super::events::ToolOutcome::Succeeded => {}
        Ok(status) if outcome != super::events::ToolOutcome::TimedOut => {
            outcome = super::events::ToolOutcome::Failed;
            let _ = status;
        }
        Ok(_) => {}
        Err(error) => return failed(format!("Failed while waiting for command: {error}")),
    }
    let tail = dependencies
        .files
        .tail(&live_path, 64 * 1024)
        .unwrap_or_default();
    let mut message = String::from_utf8_lossy(&tail).into_owned();
    if message.is_empty() {
        message.push_str("(no output)");
    }
    if outcome == super::events::ToolOutcome::TimedOut {
        message.push_str("\n\nCommand timed out; external effects may still be in flight.");
    } else if outcome == super::events::ToolOutcome::Failed {
        message.push_str("\n\nCommand exited unsuccessfully.");
    }
    ToolExecution {
        outcome,
        data: serde_json::json!({
            "output": message,
            "output_path": relative,
        }),
        result_schema_version: 1,
        knowledge: None,
    }
}
