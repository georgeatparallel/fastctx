//! Search for World conversations that have no search setup of their own.
//!
//! Codex reads the login, project, selected profile and managed settings. Its
//! ordinary HTTP MCP client owns discovery, calls, cancellation and results.

use std::path::Path;

use serde_json::{Value, json};

use super::{CodexHost, server::AppServer};

const SERVER: &str = "parallel-search";
const URL: &str = "https://search.parallel.ai/mcp";

impl CodexHost {
    /// Fill only an unconfigured search setup, never writing to the login or
    /// project. If inspection fails, keep Codex's settings rather than guess.
    pub(super) async fn search_config(&self, server: &AppServer, cwd: &Path, config: &Value) -> Value {
        if has_choices(config) {
            return config.clone();
        }
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let settings = server.request("config/read", json!({ "cwd": cwd, "includeLayers": true })).await?;
            let requirements = server.request("configRequirements/read", json!({})).await?;
            Ok::<_, String>((settings, requirements))
        })
        .await;
        match read {
            Ok(Ok((settings, requirements))) => defaults(config, &settings, &requirements),
            Ok(Err(error)) => {
                (self.log)(format!("keeping Codex's search configuration: {error}"));
                config.clone()
            }
            Err(_) => {
                (self.log)("keeping Codex's search configuration: inspection timed out".into());
                config.clone()
            }
        }
    }
}

/// Other MCP servers or plugins might supply search under any name. Disabled
/// entries, profiles and permission setups are also choices to leave alone.
fn has_choices(config: &Value) -> bool {
    let Some(config) = config.as_object() else { return true };
    for key in [
        "web_search",
        "tools",
        "profile",
        "profiles",
        "permissions",
        "default_permissions",
        "sandbox_mode",
        "sandbox_workspace_write",
        "network",
        "plugins",
        "apps",
        "mcp_optional_startup_grace_ms",
    ] {
        if config
            .get(key)
            .is_some_and(|value| !value.is_null() && value.as_object().is_none_or(|table| !table.is_empty()))
        {
            return true;
        }
    }
    if let Some(features) = config.get("features").filter(|value| !value.is_null()) {
        let Some(features) = features.as_object() else { return true };
        if features.keys().any(|key| key.contains("web_search")) {
            return true;
        }
    }
    if let Some(servers) = config.get("mcp_servers").filter(|value| !value.is_null()) {
        let Some(servers) = servers.as_object() else { return true };
        if servers.keys().any(|name| name != super::SERVER_NAME) {
            return true;
        }
    }
    false
}

fn defaults(config: &Value, settings: &Value, requirements: &Value) -> Value {
    let Some(effective) = settings.get("config").filter(|value| value.is_object()) else {
        return config.clone();
    };
    let Some(layers) = settings.get("layers").and_then(Value::as_array) else {
        return config.clone();
    };
    // A managed restriction is never a reason to route around native search.
    // Unknown protocol responses likewise do not authorize a new default.
    if requirements.get("requirements") != Some(&Value::Null)
        || has_choices(config)
        || has_choices(effective)
        || layers.iter().any(|layer| has_choices(&layer["config"]))
    {
        return config.clone();
    }
    let mut config = config.clone();
    config["web_search"] = json!("disabled");
    // The first turn must see search too, rather than race optional MCP startup.
    config["mcp_optional_startup_grace_ms"] = json!(10_000);
    config["mcp_servers"][SERVER] = json!({
        "url": URL,
        "http_headers": { "User-Agent": concat!("fastctx/", env!("CARGO_PKG_VERSION")) },
        "omit_tools_from": ["deferred"],
        "startup_timeout_sec": 10,
        "tool_timeout_sec": 30,
    });
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(config: Value) -> Value {
        json!({ "config": config, "layers": [{ "config": config }] })
    }

    #[test]
    fn fresh_search_uses_anonymous_http_mcp_and_keeps_world_tools() {
        let world = super::super::world_tools(Path::new("/world/fastctx"), &[], &["run"], 50_000);
        let result = defaults(
            &world,
            &settings(json!({ "model": "chosen-model", "web_search": null,
            "profiles": {}, "plugins": {}, "mcp_servers": {}, "apps": null, "permissions": null })),
            &json!({ "requirements": null }),
        );
        assert_eq!(result["web_search"], "disabled");
        assert_eq!(result["mcp_servers"][SERVER]["url"], URL);
        assert_eq!(
            result["mcp_servers"][SERVER]["http_headers"]["User-Agent"],
            concat!("fastctx/", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(result["mcp_servers"][SERVER]["tool_timeout_sec"], 30);
        assert!(result["mcp_servers"][SERVER].get("bearer_token_env_var").is_none());
        assert_eq!(result["mcp_servers"][super::super::SERVER_NAME], world["mcp_servers"][super::super::SERVER_NAME]);
        assert_eq!(result["features"], world["features"]);
    }

    #[test]
    fn saved_search_disables_credentials_profiles_and_permissions_win() {
        for saved in [
            json!({ "web_search": "cached" }),
            json!({ "web_search": "live" }),
            json!({ "web_search": "disabled" }),
            json!({ "mcp_servers": { "parallel-search": { "enabled": false } } }),
            json!({ "mcp_servers": { "parallel-search": { "url": URL,
                "bearer_token_env_var": "MY_PARALLEL_KEY" } } }),
            json!({ "mcp_servers": { "custom": { "command": "my-search" } } }),
            json!({ "features": { "web_search_request": false } }),
            json!({ "features": { "standalone_web_search": true } }),
            json!({ "tools": { "web_search": { "enabled": false } } }),
            json!({ "profile": "work" }),
            json!({ "mcp_optional_startup_grace_ms": 1500 }),
            json!({ "profiles": { "work": { "web_search": "disabled" } } }),
            json!({ "sandbox_mode": "read-only" }),
            json!({ "sandbox_workspace_write": { "network_access": false } }),
            json!({ "permissions": { "network": { "enabled": false } } }),
            json!({ "default_permissions": "read-only" }),
            json!({ "plugins": { "search@mine": { "enabled": true } } }),
            json!({ "apps": { "search": { "enabled": false } } }),
        ] {
            let world = json!({ "mcp_servers": { "fastctx": { "command": "fastctx" } } });
            assert_eq!(
                defaults(&world, &settings(saved.clone()), &json!({ "requirements": null })),
                world,
                "saved choice {saved}"
            );
            assert_eq!(
                defaults(&saved, &settings(json!({})), &json!({ "requirements": null })),
                saved,
                "thread override {saved}"
            );
            let layered = json!({ "config": {}, "layers": [{ "config": saved }] });
            assert_eq!(
                defaults(&world, &layered, &json!({ "requirements": null })),
                world,
                "project/profile/managed layer {layered}"
            );
        }
    }

    #[test]
    fn managed_or_unknown_configuration_does_not_enable_a_new_server() {
        let world = json!({});
        for requirements in [json!({}), json!({ "requirements": { "allowed_web_search_modes": ["disabled"] } })] {
            assert_eq!(defaults(&world, &settings(json!({})), &requirements), world);
        }
        for read in [
            json!({}),
            json!({ "config": {} }),
            json!({ "config": {}, "layers": [{}] }),
            settings(json!({ "mcp_servers": "unknown format" })),
        ] {
            assert_eq!(defaults(&world, &read, &json!({ "requirements": null })), world);
        }
    }
}
