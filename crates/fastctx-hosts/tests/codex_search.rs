//! Exercise the production World host and the official Codex HTTP MCP caller.
//! Run with FASTCTX_TEST_CODEX=/path/to/codex cargo test -p fastctx-hosts
//! --test codex_search -- --ignored --nocapture (Codex 0.160, Python 3, OpenSSL).
//! FASTCTX_TEST_SEARCH_LIVE=1 additionally opts into an anonymous live attempt
//! instead of the deterministic MCP fixture; the model remains scripted/local.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use fastctx_hosts::codex::{CodexHost, HostEvent, Input, ThreadSpec, world_tools};
use serde_json::{Value, json};

struct Fixture(std::process::Child);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn live_result(transcript: &str) -> bool {
    let calls: Vec<Value> = serde_json::Deserializer::from_str(transcript)
        .into_iter::<Value>()
        .map(Result::unwrap)
        .filter_map(|event| {
            let detail = event.pointer("/Happened/event/item/detail")?;
            (detail["type"] == "mcpToolCall" && detail["status"] != "inProgress").then(|| detail.clone())
        })
        .collect();
    let call = calls.last().expect("live MCP call was executed");
    let failed = !call["error"].is_null() || call["result"]["isError"] == true;
    if failed {
        let error = call.to_string().to_ascii_lowercase();
        assert!(
            error.contains("quota") || error.contains("rate limit") || error.contains("rate_limit"),
            "unexpected live failure: {call}"
        );
        println!("Live anonymous quota observed; this is not a successful live call: {call}");
        return false;
    }
    let content = call["result"]["content"].as_array().expect("MCP content");
    let result: Value = serde_json::from_str(content.iter().find_map(|c| c["text"].as_str()).unwrap()).unwrap();
    let sources = result["results"].as_array().expect("live result sources");
    assert!(!sources.is_empty());
    assert!(
        sources
            .iter()
            .all(|s| s["url"].as_str().is_some_and(|url| url.starts_with("https://") || url.starts_with("http://")))
    );
    assert!(transcript.contains("Consumed:"), "model consumed the live output");
    true
}

