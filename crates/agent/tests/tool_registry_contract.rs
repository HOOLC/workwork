use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{json, Value};
use zork_agent::session::events::ToolOutcome;
use zork_agent::session::tools::{
    tool_help_instance, NoToolState, ToolChange, ToolCompatibility, ToolContext, ToolContract,
    ToolExecution, ToolImplementation, ToolInstance, ToolKnowledge, ToolRegistry, ToolResolution,
    ToolState, ToolVersion,
};

struct Echo(&'static str);

impl ToolImplementation for Echo {
    fn execute<'a>(
        &'a self,
        _context: &'a ToolContext,
        _arguments: &'a Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolExecution> + Send + 'a>> {
        Box::pin(async move { ToolExecution::success(self.0) })
    }
}

#[tokio::test]
// Contract: docs/zork-agent-architecture.md [TOOL-06]
async fn tool_help_always_returns_the_targets_current_detailed_contract() {
    let registry = Arc::new(ToolRegistry::default());
    registry.register(instance("file.read", "v1", "old"));
    registry.register(tool_help_instance(&registry, ToolVersion::new("help-v1").unwrap()).unwrap());
    registry.register(instance("file.read", "v2", "new"));

    let help = match registry.resolve("tool.help", Some(&ToolVersion::new("help-v1").unwrap())) {
        ToolResolution::Ready(instance) => instance,
        other => panic!("expected tool.help, got {other:?}"),
    };
    let result = help
        .execute(
            &ToolContext {
                session_id: "session".into(),
                invocation_id: "invocation".into(),
                workspace: "/workspace".into(),
            },
            &json!({"tool": "file.read"}),
        )
        .await;
    assert_eq!(result.data["description"], "detailed file.read");
    assert_eq!(result.data["version"], "v2");
    assert!(matches!(
        result.knowledge,
        Some(ToolKnowledge::Current { ref name, ref version })
            if name == "file.read" && version.as_str() == "v2"
    ));
}

fn instance(name: &str, version: &str, output: &'static str) -> Arc<ToolInstance> {
    Arc::new(
        ToolInstance::new(
            ToolContract {
                name: name.into(),
                version: ToolVersion::new(version).unwrap(),
                initial_description: format!("initial {name}"),
                detailed_description: format!("detailed {name}"),
                input_schema: json!({"type": "object"}),
            },
            Arc::new(Echo(output)),
            Arc::new(NoToolState),
        )
        .unwrap(),
    )
}

#[tokio::test]
// Contract: docs/zork-agent-architecture.md [TOOL-02]
async fn each_logical_tool_updates_and_resolves_its_own_opaque_version() {
    let registry = ToolRegistry::default();
    let first = instance("aa.one", "blue", "old");
    registry.register(first.clone());
    registry.register(instance("aa.two", "7", "two"));

    let catalog = registry.initial_catalog();
    assert_eq!(catalog.len(), 2);
    assert_eq!(catalog[0].name, "aa.one");
    assert_eq!(catalog[0].version.as_str(), "blue");
    assert_eq!(catalog[0].description, "initial aa.one");
    assert_eq!(catalog[1].name, "aa.two");

    let known = BTreeMap::from([
        ("aa.one".to_owned(), ToolVersion::new("blue").unwrap()),
        ("aa.two".to_owned(), ToolVersion::new("7").unwrap()),
    ]);
    let captured = match registry.resolve("aa.one", known.get("aa.one")) {
        ToolResolution::Ready(instance) => instance,
        other => panic!("expected ready tool, got {other:?}"),
    };

    registry.register(instance("aa.one", "rollback-x", "new"));
    assert_eq!(
        registry.changes(&known),
        vec![ToolChange::Updated {
            name: "aa.one".into(),
            version: ToolVersion::new("rollback-x").unwrap(),
        }]
    );
    assert!(matches!(
        registry.resolve("aa.one", known.get("aa.one")),
        ToolResolution::VersionChanged { ref current }
            if current.as_str() == "rollback-x"
    ));

    let result = captured
        .execute(
            &ToolContext {
                session_id: "session".into(),
                invocation_id: "invocation".into(),
                workspace: "/workspace".into(),
            },
            &json!({}),
        )
        .await;
    assert_eq!(result.outcome, ToolOutcome::Succeeded);
    assert_eq!(result.data, "old");
}

struct MarkerCompatibility(&'static str);

impl ToolCompatibility for MarkerCompatibility {
    fn migrate_result(&self, schema_version: u32, value: Value) -> Result<Value, String> {
        (schema_version == 1)
            .then_some(value)
            .ok_or_else(|| format!("unsupported result schema {schema_version}"))
    }

    fn migrate_state(&self, state: ToolState) -> Result<ToolState, String> {
        Ok(state)
    }

    fn initial_state(&self) -> Option<ToolState> {
        Some(ToolState {
            schema_version: 1,
            value: json!({"owner": self.0}),
        })
    }

    fn fold(
        &self,
        state: Option<&ToolState>,
        _result: &Value,
    ) -> Result<Option<ToolState>, String> {
        Ok(state.cloned())
    }

    fn outstanding(
        &self,
        _state: Option<&ToolState>,
    ) -> Vec<zork_agent::session::events::OutstandingItem> {
        Vec::new()
    }
}

fn stateful_instance(name: &str, version: &str, marker: &'static str) -> Arc<ToolInstance> {
    Arc::new(
        ToolInstance::new(
            ToolContract {
                name: name.into(),
                version: ToolVersion::new(version).unwrap(),
                initial_description: format!("initial {name}"),
                detailed_description: format!("detailed {name}"),
                input_schema: json!({"type": "object"}),
            },
            Arc::new(Echo(marker)),
            Arc::new(MarkerCompatibility(marker)),
        )
        .unwrap(),
    )
}

#[test]
// Contract: docs/zork-agent-architecture.md [TOOL-08]
fn removing_a_tool_keeps_compatibility_and_readding_it_is_reported_as_added() {
    let registry = ToolRegistry::default();
    registry.register(stateful_instance("aa.removable", "old", "old-state"));
    let known = BTreeMap::from([("aa.removable".to_owned(), ToolVersion::new("old").unwrap())]);

    assert!(registry.remove("aa.removable"));
    assert!(matches!(
        registry.resolve("aa.removable", known.get("aa.removable")),
        ToolResolution::Unavailable
    ));
    assert_eq!(
        registry
            .compatibility("aa.removable")
            .unwrap()
            .initial_state()
            .unwrap()
            .value["owner"],
        "old-state"
    );
    assert_eq!(
        registry.changes(&known),
        vec![ToolChange::Removed {
            name: "aa.removable".into()
        }]
    );

    registry.register(stateful_instance("aa.removable", "new", "new-state"));
    assert_eq!(
        registry.changes(&BTreeMap::new()),
        vec![ToolChange::Added {
            name: "aa.removable".into(),
            version: ToolVersion::new("new").unwrap(),
        }]
    );
}
