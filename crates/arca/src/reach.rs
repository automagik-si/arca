//! Connector reads. Status fields follow Tailscale 1.104.7's source; the pinned image has not been exercised live.
//! Parse only the fields a read needs. AuthURL never enters a status row, error, journal or log.
use crate::{
    config::NodeConfig,
    kernel::Kernel,
    service,
    service_exec::{ExecTarget, ServiceExec},
};
use arca_incus::{IncusRead, UnixIncus, exec::ExecCommand};
use arca_mcp::{CallError, Caller};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn unavailable() -> CallError {
    CallError::Internal("connector state unavailable".into())
}

pub(crate) fn kind(cfg: &NodeConfig, record: &service::ServiceConfig) -> Option<String> {
    let template = service::load_template(&cfg.state_dir.join("templates"), &record.template).ok();
    service::connector_kind(record, template.as_ref()).map(str::to_string)
}

fn records(
    kernel: &Arc<Mutex<Kernel>>,
) -> Result<Vec<(arca_kernel::desired::Entry, service::ServiceConfig)>, CallError> {
    Ok(crate::handlers::service_entries(
        kernel.lock().map_err(|_| unavailable())?.entries(),
    ))
}

fn target(
    kernel: &Arc<Mutex<Kernel>>,
    entry: &arca_kernel::desired::Entry,
    record: &service::ServiceConfig,
) -> Result<ExecTarget, CallError> {
    let kernel = kernel.lock().map_err(|_| unavailable())?;
    Ok(ExecTarget {
        entry: entry.id.clone(),
        service: record.clone(),
        uuid: kernel.service_uuid(&entry.id, &record.instance),
        retained: kernel.retained_entries().map_err(|_| unavailable())?,
    })
}

