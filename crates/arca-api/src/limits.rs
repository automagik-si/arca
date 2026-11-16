//! Tokens limited to named services: what each verb touches ([`Target`]), whether a call of a limited token stays inside
//! its list ([`check`]), and the cut of a read's answer to that list ([`cut`]).
//!
//! A limit is a list of services ([`ServiceLimit`]). An entry is a service of the node's own service project, by its name
//! (`dev`), or a service inside a tenant, as the tenant's id, a slash and the service's name (`abcdefgh/web`). Every verb
//! says in the table what it touches. A call of a limited token is refused before its handler runs when what it touches
//! is outside the list, and when it touches the whole node. The one exception is a read whose answer is cut to the list
//! ([`Target::Filtered`]): it runs, and the door hands back only the rows of the listed services, never another's name.
//!
//! An action proposal is checked when it is made, against the proposed action's own target: once the owner approves it,
//! it runs as the owner's action, outside any token's limit, so the check cannot wait until then.

use serde_json::Value;

use crate::verbs::{Verb, verb};

/// The node's own service project: the Incus project every service of the node itself is an instance of, under the
/// service's own name. The `arca` crate's tests hold it equal to `arca_netproject::PROJECT`.
pub const SERVICE_PROJECT: &str = "arcak-svc";

/// The prefixes of a tenant's project, followed by the tenant's id: a node's own and a scratch run's. The `arca` crate's
/// tests hold them equal to `arca_kernel::ownership`'s.
pub const TENANT_PROJECT_PREFIXES: [&str; 2] = ["arcak-t-", "arcak-scratch-t-"];

/// The most services one token may name.
pub const MAX_SERVICES: usize = 64;

/// The refusal code of a call that reaches outside its token's services.
pub const OUTSIDE_SERVICES: &str = "OutsideTokenServices";

/// What a verb touches, as far as a token limited to named services is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The whole node, or something no service names (a tenant, a disk, a plan, a function, a route, a database). A
    /// limited token is refused.
    Node,
    /// One service, named by this argument, inside the tenant the call's `tenant` names when it names one.
    Service(&'static str),
    /// Two services, named by these two arguments, in one place: both must be on the list.
    Services(&'static str, &'static str),
    /// One instance: `project` and `instance` (the node's own service project only), or inside a tenant its `service` or
    /// its `instance`.
    Instance,
    /// A read of the whole node whose answer the door cuts to the token's services ([`cut`]).
    Filtered,
    /// `action_propose`: the proposed action's target, checked against the proposal's `args`.
    Proposal,
}

/// The services a token is limited to. Each entry is a service of the node's own project (no tenant) or of one tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceLimit {
    entries: Vec<(Option<String>, String)>,
}

impl ServiceLimit {
    /// The list a token file's `services=` line, or a `--services` flag, holds: a comma list of 1 to [`MAX_SERVICES`]
    /// entries, each `<service>` or `<tenant>/<service>`, none twice. `None` for anything else.
    pub fn parse(text: &str) -> Option<ServiceLimit> {
        let mut entries: Vec<(Option<String>, String)> = Vec::new();
        for item in text.split(',') {
            let entry = match item.split_once('/') {
                Some((tenant, service)) => (tenant_id_ok(tenant) && service_name_ok(service))
                    .then(|| (Some(tenant.to_string()), service.to_string()))?,
                None => service_name_ok(item).then(|| (None, item.to_string()))?,
            };
            if entries.contains(&entry) {
                return None;
            }
            entries.push(entry);
        }
        if entries.is_empty() || entries.len() > MAX_SERVICES {
            return None;
        }
        entries.sort();
        Some(ServiceLimit { entries })
    }