async fn turn(
    host: &Arc<CodexHost>,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<HostEvent>,
    thread: &str,
    prompt: &str,
) -> String {
    host.send(thread, Input::Text(prompt.into()), None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(50), async {
        let mut seen = String::new();
        loop {
            let event = events.recv().await.expect("host event stream");
            seen.push_str(&serde_json::to_string(&event).unwrap());
            let done = matches!(&event, HostEvent::Happened { thread: target, event }
                if target == thread && matches!(event.happened, fastctx_types::session::Happened::TurnEnded { .. }));
            host.acknowledge(&event);
            if done {
                return seen;
            }
        }
    })
    .await
    .expect("turn deadline")
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires official Codex 0.160, Python 3 and OpenSSL; uses a local scripted model"]
async fn world_default_search_dispatches_and_consumes_results_with_official_codex() {
    let codex = std::fs::canonicalize(std::env::var_os("FASTCTX_TEST_CODEX").expect("FASTCTX_TEST_CODEX")).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let status = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=Fastctx fixture CA",
            "-keyout",
        ])
        .arg(root.join("ca-key.pem"))
        .arg("-out")
        .arg(root.join("ca.pem"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let status = Command::new("openssl")
        .args(["req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=search.parallel.ai", "-keyout"])
        .arg(root.join("key.pem"))
        .arg("-out")
        .arg(root.join("request.pem"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::write(root.join("extensions"), "subjectAltName=DNS:search.parallel.ai\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n").unwrap();
    let status = Command::new("openssl")
        .args(["x509", "-req", "-days", "1", "-in"])
        .arg(root.join("request.pem"))
        .arg("-CA")
        .arg(root.join("ca.pem"))
        .arg("-CAkey")
        .arg(root.join("ca-key.pem"))
        .arg("-CAcreateserial")
        .arg("-extfile")
        .arg(root.join("extensions"))
        .arg("-out")
        .arg(root.join("cert.pem"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let mut fixture = Fixture(
        Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/search_fixture.py"))
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    BufReader::new(fixture.0.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let setup: Value = serde_json::from_str(&line).unwrap();
    std::fs::create_dir(root.join("bin")).unwrap();
    std::os::unix::fs::symlink(codex, root.join("bin/codex")).unwrap();
    let login = root.join("login");
    std::fs::create_dir(&login).unwrap();
    let config = format!(
        "model = 'mock-model'\nmodel_provider = 'fixture'\n[model_providers.fixture]\nname = 'fixture'\nbase_url = {}\nwire_api = 'responses'\nrequires_openai_auth = false\nrequest_max_retries = 0\nstream_max_retries = 0\n",
        setup["model"]
    );
    std::fs::write(login.join("config.toml"), &config).unwrap();
    // A stand-in only for the unrelated World shell; search uses the real
    // production resolver and Codex HTTP MCP client at its production URL.
    let shell = root.join("world-tools.py");
    std::fs::write(&shell, "#!/usr/bin/env python3\nimport sys,json\nfor line in sys.stdin:\n r=json.loads(line)\n if 'id' not in r: continue\n m=r.get('method')\n v={'protocolVersion':r['params']['protocolVersion'],'capabilities':{'tools':{}},'serverInfo':{'name':'world-fixture','version':'1'}} if m=='initialize' else {'tools':[{'name':'run','description':'fixture shell','inputSchema':{'type':'object','properties':{}}}]} if m=='tools/list' else {}\n print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':v}),flush=True)\n").unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
    let live = std::env::var_os("FASTCTX_TEST_SEARCH_LIVE").is_some();
    let mut env = vec![
        ("PATH".into(), format!("{}:/usr/bin:/bin", root.join("bin").display()).into()),
        ("CODEX_HOME".into(), login.as_os_str().to_owned()),
        ("HTTPS_PROXY".into(), setup["proxy"].as_str().unwrap().into()),
        ("NO_PROXY".into(), "127.0.0.1,localhost".into()),
        ("CODEX_CA_CERTIFICATE".into(), root.join("ca.pem").into_os_string()),
        ("RUST_LOG".into(), "error".into()),
    ];
    if live {
        env.retain(|(name, _)| name != "HTTPS_PROXY" && name != "CODEX_CA_CERTIFICATE");
        // Keep the runner's network route and trust roots, without inheriting
        // any provider keys or a saved Codex login.
        for name in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy", "SSL_CERT_FILE", "CODEX_CA_CERTIFICATE"]
        {
            if let Some(value) = std::env::var_os(name) {
                env.push((name.into(), value));
            }
        }
    }
    let (host, mut events) =
        CodexHost::new(login.clone(), None, root.join("host"), env, Arc::new(|message| eprintln!("{message}")));
    let spec = |id: &str| ThreadSpec {
        owner: fastctx_types::Address { machine: "fixture".parse().unwrap(), resident: id.parse().unwrap() },
        cwd: root.to_owned(),
        developer_instructions: "Scripted integration fixture".into(),
        config: world_tools(&shell, &[], &["run"], 50_000),
        name: None,
        choices: root.join(format!("{id}.json")),
    };
    let (thread, _) = host.start_thread(spec("codex-abcd")).await.unwrap();
    assert_eq!(host.prepare(&thread).await.unwrap(), None);
    if live {
        let search =
            turn(&host, &mut events, &thread, "LIVE SEARCH Find official Parallel Search MCP documentation").await;
        println!("LIVE_SEARCH: {search}");
        if !live_result(&search) {
            return;
        }
        let fetch = turn(&host, &mut events, &thread, "LIVE FETCH Parallel Search MCP documentation").await;
        println!("LIVE_FETCH: {fetch}");
        live_result(&fetch);
        return;
    }
    let search = turn(&host, &mut events, &thread, "SEARCH fixture source").await;
    assert!(search.contains("fixture-source"), "{search}");
    assert!(search.contains("Consumed:"), "{search}");
    let fetch = turn(&host, &mut events, &thread, "FETCH fixture page").await;
    assert!(fetch.contains("https://example.org/source"), "{fetch}");
    host.sleep(&thread).await.unwrap();
    assert_eq!(host.prepare(&thread).await.unwrap(), None);
    let resumed = turn(&host, &mut events, &thread, "SEARCH resumed conversation").await;
    assert!(resumed.contains("fixture-source"), "{resumed}");
    let (fork, _) = host.fork(&thread, None, spec("codex-bcde")).await.unwrap();
    assert_eq!(host.prepare(&fork).await.unwrap(), None);
    let forked = turn(&host, &mut events, &fork, "SEARCH forked conversation").await;
    assert!(forked.contains("fixture-source"), "{forked}");
    let error = turn(&host, &mut events, &thread, "ERROR fixture search").await;
    assert!(error.contains("fixture-error"), "{error}");
    let timeout = turn(&host, &mut events, &thread, "TIMEOUT fixture search").await;
    assert!(timeout.contains("timed out") || timeout.contains("timeout"), "{timeout}");
    host.send(&thread, Input::Text("CANCEL fixture search".into()), None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let log = std::fs::read_to_string(root.join("requests.jsonl")).unwrap();
            if log.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).any(|r| {
                r["kind"] == "mcp"
                    && r.pointer("/body/params/arguments/objective")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.contains("CANCEL"))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(host.interrupt(&thread).await.unwrap());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            let done = matches!(&event, HostEvent::Happened { thread: target, event }
                if target == &thread && matches!(event.happened, fastctx_types::session::Happened::TurnEnded { .. }));
            host.acknowledge(&event);
            if done {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(login.join("config.toml")).unwrap(), config);
    let calls_before = std::fs::read_to_string(root.join("requests.jsonl"))
        .unwrap()
        .lines()
        .filter(|line| line.contains("\"kind\": \"mcp\""))
        .count();
    std::fs::write(login.join("config.toml"), format!("web_search = 'disabled'\n{config}")).unwrap();
    let (disabled, _) = host.start_thread(spec("codex-cdef")).await.unwrap();
    assert_eq!(host.prepare(&disabled).await.unwrap(), None);
    let disabled_result = turn(&host, &mut events, &disabled, "SEARCH must remain disabled").await;
    assert!(disabled_result.contains("NO_SEARCH_TOOL"), "{disabled_result}");
    std::fs::write(login.join("config.toml"), &config).unwrap();
    let mut overridden = spec("codex-def0");
    overridden.config["web_search"] = json!("disabled");
    let (overridden, _) = host.start_thread(overridden).await.unwrap();
    assert_eq!(host.prepare(&overridden).await.unwrap(), None);
    let override_result = turn(&host, &mut events, &overridden, "SEARCH with explicit disable override").await;
    assert!(override_result.contains("NO_SEARCH_TOOL"), "{override_result}");
    // Existing native choices, a failing explicitly configured provider, and
    // a project/login permission setup must never trigger a Parallel fallback.
    for (id, saved) in [
        ("codex-ef01", format!("web_search = 'live'\n{config}")),
        (
            "codex-f012",
            format!(
                "{config}\n[mcp_servers.parallel-search]\nurl = 'http://127.0.0.1:9/mcp'\nstartup_timeout_sec = 1\n"
            ),
        ),
        ("codex-0123", format!("sandbox_workspace_write = {{ network_access = false }}\n{config}")),
    ] {
        std::fs::write(login.join("config.toml"), &saved).unwrap();
        let (kept, _) = host.start_thread(spec(id)).await.unwrap();
        assert_eq!(host.prepare(&kept).await.unwrap(), None);
        let kept_result = turn(&host, &mut events, &kept, "SEARCH keep existing setup").await;
        assert!(kept_result.contains("NO_SEARCH_TOOL"), "{kept_result}");
        assert_eq!(std::fs::read_to_string(login.join("config.toml")).unwrap(), saved);
    }
    let calls_after = std::fs::read_to_string(root.join("requests.jsonl"))
        .unwrap()
        .lines()
        .filter(|line| line.contains("\"kind\": \"mcp\""))
        .count();
    assert_eq!(calls_before, calls_after, "disabled search must not connect to Parallel");
    let requests: Vec<Value> = std::fs::read_to_string(root.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mcp: Vec<_> = requests.iter().filter(|r| r["kind"] == "mcp").collect();
    for r in &mcp {
        assert_eq!(r["path"], "/mcp");
        assert_eq!(r["host"], "search.parallel.ai");
        assert_eq!(r["user_agent"], concat!("fastctx/", env!("CARGO_PKG_VERSION")));
        assert!(r["authorization"].is_null());
    }
    for method in ["initialize", "tools/list", "tools/call"] {
        assert!(mcp.iter().any(|r| r["body"]["method"] == method));
    }
    assert!(mcp.iter().any(|r| r["body"]["params"]["name"] == "web_fetch"));
    let first_model = requests.iter().find(|r| r["kind"] == "model").unwrap();
    assert!(
        !first_model["body"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["type"].as_str().is_some_and(|kind| kind.starts_with("web_search"))),
        "the default uses Parallel, without also offering native search"
    );
    println!(
        "{}",
        json!({"scripted_model":true,"mcp_requests":mcp,"search_consumed":true,"fetch_consumed":true,"error_consumed":true,"timeout_observed":true,"cancelled":true,"resume":true,"fork":true,"saved_disable":true,"explicit_override":true})
    );
    drop(host);
}
