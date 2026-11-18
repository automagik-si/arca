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

pub(crate) fn page_reads(
    kernel: Arc<Mutex<Kernel>>,
    cfg: Arc<NodeConfig>,
    exec: Arc<ServiceExec>,
) -> crate::approve_page::ReachReads {
    let caller = Caller {
        token_id: arca_mcp::auth::APPROVED_ACTION_ID.into(),
        scope: arca_api::Scope::Doctor,
        tools: None,
        limits: Default::default(),
    };
    let (rows_kernel, rows_cfg, rows_exec, rows_caller) =
        (kernel.clone(), cfg.clone(), exec.clone(), caller.clone());
    crate::approve_page::ReachReads {
        rows: Arc::new(move || reach_rows(&rows_kernel, &rows_cfg, &rows_exec, &rows_caller)),
        sign_in: Arc::new(move |name| reach_sign_in(&kernel, &cfg, &exec, &caller, name)),
    }
}

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
    verb: &str,
) -> Result<Option<Vec<u8>>, CallError> {
    match exec.run(
        target,
        &caller.token_id,
        verb,
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
    (!text.is_empty() && text.len() <= 253 && text.split('.').all(arca_incus::exec::node_ok))
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
                let bytes = read(exec, &target(kernel, entry, record)?, caller, "reach_list")?;
                // Unreadable status or a missing backend state cannot establish whether sign-in is needed.
                let status = bytes
                    .as_deref()
                    .and_then(|bytes| serde_json::from_slice::<Status>(bytes).ok())
                    .unwrap_or_default();
                row["state"] = json!(match status.backend_state.as_deref() {
                    Some("Running") => "running",
                    Some(state) if !state.is_empty() => "waiting-sign-in",
                    _ => "unknown",
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

/// Only the passkey-checked page route calls this; no tool handler returns its result.
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
        #[serde(rename = "BackendState")]
        state: Option<String>,
    }
    Ok(read(
        exec,
        &target(kernel, &entry, &record)?,
        caller,
        "reach_sign_in",
    )?
    .as_deref()
    .and_then(|bytes| serde_json::from_slice::<SignIn>(bytes).ok())
    .filter(|status: &SignIn| {
        status
            .state
            .as_deref()
            .is_some_and(|state| !state.is_empty() && state != "Running")
    })
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

fn login_links() -> &'static regex::Regex {
    static LINKS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        let host = "login.tailscale.com/a/"
            .chars()
            .map(|ch| regex::escape(&ch.to_string()))
            .collect::<Vec<_>>()
            .join(r"\r*");
        // Other breaks inside an identifier cannot be distinguished from the next log word; leave that tail.
        regex::Regex::new(&format!(
            r"(?i)(?:[a-z\r]*:?\r*/\r*/)?{host}[ \t\r\n]*(?P<id>[A-Za-z0-9_-]+(?:\r+[A-Za-z0-9_-]+)*)"
        )).expect("fixed login-link pattern")
    });
    &LINKS
}

/// Hide the vendor's login host and path, with or without a scheme; preserve admin URLs and other text.
pub(crate) fn redact_login_links(text: &str) -> String {
    login_links()
        .replace_all(text, "[sign-in link]")
        .into_owned()
}

fn redact_connector_links(text: &str) -> String {
    static LINKS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        let gap = r"(?:\x1b(?:\[[0-?]*[ -/]*[@-~]?|\][0-?]*|[PX^_\\])?|[\x00-\x08\x0b\x0c\x0e-\x1f\x7f-\x9f]|\r|\p{Cf})*";
        let host = "login.tailscale.com/a/"
            .chars()
            .map(|ch| regex::escape(&ch.to_string()))
            .collect::<Vec<_>>()
            .join(gap);
        regex::Regex::new(&format!(
            r"(?i)(?:[a-z\r]*:?\r*/\r*/)?{host}{gap}[ \t\r\n]*[A-Za-z0-9_-]+(?:\r+[A-Za-z0-9_-]+)*"
        ))
        .expect("fixed connector login-link pattern")
    });
    LINKS.replace_all(text, "[sign-in link]").into_owned()
}