fn read(
    exec: &ServiceExec,
    target: &ExecTarget,
    caller: &Caller,
) -> Result<Option<Vec<u8>>, CallError> {
    match exec.run(
        target,
        &caller.token_id,
        "reach_list",
        &ExecCommand::ConnectorStatus,
        &[],
        None,
    ) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.ownership() => Err(error.call_error()),
        Err(_) => Ok(None),
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Status {
    backend_state: Option<String>,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
    #[serde(rename = "Self")]
    own: Option<Peer>,
}
#[derive(Deserialize)]
struct Peer {
    #[serde(rename = "DNSName")]
    dns: Option<String>,
    #[serde(rename = "HostName")]
    host: Option<String>,
}

fn tailnet_name(text: &str) -> Option<&str> {
    let text = text.trim_end_matches('.');
    (!text.is_empty()
        && text.len() <= 253
        && text
            .split('.')
            .all(|label| arca_incus::exec::node_ok(label)))
    .then_some(text)
}

/// One safe row per connector, including records written before connector kinds were stored on services.
pub(crate) fn reach_rows(
    kernel: &Arc<Mutex<Kernel>>,
    cfg: &NodeConfig,
    exec: &ServiceExec,
    caller: &Caller,
) -> Result<Vec<Value>, CallError> {
    let records = records(kernel)?;
    let incus = UnixIncus::new(cfg.incus.socket.clone());
    let mut rows = Vec::new();
    for (entry, record) in &records {
        if record.tenant.is_some() {
            continue;
        }
        let Some(kind) = kind(cfg, record) else {
            continue;
        };
        let edge = record
            .links
            .iter()
            .find(|link| {
                records
                    .iter()
                    .any(|(_, provider)| provider.name == link.to && provider.edge.is_some())
            })
            .map(|link| link.to.as_str());
        let live = incus
            .get(&format!(
                "/1.0/instances/{}?project={}",
                record.instance, record.project
            ))
            .ok();
        let running = live.as_ref().is_some_and(|row| row["status"] == "Running");
        let mut row = json!({"name":record.name, "kind":kind, "edge":edge, "state":if running {"running"} else {"stopped"}});
        if kind == "tailscale" {
            if running {
                let bytes = read(exec, &target(kernel, entry, record)?, caller)?;
                // Missing, new or malformed fields leave the connector waiting, never authenticated by inference.
                let status = bytes
                    .as_deref()
                    .and_then(|bytes| serde_json::from_slice::<Status>(bytes).ok())
                    .unwrap_or_default();
                row["state"] = json!(if status.backend_state.as_deref() == Some("Running") {
                    "running"
                } else {
                    "waiting-sign-in"
                });
                if row["state"] == "running" {
                    if let Some(address) = status
                        .tailscale_ips
                        .into_iter()
                        .flatten()
                        .find_map(|ip| ip.parse::<std::net::IpAddr>().ok())
                    {
                        row["tailnetAddress"] = json!(address.to_string());
                    }
                    if let Some(peer) = status.own
                        && let Some(name) = peer
                            .dns
                            .as_deref()
                            .and_then(tailnet_name)
                            .or_else(|| peer.host.as_deref().and_then(tailnet_name))
                    {
                        row["tailnetName"] = json!(name);
                    }
                }
            } else {
                // Containerboot's authentication timeout, not a daemon/socket timeout or an ordinary operator stop.
                let log = incus
                    .get_text(&format!(
                        "/1.0/instances/{}/console?project={}&type=log",
                        record.instance, record.project
                    ))
                    .unwrap_or_default();
                let mut timed_out = false;
                for line in log.lines().filter(|line| line.starts_with("boot: ")) {
                    if line.ends_with("Starting tailscaled")
                        || line.ends_with("Startup complete, waiting for shutdown signal")
                    {
                        timed_out = false;
                    } else if line.ends_with(
                        "failed to auth tailscale: tailscale up failed: context deadline exceeded",
                    ) {
                        timed_out = true;
                    }
                }
                if timed_out {
                    row["state"] = json!("gave-up");
                }
            }
        }
        rows.push(row);
    }
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(rows)
}

/// Only the next passkey-checked page route will call this; no tool handler returns its result.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the next reach change adds the owner's sign-in page"
    )
)]
pub(crate) fn reach_sign_in(
    kernel: &Arc<Mutex<Kernel>>,
    cfg: &NodeConfig,
    exec: &ServiceExec,
    caller: &Caller,
    name: &str,
) -> Result<Option<String>, CallError> {
    let (entry, record) = records(kernel)?
        .into_iter()
        .find(|(_, record)| {
            record.name == name
                && record.tenant.is_none()
                && kind(cfg, record).as_deref() == Some("tailscale")
        })
        .ok_or_else(|| CallError::Refused {
            code: "NotFound".into(),
            message: "Tailscale connector not found".into(),
        })?;
    #[derive(Deserialize)]
    struct SignIn {
        #[serde(rename = "AuthURL")]
        url: Option<String>,
    }
    Ok(read(exec, &target(kernel, &entry, &record)?, caller)?
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<SignIn>(bytes).ok())
        .and_then(|status| status.url)
        .filter(|url| login_link(url)))
}

fn login_link(url: &str) -> bool {
    url.strip_prefix("https://login.tailscale.com/a/")
        .is_some_and(|id| {
            !id.is_empty()
                && id.len() <= 512
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        })
}