    /// The list as a token file and a flag spell it, sorted.
    pub fn as_line(&self) -> String {
        self.entries
            .iter()
            .map(|(tenant, service)| match tenant {
                Some(tenant) => format!("{tenant}/{service}"),
                None => service.clone(),
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Whether the service `service`, of the tenant `tenant` or of the node's own project, is on the list.
    pub fn allows(&self, tenant: Option<&str>, service: &str) -> bool {
        self.entries
            .iter()
            .any(|(t, s)| t.as_deref() == tenant && s == service)
    }

    /// Whether every service of this list is on `wider` too.
    pub fn within(&self, wider: &ServiceLimit) -> bool {
        self.entries
            .iter()
            .all(|(tenant, service)| wider.allows(tenant.as_deref(), service))
    }

    /// Whether the instance `name` of the Incus project `project` is a listed service's: a service of the node's own
    /// project under its name, or a tenant's service under `<service>-<tenant id>` in that tenant's project.
    pub fn allows_instance(&self, project: &str, name: &str) -> bool {
        if project == SERVICE_PROJECT {
            return self.allows(None, name);
        }
        let Some(tenant) = tenant_of_project(project) else {
            return false;
        };
        self.allows_tenant_instance(tenant, name)
    }

    /// Whether `instance`, an instance of the tenant `tenant`'s project, is the instance of a listed service of it.
    fn allows_tenant_instance(&self, tenant: &str, instance: &str) -> bool {
        instance
            .strip_suffix(tenant)
            .and_then(|rest| rest.strip_suffix('-'))
            .is_some_and(|service| self.allows(Some(tenant), service))
    }
}

/// The tenant whose project `project` is, by its name alone.
fn tenant_of_project(project: &str) -> Option<&str> {
    TENANT_PROJECT_PREFIXES
        .iter()
        .find_map(|prefix| project.strip_prefix(prefix))
        .filter(|id| tenant_id_ok(id))
}

/// A tenant id: eight characters of lowercase base32 (`a-z`, `2-7`), as `arca_kernel::ownership::tenant_id_ok`.
fn tenant_id_ok(id: &str) -> bool {
    id.len() == 8
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
}

/// A service name: 1 to 40 of `a-z`, `0-9` and inner `-`, starting with a letter (the verbs' service name pattern).
fn service_name_ok(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 40
        && bytes[0].is_ascii_lowercase()
        && bytes[bytes.len() - 1] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Why a limited token's call is refused: the words, never a name from the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outside(pub String);

impl Outside {
    fn new(verb: &str, why: &str) -> Outside {
        Outside(format!(
            "{verb} is refused: this token is limited to named services, and {why}"
        ))
    }
}

/// Whether a call of `verb` with `args` stays inside `limit`. `args` has already met the verb's params schema.
pub fn check(verb: &Verb, limit: &ServiceLimit, args: &Value) -> Result<(), Outside> {
    check_target(verb.name, verb.target, limit, args, true)
}

fn check_target(
    name: &str,
    target: Target,
    limit: &ServiceLimit,
    args: &Value,
    top: bool,
) -> Result<(), Outside> {
    let outside = |why: &str| Err(Outside::new(name, why));
    let tenant = match args.get("tenant") {
        None => None,
        Some(Value::String(tenant)) => Some(tenant.as_str()),
        Some(_) => return outside("the call names no tenant it may reach"),
    };
    let named = |field: &str| args.get(field).and_then(Value::as_str);
    match target {
        Target::Node => outside("this verb reaches the whole node"),
        Target::Service(field) => match named(field) {
            Some(service) if limit.allows(tenant, service) => Ok(()),
            Some(_) => outside("the service it names is not on the token's list"),
            None => outside("the call names no service"),
        },
        Target::Services(a, b) => match (named(a), named(b)) {
            (Some(a), Some(b)) if limit.allows(tenant, a) && limit.allows(tenant, b) => Ok(()),
            (Some(_), Some(_)) => outside("a service it names is not on the token's list"),
            _ => outside("the call names no service"),
        },
        Target::Instance => {
            let allowed = match (tenant, named("instance"), named("service")) {
                (Some(tenant), None, Some(service)) => limit.allows(Some(tenant), service),
                (Some(tenant), Some(instance), None) => {
                    limit.allows_tenant_instance(tenant, instance)
                }
                (None, Some(instance), None) => {
                    named("project") == Some(SERVICE_PROJECT) && limit.allows(None, instance)
                }
                _ => false,
            };
            if allowed {
                Ok(())
            } else {
                outside("the instance it names is not a listed service's")
            }
        }
        Target::Filtered if top => Ok(()),
        Target::Proposal if top => {
            let Some(inner) = named("action").and_then(verb) else {
                return outside("the action it proposes is not one this token may reach");
            };
            let inner_args = args.get("args").cloned().unwrap_or(Value::Null);
            check_target(inner.name, inner.target, limit, &inner_args, false)
                .map_err(|Outside(why)| Outside(format!("action_propose: {why}")))
        }
        // A proposal of a read, or of a proposal, is no action the owner could approve.
        Target::Filtered | Target::Proposal => {
            outside("this verb is not an action a proposal may carry")
        }
    }
}

/// The answer of `verb`, called with `args` by a token limited to `limit`, cut to the listed services: rows of other
/// services are dropped, and so is every link to one. A verb whose answer names no other service is handed back as it is.
/// `None` for a [`Target::Filtered`] verb this function has no cut for: the door then refuses the answer whole.
pub fn cut(verb: &Verb, limit: &ServiceLimit, args: &Value, mut value: Value) -> Option<Value> {
    let tenant = args.get("tenant").and_then(Value::as_str);
    match verb.name {
        "service_list" => {
            retain(&mut value, "services", |row| {
                row.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| limit.allows(tenant, name))
            });
            if let Some(rows) = value.get_mut("services").and_then(Value::as_array_mut) {
                for row in rows {
                    cut_links(row, limit, tenant);
                }
            }
            Some(value)
        }
        "service_status" => {
            cut_links(&mut value, limit, tenant);
            Some(value)
        }
        "snapshot_list" => {
            retain(&mut value, "snapshots", |row| {
                let text = |key: &str| row.get(key).and_then(Value::as_str);
                matches!(text("kind"), Some("container" | "virtual-machine"))
                    && match (text("sourceProject"), text("sourceName")) {
                        (Some(project), Some(name)) => {
                            limit.allows_instance(project, name)
                                && (tenant.is_some() == (project != SERVICE_PROJECT))
                        }
                        _ => false,
                    }
            });
            Some(value)
        }
        "watch_report" => {
            let instance_ok = |project: &str, row: &Value| {
                row.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| limit.allows_instance(project, name))
            };
            if let Some(projects) = value.get_mut("projects").and_then(Value::as_array_mut) {
                for project in projects.iter_mut() {
                    let name = project
                        .get("project")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    retain(project, "instances", |row| instance_ok(&name, row));
                }
                // A project left with no listed instance goes too: its name is not the token's to know.
                projects.retain(|project| {
                    project
                        .get("instances")
                        .and_then(Value::as_array)
                        .is_some_and(|rows| !rows.is_empty())
                });
            }
            retain(&mut value, "drift", |row| {
                row.get("project")
                    .and_then(Value::as_str)
                    .is_some_and(|project| instance_ok(project, row))
            });
            Some(value)
        }
        _ if verb.target == Target::Filtered => None,
        _ => Some(value),
    }
}

/// Keeps the rows of the array under `key` for which `keep` holds. Anything that is not such an array is left as it is.
fn retain(value: &mut Value, key: &str, keep: impl Fn(&Value) -> bool) {
    if let Some(rows) = value.get_mut(key).and_then(Value::as_array_mut) {
        rows.retain(|row| keep(row));
    }
}

/// Drops every link of a service view to a service that is not on the list.
fn cut_links(view: &mut Value, limit: &ServiceLimit, tenant: Option<&str>) {
    retain(view, "links", |link| {
        link.get("to")
            .and_then(Value::as_str)
            .is_some_and(|to| limit.allows(tenant, to))
    });
}

/// The verbs a limited token is offered in a tool list: every verb but those it is always refused.
pub fn offered(verb: &Verb) -> bool {
    verb.target != Target::Node
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verbs::verbs;
    use serde_json::json;

    fn limit(text: &str) -> ServiceLimit {
        ServiceLimit::parse(text).expect("a list")
    }

    fn v(name: &str) -> &'static Verb {
        verb(name).expect("a verb")
    }

    #[test]
    fn a_list_parses_sorted_and_refuses_what_is_not_a_service() {
        assert_eq!(limit("gtm,dev").as_line(), "dev,gtm");
        // The node's own services sort before any tenant's.
        assert_eq!(limit("abcdefgh/web,dev").as_line(), "dev,abcdefgh/web");
        for bad in [
            "",
            ",",
            "dev,",
            "dev,dev",
            "Dev",
            "1dev",
            "dev-",
            "a/web",
            "abcdefg1/web",
            "abcdefgh/",
            "abcdefgh/web/x",
            &"a".repeat(41),
        ] {
            assert_eq!(ServiceLimit::parse(bad), None, "{bad:?}");
        }
        let many: Vec<String> = (0..=MAX_SERVICES).map(|i| format!("s{i}")).collect();
        assert_eq!(ServiceLimit::parse(&many.join(",")), None);
        assert!(ServiceLimit::parse(&many[..MAX_SERVICES].join(",")).is_some());
    }

    #[test]
    fn allows_names_the_tenant_too() {
        let list = limit("dev,abcdefgh/web");
        assert!(list.allows(None, "dev"));
        assert!(!list.allows(Some("abcdefgh"), "dev"));
        assert!(list.allows(Some("abcdefgh"), "web"));
        assert!(!list.allows(None, "web"));
        assert!(limit("dev").within(&list));
        assert!(!limit("dev,gtm").within(&list));
    }

    #[test]
    fn a_service_target_needs_its_name_on_the_list() {
        let list = limit("dev");
        let status = v("service_status");
        assert_eq!(status.target, Target::Service("name"));
        assert!(check(status, &list, &json!({"name": "dev"})).is_ok());
        let refused = check(status, &list, &json!({"name": "gtm"})).unwrap_err();
        assert!(refused.0.contains("not on the token's list"), "{refused:?}");
        assert!(!refused.0.contains("gtm"), "a refusal never echoes a name");
        assert!(check(status, &list, &json!({"name": "dev", "tenant": "abcdefgh"})).is_err());
        assert!(check(status, &list, &json!({})).is_err());
        assert!(check(status, &list, &json!({"name": "dev", "tenant": 7})).is_err());
    }

    #[test]
    fn a_pair_needs_both_on_the_list() {
        let list = limit("dev,gtm");
        let connect = v("service_connect");
        assert!(check(connect, &list, &json!({"from": "dev", "to": "gtm"})).is_ok());
        assert!(check(connect, &list, &json!({"from": "dev", "to": "cx"})).is_err());
        assert!(check(connect, &list, &json!({"from": "cx", "to": "dev"})).is_err());
        assert!(check(connect, &list, &json!({"from": "dev"})).is_err());
    }

    #[test]
    fn an_instance_is_a_listed_services_in_its_own_place() {
        let list = limit("dev,abcdefgh/web");
        let start = v("instance_start");
        assert_eq!(start.target, Target::Instance);
        let ok = |args: Value| check(start, &list, &args).is_ok();
        assert!(ok(json!({"project": "arcak-svc", "instance": "dev"})));
        assert!(!ok(json!({"project": "arcak-svc", "instance": "gtm"})));
        assert!(!ok(json!({"project": "other", "instance": "dev"})));
        assert!(!ok(json!({"instance": "dev"})));
        assert!(ok(json!({"tenant": "abcdefgh", "service": "web"})));
        assert!(ok(
            json!({"tenant": "abcdefgh", "instance": "web-abcdefgh"})
        ));
        assert!(!ok(json!({"tenant": "abcdefgh", "instance": "web"})));
        assert!(!ok(
            json!({"tenant": "abcdefgh", "instance": "web-bcdefghi"})
        ));
        assert!(!ok(json!({"tenant": "abcdefgh", "service": "dev"})));
        assert!(!ok(json!({"tenant": "bcdefghi", "service": "web"})));
        assert!(!ok(json!({"service": "dev"})));
        assert!(!ok(
            json!({"tenant": "abcdefgh", "service": "web", "instance": "web-abcdefgh"})
        ));
    }

    #[test]
    fn a_node_wide_verb_is_refused_and_a_filtered_read_is_not() {
        let list = limit("dev");
        let refused = check(v("arca_status"), &list, &json!({})).unwrap_err();
        assert!(refused.0.contains("whole node"), "{refused:?}");
        assert!(check(v("tenant_list"), &list, &json!({})).is_err());
        assert!(check(v("journal_list"), &list, &json!({})).is_err());
        assert!(check(v("service_list"), &list, &json!({})).is_ok());
        assert!(check(v("owner_approval_list"), &list, &json!({})).is_err());
    }

    #[test]
    fn a_proposal_is_checked_against_its_action() {
        let list = limit("dev");
        let propose = v("action_propose");
        let call = |action: &str, args: Value| {
            check(propose, &list, &json!({"action": action, "args": args}))
        };
        assert!(call("service_remove", json!({"name": "dev"})).is_ok());
        let refused = call("service_remove", json!({"name": "gtm"})).unwrap_err();
        assert!(
            refused.0.starts_with("action_propose: service_remove"),
            "{refused:?}"
        );
        assert!(
            call(
                "instance_stop",
                json!({"project": "arcak-svc", "instance": "dev"})
            )
            .is_ok()
        );
        assert!(
            call(
                "instance_stop",
                json!({"project": "arcak-svc", "instance": "gtm"})
            )
            .is_err()
        );
        assert!(call("tenant_create", json!({"id": "abcdefgh"})).is_err());
        assert!(call("host_network_restore", json!({})).is_err());
        assert!(check(propose, &list, &json!({"action": "nothing", "args": {}})).is_err());
        assert!(check(propose, &list, &json!({"args": {"name": "dev"}})).is_err());
        assert!(call("service_list", json!({})).is_err());
        assert!(
            call(
                "action_propose",
                json!({"action": "service_remove", "args": {"name": "dev"}})
            )
            .is_err()
        );
    }

    #[test]
    fn every_proposable_action_has_a_target_that_is_not_a_read() {
        for name in crate::verbs::ACTION_NAMES {
            let target = v(name).target;
            assert!(
                !matches!(target, Target::Filtered | Target::Proposal),
                "{name}: {target:?}"
            );
        }
    }

    #[test]
    fn every_target_names_arguments_its_verb_takes() {
        for verb in verbs() {
            let params = (verb.params)();
            let has = |field: &str| params["properties"].get(field).is_some();
            match verb.target {
                Target::Service(field) => assert!(has(field), "{}: {field}", verb.name),
                Target::Services(a, b) => {
                    assert!(has(a) && has(b), "{}: {a}, {b}", verb.name)
                }
                Target::Instance => assert!(
                    has("instance") && has("project") && has("service") && has("tenant"),
                    "{}",
                    verb.name
                ),
                Target::Proposal => assert!(has("action") && has("args"), "{}", verb.name),
                Target::Filtered => assert!(
                    !verb.writes && verb.read_only_hint(),
                    "{}: a filtered verb is a read",
                    verb.name
                ),
                Target::Node => {}
            }
        }
    }

    #[test]
    fn every_filtered_verb_has_a_cut() {
        let list = limit("dev");
        for verb in verbs().iter().filter(|v| v.target == Target::Filtered) {
            assert!(
                cut(verb, &list, &json!({}), json!({})).is_some(),
                "{} is filtered and has no cut",
                verb.name
            );
        }
    }

    #[test]
    fn service_list_keeps_listed_rows_and_their_listed_links_only() {
        let list = limit("dev,gtm");
        let answer = json!({"services": [
            {"name": "dev", "links": [{"to": "gtm", "port": 1}, {"to": "secret-db", "port": 2}]},
            {"name": "secret-db", "links": []},
            {"name": "gtm"},
            {"links": []}
        ]});
        let cut = cut(v("service_list"), &list, &json!({}), answer).unwrap();
        assert_eq!(
            cut,
            json!({"services": [
                {"name": "dev", "links": [{"to": "gtm", "port": 1}]},
                {"name": "gtm"}
            ]})
        );
        assert!(!cut.to_string().contains("secret-db"));
    }

    #[test]
    fn service_list_in_a_tenant_keeps_that_tenants_listed_services() {
        let list = limit("dev,abcdefgh/web");
        let answer = json!({"services": [{"name": "web"}, {"name": "dev"}, {"name": "api"}]});
        let cut = cut(
            v("service_list"),
            &list,
            &json!({"tenant": "abcdefgh"}),
            answer,
        )
        .unwrap();
        assert_eq!(cut, json!({"services": [{"name": "web"}]}));
    }

    #[test]
    fn service_status_drops_links_to_unlisted_services() {
        let list = limit("dev");
        let answer = json!({"name": "dev", "links": [{"to": "pg", "port": 5432}]});
        let cut = cut(v("service_status"), &list, &json!({"name": "dev"}), answer).unwrap();
        assert_eq!(cut, json!({"name": "dev", "links": []}));
    }

    #[test]
    fn snapshot_list_keeps_listed_instances_copies_only() {
        let list = limit("dev,abcdefgh/web");
        let row = |kind: &str, project: &str, name: &str| json!({"kind": kind, "sourceProject": project, "sourceName": name, "name": "s"});
        let answer = json!({"snapshots": [
            row("container", "arcak-svc", "dev"),
            row("virtual-machine", "arcak-svc", "dev"),
            row("container", "arcak-svc", "gtm"),
            row("volume", "arcak-svc", "dev"),
            row("folder", "arcak-svc", "dev"),
            row("container", "droidd", "dev"),
            row("container", "arcak-t-abcdefgh", "web-abcdefgh"),
            {"kind": "container", "sourceName": "dev"}
        ]});
        let own = cut(v("snapshot_list"), &list, &json!({}), answer.clone()).unwrap();
        assert_eq!(
            own["snapshots"],
            json!([
                row("container", "arcak-svc", "dev"),
                row("virtual-machine", "arcak-svc", "dev")
            ])
        );
        let tenant = cut(
            v("snapshot_list"),
            &list,
            &json!({"tenant": "abcdefgh"}),
            answer,
        )
        .unwrap();
        assert_eq!(
            tenant["snapshots"],
            json!([row("container", "arcak-t-abcdefgh", "web-abcdefgh")])
        );
    }

    #[test]
    fn watch_report_keeps_listed_instances_and_drops_other_projects_whole() {
        let list = limit("dev,abcdefgh/web");
        let answer = json!({
            "at": 1,
            "projects": [
                {"project": "arcak-svc", "instances": [{"name": "dev"}, {"name": "gtm"}]},
                {"project": "droidd-secret", "instances": [{"name": "dev"}]},
                {"project": "arcak-scratch-t-abcdefgh", "instances": [{"name": "web-abcdefgh"}, {"name": "api-abcdefgh"}]},
                {"project": "arcak-t-bcdefghi", "instances": [{"name": "web-bcdefghi"}]}
            ],
            "drift": [
                {"kind": "markedWithoutEntry", "entry": "e", "project": "arcak-svc", "name": "dev"},
                {"kind": "markedWithoutEntry", "entry": "e", "project": "arcak-svc", "name": "gtm"},
                {"kind": "entryWithoutObject", "entry": "service-gtm"}
            ]
        });
        let cut = cut(v("watch_report"), &list, &json!({}), answer).unwrap();
        assert_eq!(
            cut,
            json!({
                "at": 1,
                "projects": [
                    {"project": "arcak-svc", "instances": [{"name": "dev"}]},
                    {"project": "arcak-scratch-t-abcdefgh", "instances": [{"name": "web-abcdefgh"}]}
                ],
                "drift": [
                    {"kind": "markedWithoutEntry", "entry": "e", "project": "arcak-svc", "name": "dev"}
                ]
            })
        );
        for leak in ["gtm", "droidd-secret", "bcdefghi", "api-"] {
            assert!(!cut.to_string().contains(leak), "{leak}");
        }
    }

    #[test]
    fn an_answer_that_names_no_other_service_is_handed_back_whole() {
        let list = limit("dev");
        let lines = json!({"lines": ["a", "b"]});
        assert_eq!(
            cut(
                v("service_logs"),
                &list,
                &json!({"name": "dev"}),
                lines.clone()
            ),
            Some(lines)
        );
    }

    #[test]
    fn a_limited_token_is_offered_no_node_wide_verb() {
        assert!(offered(v("service_status")));
        assert!(offered(v("service_list")));
        assert!(!offered(v("arca_status")));
        assert!(!offered(v("token_list")));
    }
}