fn clean_connector_strings(text: &str) -> (String, String) {
    static STRINGS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?:\x1b[PX^_]|[\x{90}\x{98}\x{9e}\x{9f}])[^\x1b\x18\x1a\r\n\x{85}\x{2028}\x{2029}\x{9c}]*(?:\x1b\\|\x{9c})|(?:\x1b\]|\x{9d})[^\x1b\x07\x18\x1a\r\n\x{85}\x{2028}\x{2029}\x{9c}]*(?:\x07|\x1b\\|\x{9c})"
        ).expect("fixed connector console-string pattern")
    });
    let mut dropped = String::new();
    let cleaned = STRINGS
        .replace_all(text, |found: &regex::Captures| {
            if found[0].contains("[sign-in link]") {
                "[sign-in link]"
            } else {
                dropped.push_str(&found[0]);
                dropped.push('\n');
                ""
            }
        })
        .into_owned();
    (cleaned, dropped)
}

fn clean_console(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            line.split('\r')
                .map(service::clean_log)
                .collect::<Vec<_>>()
                .join("\r")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The raw pass masks one-piece links regardless of surrounding text. The sweep closes a single tested insertion
/// anywhere in a link whose identifier meets the value-hiding floor. Damage to both host and login path is best effort.
/// Detection views are never answered.
pub(crate) fn connector_log(text: &str, secrets: &[String]) -> (String, service::Hidden) {
    let shown = clean_console(&service::redact_blocks(text));
    let without_controls: String = text.chars().filter(|ch| !ch.is_control()).collect();
    static INTRODUCERS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\x1b(?:[\[\]][0-?]*)?").expect("fixed terminal-introducer pattern")
    });
    // A detection-only projection: discard introducers and parameters without consuming a following letter.
    let without_introducers = INTRODUCERS.replace_all(text, "");
    let mut values = secrets.to_vec();
    for view in [
        text,
        &shown,
        &without_controls,
        without_introducers.as_ref(),
    ] {
        for found in login_links().captures_iter(view) {
            values.push(found["id"].replace('\r', ""));
        }
    }
    let raw = clean_connector_strings(&redact_connector_links(&service::redact_blocks(text))).0;
    // Keep the fixed placeholder outside any unfinished terminal escape while rendering it.
    let raw = raw.replace("[sign-in link]", "\u{1b}[0m[sign-in link]");
    let answer = redact_login_links(&clean_console(&raw));
    static URL_IDS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"//[^\s/]*\.[^\s/]*/a/([A-Za-z0-9_-]{8,})")
            .expect("fixed URL-identifier pattern")
    });
    values.extend(
        URL_IDS
            .captures_iter(&answer)
            .map(|found| found[1].to_string()),
    );
    // values_to_hide keeps its existing 8-byte floor: short identifiers are hidden by shape, not as ordinary words.
    (answer, service::values_to_hide(&values))
}

/// Find additional identifiers without changing the ordinary renderer's hidden values.
pub(crate) fn connector_log_identifiers(text: &str) -> Vec<String> {
    let masked = redact_connector_links(&service::redact_blocks(text));
    let detection = redact_login_links(&clean_console(
        &masked.replace("[sign-in link]", "\u{1b}[0m[sign-in link]"),
    ));
    let (_, dropped) = clean_connector_strings(&masked);
    let dropped = redact_login_links(&clean_console(&dropped));
    static URL_IDS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"//[^\s/]*\.[^\s/]*/a/([A-Za-z0-9_-]{8,})")
            .expect("fixed URL-identifier pattern")
    });
    let dropped_links: std::collections::HashSet<_> = URL_IDS
        .find_iter(&dropped)
        .map(|found| found.as_str())
        .collect();
    let mut identifiers: Vec<_> = URL_IDS
        .captures_iter(&detection)
        .filter(|found| !dropped_links.contains(&found[0]))
        .map(|found| found[1].to_string())
        .collect();
    identifiers.sort_unstable();
    identifiers.dedup();
    identifiers.sort_by_key(|value| std::cmp::Reverse(value.len()));
    identifiers
}