/// Hide exactly the vendor's node-login URL shape; preserve punctuation, admin URLs and other text.
pub(crate) fn redact_login_links(text: &str) -> String {
    static LINKS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"https://login\.tailscale\.com/a/[A-Za-z0-9_-]+\b")
            .expect("fixed login-link pattern")
    });
    LINKS.replace_all(text, "[sign-in link]").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_drops_auth_url_and_tolerates_missing_fields() {
        let status: Status = serde_json::from_value(json!({"AuthURL":"https://login.tailscale.com/a/synthetic", "BackendState":"NeedsLogin", "NewField":true})).unwrap();
        assert_eq!(status.backend_state.as_deref(), Some("NeedsLogin"));
        assert!(status.tailscale_ips.is_none());
        assert!(tailnet_name("https://login.tailscale.com/a/synthetic").is_none());
        assert!(login_link("https://login.tailscale.com/a/synthetic"));
        assert!(!login_link("https://login.tailscale.com/admin/machines"));
    }
    #[test]
    fn sign_in_reads_only_the_link_and_keeps_it_out_of_the_exec_audit() {
        use arca_approval::{
            ApprovalVerifier, FileNonces, OwnerKeys, PasskeyVerifier, SshSigVerifier,
        };
        use arca_incus::{FakeIncus, FakeState, fake::ExecScript};
        use arca_kernel::identity::NodeKey;
        use arca_providers::incus::{Family, IncusProvider};
        use arca_reconcile::PluginSet;
        use std::fs;
        let dir = std::env::temp_dir().join(format!("rg4-sign-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let fake = FakeIncus::start(
            &dir,
            FakeState {
                server: json!({}),
                projects: vec![],
                instances: vec![],
                networks: vec![],
                pools: vec![],
            },
        )
        .unwrap();
        fake.add_profile("default", json!({"name":"default","config":{},"devices":{"root":{"type":"disk","pool":"pool1","path":"/"}}}));
        let cfg: Arc<NodeConfig> = Arc::new(serde_json::from_value(json!({"stateDir":dir,"incus":{"socket":fake.socket()},"listen":{"unix":dir.join("arca.sock")},"tokens":{"hashDir":dir.join("tokens")},"kernel":{"projects":[arca_netproject::PROJECT],"serviceCidr":"192.0.2.0/24"}})).unwrap());
        let mut plugins = PluginSet::new();
        plugins.add::<arca_netproject::NetProject>().unwrap();
        plugins.add::<service::Service>().unwrap();
        let verifier = ApprovalVerifier::new(
            "node1".into(),
            OwnerKeys::new(
                SshSigVerifier::new(vec![]),
                PasskeyVerifier::refusing(arca_approval::ApprovalRefusal::NoVerifier),
            ),
            FileNonces::open(&dir.join("nonces")).unwrap(),
        );
        let kernel = Arc::new(Mutex::new(
            Kernel::open(
                &dir,
                NodeKey::load_or_create(&dir.join("node.key")).unwrap(),
                plugins,
                verifier,
                "node1".into(),
                None,
                |retained| {
                    Ok(Box::new(
                        IncusProvider::new(
                            fake.socket().into(),
                            Family {
                                projects: vec![arca_netproject::PROJECT.into()],
                                scratch: false,
                                droidd_projects: vec![],
                                reserved_cidrs: vec![],
                            },
                            dir.join("secrets"),
                            retained,
                        )
                        .unwrap(),
                    ))
                },
                1000,
            )
            .unwrap(),
        ));
        fs::create_dir(dir.join("templates")).unwrap();
        fs::write(dir.join("templates/tailscale.json"), json!({"name":"tailscale","kind":"oci","image":"docker.io/library/example","digest":format!("sha256:{}", "a".repeat(64)),"connector":"tailscale","env":{}}).to_string()).unwrap();
        let caller = Caller {
            token_id: "doctor".into(),
            scope: arca_api::Scope::Doctor,
            tools: None,
        };
        let handlers =
            crate::handlers::service_handlers(kernel.clone(), Arc::new(|| 1000), cfg.clone());
        handlers
            .iter()
            .find(|(verb, _)| *verb == "service_create")
            .unwrap()
            .1
            .call(json!({"name":"node-ts","template":"tailscale"}), &caller)
            .unwrap();
        let exec = ServiceExec::new(cfg.clone());
        let link = "https://login.tailscale.com/a/synthetic123";
        fake.script_exec(ExecScript::new(
            ExecCommand::ConnectorStatus.argv(),
            0,
            &json!({"AuthURL":link,"Unrelated":"ignored"}).to_string(),
        ));
        assert_eq!(
            reach_sign_in(&kernel, &cfg, &exec, &caller, "node-ts")
                .unwrap()
                .as_deref(),
            Some(link)
        );
        fake.script_exec(ExecScript::new(
            ExecCommand::ConnectorStatus.argv(),
            0,
            r#"{"AuthURL":"https://example.com/wrong"}"#,
        ));
        assert!(
            reach_sign_in(&kernel, &cfg, &exec, &caller, "node-ts")
                .unwrap()
                .is_none()
        );
        assert!(reach_sign_in(&kernel, &cfg, &exec, &caller, "missing").is_err());
        assert!(
            !fs::read_to_string(dir.join(crate::service_exec::AUDIT_FILE))
                .unwrap()
                .contains(link)
        );
        drop(handlers);
        drop(kernel);
        drop(fake);
        fs::remove_dir_all(dir).unwrap();
    }
}