/// Apply additional identifier masking only after the ordinary connector line is finished.
pub(crate) fn redact_connector_identifiers(text: &str, identifiers: &[String]) -> String {
    identifiers.iter().fold(text.to_string(), |line, value| {
        line.replace(value.as_str(), "[secret]")
    })
}

/// Finish connector redaction after cleaning and value hiding, including adjacent identifier continuations.
pub(crate) fn finish_connector_log(text: &str) -> String {
    static REMAINDERS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"\[sign-in link\][\p{Cf}A-Za-z0-9_-]+|(?i:(?:[a-z]*:?//)?login\.tailscale\.com/a/?)[A-Za-z0-9_-]{8,}"
        ).expect("fixed connector-log remainder pattern")
    });
    REMAINDERS.replace_all(text, "[sign-in link]").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn login_link_redaction_keeps_punctuation_and_hides_the_whole_identifier() {
        assert_eq!(
            redact_login_links("visit https://login.tailscale.com/a/synthetic-id- now"),
            "visit [sign-in link] now"
        );
        assert_eq!(
            redact_login_links("(https://login.tailscale.com/a/synthetic_123)."),
            "([sign-in link])."
        );
        assert_eq!(
            redact_login_links("https://login.tailscale.com/admin/machines"),
            "https://login.tailscale.com/admin/machines"
        );
    }

    fn rendered_connector_lines(raw: &str) -> Vec<String> {
        let identifiers = connector_log_identifiers(raw);
        let (text, hidden) = connector_log(raw, &[]);
        text.split(['\n', '\r'])
            .filter(|line| !line.trim().is_empty())
            .map(service::clean_log)
            .filter(|line| !line.is_empty())
            .map(|line| {
                let finished = finish_connector_log(&service::redact_hidden(&line, &hidden));
                redact_connector_identifiers(&finished, &identifiers)
            })
            .collect()
    }

    fn visible_identifier_piece(id: &str, text: &str) -> usize {
        assert!(id.is_ascii());
        (1..=id.len())
            .rev()
            .find(|&len| (0..=id.len() - len).any(|start| text.contains(&id[start..start + len])))
            .unwrap_or(0)
    }

    #[test]
    fn connector_privacy_two_position_sweep() {
        let mut failures = Vec::new();
        for (id, expected_rows) in [("syntheticSweep1234", 14640), ("Qz6Xwq", 9408)] {
            let line = format!("visit https://login.tailscale.com/a/{id} done\n");
            let mut rows = 0;
            let mut failed = 0;
            let mut samples = Vec::new();
            for (index, (intro, term)) in [
                ("\u{1b}]0;", "\u{7}"),
                ("\u{1b}]0;", "\u{1b}\\"),
                ("\u{1b}]8;;", "\u{1b}\\"),
                ("\u{1b}P", "\u{1b}\\"),
                ("\u{1b}_", "\u{1b}\\"),
                ("\u{9d}", "\u{7}"),
                ("\u{90}", "\u{9c}"),
                ("\u{1b}X", "\u{9c}"),
            ]
            .into_iter()
            .enumerate()
            {
                for first in 0..line.len() {
                    for second in first..line.len() {
                        let raw = format!(
                            "{}{intro}{}{term}{}",
                            &line[..first],
                            &line[first..second],
                            &line[second..]
                        );
                        let lines = rendered_connector_lines(&raw);
                        let longest = visible_identifier_piece(id, &lines.concat());
                        rows += 1;
                        if longest >= 4 {
                            failed += 1;
                            if samples.len() < 8 {
                                samples.push(format!("row={index} first={first} second={second} longest={longest} answer={lines:?}"));
                            }
                        }
                    }
                }
            }
            assert_eq!(rows, expected_rows);
            println!(
                "two-position sweep identifier-length={} rows={rows} failures={failed}",
                id.len()
            );
            if failed > 0 {
                failures.push(format!(
                    "identifier-length={} failures={failed}: {}",
                    id.len(),
                    samples.join("\n")
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn connector_privacy_four_samples() {
        let id = "syntheticSweep1234";
        let mut rows = 0;
        for (intro, term) in [
            ("\u{1b}]0;", "\u{7}"),
            ("\u{1b}]8;;", "\u{1b}\\"),
            ("\u{1b}P", "\u{1b}\\"),
            ("\u{9d}", "\u{7}"),
        ] {
            let raw = format!("{intro}visit https://l{term};ogin.tailscale.com/a/{id} done\n");
            let lines = rendered_connector_lines(&raw);
            let longest = visible_identifier_piece(id, &lines.concat());
            assert!(longest < 4, "longest={longest} answer={lines:?}");
            assert_eq!(lines, [";ogin.tailscale.com/a/[secret] done"]);
            rows += 1;
        }
        assert_eq!(rows, 4);
    }

    #[test]
    fn connector_privacy_preserves_sign_in_identifier_masking() {
        let raw = "https://login.tailscale.com/a/abcdefghijkl\n\u{1b}]0;x //h.io/a/ghijklMOREMOREMORE\u{7}\ntoken abcdefghijklMOREMOREMORE end\n";
        assert_eq!(
            rendered_connector_lines(raw),
            ["[sign-in link]", "token [secret]MOREMOREMORE end"]
        );
    }

    #[test]
    fn connector_privacy_preserves_ordinary_words() {
        let raw = "see \u{1b}]8;;https://docs.example.com/a/installation\u{7} docs\u{1b}]8;;\u{7}\nrun installation now\n";
        assert_eq!(
            rendered_connector_lines(raw),
            ["see docs", "run installation now"]
        );
    }

    #[test]
    fn connector_privacy_preserves_ordinary_status() {
        let raw = "\u{1b}P1$r//x.example.com/a/sessionabc\u{1b}\\\nsessionabc is fine\n";
        assert_eq!(rendered_connector_lines(raw), ["sessionabc is fine"]);
    }

    #[test]
    fn connector_privacy_preserves_identifier_precedence() {
        let raw = "https://login.tailscale.com/a/abcdefghijkl\n\u{1b}]0;x //h.io/a/ghijklMO\u{7}REMOREMORE\ntoken abcdefghijklMOREMOREMORE end\n";
        assert_eq!(
            rendered_connector_lines(raw),
            [
                "[sign-in link]",
                "REMOREMORE",
                "token [secret]MOREMOREMORE end"
            ]
        );
    }

    #[test]
    fn connector_privacy_stored_value_preservation() {
        let secret = "syntheticSecret12345";
        assert_eq!(secret.len(), 20);
        let raw = "\u{1b}]0;x //h.io/a/ret12345Mo\u{7}reSyntheticSuffix\ntoken syntheticSecret12345MoreSyntheticSuffix end\n";
        let (text, hidden) = connector_log(raw, &[secret.to_string()]);
        let lines: Vec<_> = text
            .split(['\n', '\r'])
            .filter(|line| !line.trim().is_empty())
            .map(service::clean_log)
            .filter(|line| !line.is_empty())
            .map(|line| finish_connector_log(&service::redact_hidden(&line, &hidden)))
            .collect();
        assert_eq!(
            lines,
            ["reSyntheticSuffix", "token [secret]MoreSyntheticSuffix end"]
        );
    }

    #[test]
    fn connector_privacy_independent_value_preservation() {
        let raw = "https://login.tailscale.com/a/syntheticOther99s\n\u{1b}]0;visit https://l\u{7};ogin.tailscale.com/a/syntheticSweep1234 done\n";
        let lines = rendered_connector_lines(raw);
        assert!(visible_identifier_piece("syntheticSweep1234", &lines.concat()) < 4);
        assert_eq!(
            lines,
            ["[sign-in link]", ";ogin.tailscale.com/a/[secret] done"]
        );
    }

    fn baseline_connector_log(text: &str, secrets: &[String]) -> (String, service::Hidden) {
        let shown = clean_console(&service::redact_blocks(text));
        let without_controls: String = text.chars().filter(|ch| !ch.is_control()).collect();
        static INTRODUCERS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"\x1b(?:[\[\]][0-?]*)?").expect("fixed terminal-introducer pattern")
        });
        // A detection-only projection: discard introducers and parameters without consuming a following letter.
        let without_introducers = INTRODUCERS.replace_all(text, "");
        let mut values = secrets.to_vec();
        for view in [
            text,
            &shown,
            &without_controls,
            without_introducers.as_ref(),
        ] {
            for found in login_links().captures_iter(view) {
                values.push(found["id"].replace('\r', ""));
            }
        }
        let raw = clean_connector_strings(&redact_connector_links(&service::redact_blocks(text))).0;
        // Keep the fixed placeholder outside any unfinished terminal escape while rendering it.
        let raw = raw.replace("[sign-in link]", "\u{1b}[0m[sign-in link]");
        let answer = redact_login_links(&clean_console(&raw));
        static URL_IDS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"//[^\s/]*\.[^\s/]*/a/([A-Za-z0-9_-]{8,})")
                .expect("fixed URL-identifier pattern")
        });
        values.extend(
            URL_IDS
                .captures_iter(&answer)
                .map(|found| found[1].to_string()),
        );
        // values_to_hide keeps its existing 8-byte floor: short identifiers are hidden by shape, not as ordinary words.
        (answer, service::values_to_hide(&values))
    }

    #[test]
    fn connector_privacy_last_pass_preserves_hidden_positions() {
        // The ordinary renderer must still give the baseline text and finished lines.
        // Every character of an existing hiding marker must survive the last pass;
        // additional masking may replace visible text but cannot uncover hidden positions.
        let id = "syntheticSweep1234";
        let line = format!("visit https://login.tailscale.com/a/{id} done\n");
        let id_start = line.find(id).unwrap();
        let markers = regex::Regex::new(r"\[secret\]|\[sign-in link\]").unwrap();
        let render = |text: &str, hidden: &service::Hidden| {
            text.split(['\n', '\r'])
                .filter(|line| !line.trim().is_empty())
                .map(service::clean_log)
                .filter(|line| !line.is_empty())
                .map(|line| finish_connector_log(&service::redact_hidden(&line, hidden)))
                .collect::<Vec<_>>()
        };
        let mut rows = 0;
        for (intro, term) in [
            ("\u{1b}]0;", "\u{7}"),
            ("\u{1b}]8;;", "\u{1b}\\"),
            ("\u{1b}P", "\u{1b}\\"),
            ("\u{9d}", "\u{7}"),
        ] {
            for first in 0..id_start {
                for second in first + 1..=id_start {
                    for extra in [
                        ";", "_", ".", "%", "\u{1b}7", "\u{1b})0", "\u{1b}=", "\u{9b}1m", " ",
                        "\t", "\u{3164}",
                    ] {
                        let raw = format!(
                            "{}{intro}{}{term}{extra}{}",
                            &line[..first],
                            &line[first..second],
                            &line[second..]
                        );
                        let (text, hidden) = connector_log(&raw, &[]);
                        let (baseline_text, baseline_hidden) = baseline_connector_log(&raw, &[]);
                        assert_eq!(text, baseline_text, "row={rows}");
                        let before = render(&text, &hidden);
                        assert_eq!(
                            before,
                            render(&baseline_text, &baseline_hidden),
                            "row={rows}"
                        );
                        let identifiers = connector_log_identifiers(&raw);
                        for line in before {
                            let after = redact_connector_identifiers(&line, &identifiers);
                            let mut protected = String::new();
                            let mut hidden_positions = Vec::new();
                            let mut end = 0;
                            for marker in markers.find_iter(&line) {
                                protected.push_str(&redact_connector_identifiers(
                                    &line[end..marker.start()],
                                    &identifiers,
                                ));
                                let output_start = protected.chars().count();
                                protected.push_str(marker.as_str());
                                hidden_positions.extend(
                                    marker
                                        .as_str()
                                        .chars()
                                        .enumerate()
                                        .map(|(offset, ch)| (output_start + offset, ch)),
                                );
                                end = marker.end();
                            }
                            protected.push_str(&redact_connector_identifiers(
                                &line[end..],
                                &identifiers,
                            ));
                            assert_eq!(after, protected, "row={rows}");
                            let after: Vec<_> = after.chars().collect();
                            for (position, ch) in hidden_positions {
                                assert_eq!(after[position], ch, "row={rows} position={position}");
                            }
                        }
                        rows += 1;
                    }
                }
            }
        }
        assert_eq!(rows, 29304);
    }

    #[test]
    fn connector_privacy_host_continuations() {
        let mut rows = 0;
        let mut failures = Vec::new();
        for id in ["syntheticSweep1234", "Qz6Xwq"] {
            for scheme in ["https://", ""] {
                let url = format!("{scheme}login.tailscale.com/a/{id}");
                for offset in scheme.len()..=url.len() - id.len() {
                    for insert in ["\u{200b}", "\u{ad}", "\u{200d}", "\u{feff}", "\u{202e}"] {
                        let raw =
                            format!("visit {}{insert}{} done\n", &url[..offset], &url[offset..]);
                        let lines = rendered_connector_lines(&raw);
                        let longest = visible_identifier_piece(id, &lines.concat());
                        rows += 1;
                        if longest >= 4 {
                            failures.push(format!("row={rows} longest={longest} answer={lines:?}"));
                        }
                    }
                }
            }
        }
        assert_eq!(rows, 460);
        println!(
            "host-continuation sweep rows={rows} failures={}",
            failures.len()
        );
        assert!(
            failures.is_empty(),
            "{}",
            failures
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

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
        let cfg: Arc<NodeConfig> = Arc::new(serde_json::from_value(json!({"stateDir":dir,"incus":{"socket":fake.socket()},"listen":{"unix":dir.join("arca.sock")},"tokens":{"hashDir":dir.join("tokens")},"kernel":{"projects":[arca_netproject::PROJECT],"droiddProjects":[],"serviceCidr":"192.0.2.0/24"}})).unwrap());
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
            limits: Default::default(),
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
            &json!({"AuthURL":link,"BackendState":"NeedsLogin","Unrelated":"ignored"}).to_string(),
        ));
        assert_eq!(
            reach_sign_in(&kernel, &cfg, &exec, &caller, "node-ts")
                .unwrap()
                .as_deref(),
            Some(link)
        );
        for state in ["Running", ""] {
            fake.script_exec(ExecScript::new(
                ExecCommand::ConnectorStatus.argv(),
                0,
                &json!({"BackendState":state,"AuthURL":link}).to_string(),
            ));
            assert!(
                reach_sign_in(&kernel, &cfg, &exec, &caller, "node-ts")
                    .unwrap()
                    .is_none()
            );
        }
        fake.script_exec(ExecScript::new(
            ExecCommand::ConnectorStatus.argv(),
            0,
            r#"{"BackendState":"NeedsLogin","AuthURL":"https://example.com/wrong"}"#,
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
