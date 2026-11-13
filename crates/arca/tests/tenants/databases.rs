//! Where a new database comes from. On a node whose config names no `kernel.postgresImage`, `postgres_create` makes
//! the service `service_create` makes from the node's `postgres-autopg` template, its image fetched by digest, with
//! the password that template generates; a node whose config names the image keeps the old kind whole. Through neither door does a
//! call choose an image. Every image here but the shipped template's own is synthetic.
use super::*;
use arca::service::DATABASE_TEMPLATE;
use std::os::unix::fs::PermissionsExt;

/// Another digest-pinned image than the node's, as a caller that chose one would name.
const OTHER_IMAGE: &str = "docker.io/library/postgres@sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn kernel(image: Option<&str>) -> Value {
    let mut kernel = json!({
        "projects": [PROJECT],
        "droiddProjects": ["other"],
        "serviceCidr": CIDR
    });
    if let Some(image) = image {
        kernel["postgresImage"] = json!(image);
    }
    kernel
}

/// Puts the templates Arca ships in the node's state directory, exactly as `arca templates install` does, and one
/// plain template `app` beside them. The database template is the shipped file, unchanged.
fn install_templates(rig: &Rig) {
    arca::cmd_templates::install_into(&rig.dir.join("templates"), false)
        .unwrap_or_else(|_| panic!("the shipped templates install"));
    rig.template("app", app_template("app"));
}

/// Rewrites the node's database template with `edit` applied.
fn edit_database_template(rig: &Rig, edit: impl FnOnce(&mut Value)) {
    let path = rig
        .dir
        .join("templates")
        .join(format!("{DATABASE_TEMPLATE}.json"));
    let mut template: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("the template")).expect("json");
    edit(&mut template);
    fs::write(&path, template.to_string()).expect("the edited template");
}

/// A node as `arca init --enable-changes` leaves it: no `kernel.postgresImage`, and the templates Arca ships.
fn clean() -> Rig {
    let rig = Rig::with_kernel(kernel(None), empty_state());
    install_templates(&rig);
    rig
}

/// A node whose config names the image of its databases, with the shipped templates too.
fn configured() -> Rig {
    let rig = Rig::with_kernel(kernel(Some(PG_IMAGE)), empty_state());
    install_templates(&rig);
    rig
}

/// The image by digest the node's database template names now, read from the template as the node holds it.
fn pin(rig: &Rig) -> String {
    let template = arca::service::load_template(&rig.dir.join("templates"), DATABASE_TEMPLATE)
        .unwrap_or_else(|_| panic!("the database template loads"));
    arca::service::template_pin(&template).expect("it names an image by digest")
}

/// The fingerprint of the node's database template as it stands now: its file and its entrypoint script.
fn content(rig: &Rig) -> String {
    arca::service::load_template_content(&rig.dir.join("templates"), DATABASE_TEMPLATE)
        .unwrap_or_else(|_| panic!("the database template loads"))
        .digest
        .expect("its content has a fingerprint")
}

/// The card's last row for a template whose content has the fingerprint `digest`.
fn content_row(digest: &str) -> String {
    format!(
        "{digest}. Arca creates this only if the template on the node is still exactly this one."
    )
}

fn propose(rig: &Rig, args: Value) -> Result<Value, CallError> {
    rig.call(
        "action_propose",
        json!({ "action": "postgres_create", "args": args }),
    )
}

/// Approves a waiting proposal with the owner's passkey and runs it, as the approval page does.
fn approve(rig: &Rig, proposal: &Value) -> Result<Value, CallError> {
    let id = proposal["id"].as_str().expect("the proposal's id");
    let action = proposal["action"].as_str().expect("its action");
    let digest = proposal["argsDigest"].as_str().expect("its digest");
    let nonce = format!("{:064x}", rig.nonce.fetch_add(1, Ordering::SeqCst) + 1);
    let statement =
        arca::handlers::action_statement("node1", id, action, digest, rig.now(), &nonce)
            .expect("the statement");
    let approval = common::approve_statement(&rig.key, &statement);
    let approved = arca::handlers::approve_action(
        &mut rig.kernel.lock().expect("kernel"),
        &json!({ "plan": id, "approval": approval }),
        rig.now(),
    )?;
    ActionExecutor::new(
        rig.handlers
            .iter()
            .map(|(verb, handler)| (*verb, Arc::clone(handler))),
    )
    .run(&approved.action, &approved.args, &approved.grant)
}

fn password_file(rig: &Rig, name: &str) -> std::path::PathBuf {
    rig.dir
        .join("secrets")
        .join(format!("service-{name}"))
        .join("postgres-password")
}

fn has_entry(rig: &Rig, id: &str) -> bool {
    rig.entries().iter().any(|entry| entry.id.0 == id)
}

fn instance(rig: &Rig, name: &str) -> Option<Value> {
    rig.get(&format!("/1.0/instances/{name}?project={PROJECT}"))
}

fn databases(rig: &Rig) -> Vec<Value> {
    rig.call("postgres_list", json!({})).expect("the list")["databases"]
        .as_array()
        .expect("databases")
        .clone()
}

/// The source a service's record and its instance carry for the node's database template: the registry and the
/// digest, fetched by Incus itself.
fn fetched_source(rig: &Rig) -> Value {
    let pin = pin(rig);
    let (server, alias) = pin.split_once('/').expect("a registry and a repository");
    json!({
        "type": "image",
        "mode": "pull",
        "protocol": "oci",
        "server": format!("https://{server}"),
        "alias": alias
    })
}

const MADE_AS: &str = "A service from this node's postgres-autopg template, with the password that template generates; Arca keeps it. Your assistant can connect it to your other services, and removing that service deletes the database with its data.";
/// The same row for a template that generates no password: the card claims none.
const MADE_AS_PLAIN: &str = "A service from this node's postgres-autopg template. Your assistant can connect it to your other services, and removing that service deletes the database with its data.";

/// The shipped database template generates its password: a database made from a clean install starts by itself,
/// and no token ever sets its password.
#[test]
fn the_shipped_database_template_generates_its_password() {
    let (_, text) = arca::cmd_templates::SHIPPED
        .iter()
        .find(|(name, _)| *name == "postgres-autopg.json")
        .expect("the shipped database template");
    let template: Value = serde_json::from_str(text).expect("json");
    assert_eq!(template["generate"], json!({ "POSTGRES_PASSWORD": "" }));
}

#[test]
fn on_a_clean_node_a_proposed_database_is_the_templates_service_with_its_image_fetched_by_digest() {
    let rig = clean();
    let pin = pin(&rig);
    let proposal = propose(&rig, json!({ "name": "orders" })).expect("the proposal");
    // The kernel names the template and the image it pins: the card shows the image, and the approval binds both.
    let content = content(&rig);
    assert_eq!(
        proposal["args"],
        json!({
            "name": "orders", "template": DATABASE_TEMPLATE, "image": pin, "templateDigest": content
        })
    );
    assert_eq!(
        proposal["details"],
        json!([
            { "label": "Database", "value": "orders" },
            { "label": "Image", "value": pin },
            { "label": "Template", "value": DATABASE_TEMPLATE },
            { "label": "Made as", "value": MADE_AS },
            { "label": "Template content", "value": content_row(&content) }
        ])
    );
    assert!(
        !has_entry(&rig, "service-orders"),
        "a proposal makes nothing"
    );
    assert!(!password_file(&rig, "orders").exists());

    let made = approve(&rig, &proposal).expect("the approved create");
    assert_eq!(
        made,
        json!({ "name": "orders", "entry": "service-orders", "status": "pending", "service": true })
    );
    // It is a service, made from the template, and no database of the old kind.
    assert!(!has_entry(&rig, "postgres-orders"));
    let record = rig.entry("service-orders");
    assert_eq!(record["template"], DATABASE_TEMPLATE);
    assert_eq!(record["source"], fetched_source(&rig));
    assert_eq!(record["start"], true);
    let live = instance(&rig, "orders").expect("its instance");
    // The script goes to Incus as one line of base64 (Incus 7.6 refuses a line break); it decodes to the template's own.
    let entrypoint = live["config"]["oci.entrypoint"]
        .as_str()
        .expect("entrypoint");
    assert!(!entrypoint.contains('\n'));
    let encoded = entrypoint
        .split("printf %s ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .expect("the encoded script");
    let decoded = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64")
    };
    assert!(
        String::from_utf8(decoded)
            .expect("text")
            .contains("autopg postmaster"),
        "the template's own entrypoint script opens the database"
    );
    // The password is the one the template generates: 32 letters and digits in a private file, given to the
    // instance and in no answer. So the database starts, and no token ever sets its password.
    let file = password_file(&rig, "orders");
    let password = fs::read_to_string(&file).expect("the password");
    assert!(password.len() == 32 && password.bytes().all(|b| b.is_ascii_alphanumeric()));
    assert!(made.get("stoppedBecause").is_none());
    assert_eq!(
        record["environment"]["POSTGRES_PASSWORD"],
        "arca-secret:service-orders/postgres-password"
    );
    assert_eq!(live["config"]["environment.POSTGRES_PASSWORD"], password);
    assert_eq!(
        fs::metadata(&file).expect("its file").permissions().mode() & 0o777,
        0o600
    );
    assert!(!made.to_string().contains(&password));
    // Both lists show it; the database list says it is a service.
    let rows = databases(&rig);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "orders");
    assert_eq!(rows[0]["entry"], "service-orders");
    assert_eq!(rows[0]["port"], 5432);
    assert_eq!(rows[0]["links"], json!([]));
    assert_eq!(rows[0]["service"], true);
    assert!(!rows[0].to_string().contains(&password));
    let services = rig.call("service_list", json!({})).expect("services");
    assert!(
        services["services"]
            .as_array()
            .expect("services")
            .iter()
            .any(|service| service["name"] == "orders")
    );
}

#[test]
fn a_database_made_from_the_template_is_reached_and_removed_with_the_service_verbs() {
    let rig = clean();
    approve(
        &rig,
        &propose(&rig, json!({ "name": "orders" })).expect("the proposal"),
    )
    .expect("the database");
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app" }),
    )
    .expect("a service");
    rig.call("service_connect", json!({ "from": "web", "to": "orders" }))
        .expect("the link");
    // The service that connects is given the template's own URL: role app, database app.
    let web = rig.entry("service-web");
    assert!(
        web["links"]
            .as_array()
            .expect("links")
            .iter()
            .any(|link| link.to_string().contains("orders")),
        "{web}"
    );
    assert_eq!(databases(&rig)[0]["links"], json!(["web"]));
    // The database verbs of the old kind do not act on it, and say which verb does.
    let before = rig.entries();
    for (verb, args, instead) in [
        (
            "postgres_link",
            json!({ "name": "orders", "consumer": "web" }),
            "connect a service to it with service_connect, and take the link away with service_disconnect",
        ),
        (
            "postgres_unlink",
            json!({ "name": "orders", "consumer": "web" }),
            "connect a service to it with service_connect, and take the link away with service_disconnect",
        ),
        (
            "postgres_delete",
            json!({ "name": "orders" }),
            "remove it with service_remove",
        ),
    ] {
        let (code, message) = refusal(rig.call(verb, args));
        assert_eq!(code, "refused", "{verb}");
        assert_eq!(
            message,
            format!(
                "orders is a service: a database made from this node's postgres-autopg template is one. {verb} acts on a database of the old kind only; {instead}"
            )
        );
    }
    assert_eq!(rig.entries(), before, "a refused verb writes nothing");
    assert!(password_file(&rig, "orders").exists());
}

#[test]
fn a_template_changed_between_the_card_and_the_approval_refuses_the_create() {
    let rig = clean();
    let shown = pin(&rig);
    let proposal = propose(&rig, json!({ "name": "orders" })).expect("the proposal");
    // The template now names another image than the card showed.
    let path = rig
        .dir
        .join("templates")
        .join(format!("{DATABASE_TEMPLATE}.json"));
    edit_database_template(&rig, |template| {
        template["digest"] = json!(format!("sha256:{}", "d".repeat(64)));
    });
    let now = pin(&rig);
    assert_ne!(now, shown);
    let writes = rig.fake().writes().len();
    let (code, message) = refusal(approve(&rig, &proposal));
    assert_eq!(code, "refused");
    assert_eq!(
        message,
        format!(
            "image: a call cannot choose an image. This node makes a database from its postgres-autopg template, which names {now} now. A proposal that names another was made before the template changed: nothing was created, propose it again"
        )
    );
    assert!(
        !message.contains(&shown),
        "the refusal never repeats what was stated"
    );
    assert!(!has_entry(&rig, "service-orders"));
    assert!(!password_file(&rig, "orders").exists());
    assert_eq!(rig.fake().writes().len(), writes, "Incus was not written");
    // A template that is gone refuses too, and says what puts it back.
    let proposal = propose(&rig, json!({ "name": "orders" })).expect("a new proposal");
    fs::remove_file(&path).expect("the template is removed");
    let (code, message) = refusal(approve(&rig, &proposal));
    assert_eq!(code, "refused");
    assert_eq!(
        message,
        "this node has no postgres-autopg template (templates/postgres-autopg.json in its state directory), and its config names no kernel.postgresImage: a database is made from one of the two. `arca templates install` puts the template Arca ships there"
    );
    // Without the template nothing is proposed either: the owner is never shown a card for it.
    let (_, again) = refusal(propose(&rig, json!({ "name": "orders" })));
    assert_eq!(again, message);
    assert!(!has_entry(&rig, "service-orders"));
    assert!(!password_file(&rig, "orders").exists());
}

#[test]
fn no_call_chooses_the_image_of_a_database_through_either_door() {
    // A clean node: the image is the template's.
    let rig = clean();
    let pin = pin(&rig);
    let writes = rig.fake().writes().len();
    let entries = rig.entries();
    let chosen = format!(
        "image: a call cannot choose an image. This node makes a database from its postgres-autopg template, which names {pin} now. A proposal that names another was made before the template changed: nothing was created, propose it again"
    );
    for door in ["direct", "proposed"] {
        let call = |args: Value| match door {
            "direct" => rig.call("postgres_create", args),
            _ => propose(&rig, args),
        };
        let (code, message) = refusal(call(json!({ "name": "mine", "image": OTHER_IMAGE })));
        assert_eq!(code, "refused", "{door}");
        assert_eq!(message, chosen, "{door}");
        assert!(!message.contains(OTHER_IMAGE), "{door}");
        // A proposal made by hand, with the template and another image than it pins, is refused when it is
        // proposed: the owner never sees a card that cannot run.
        let (code, message) = refusal(call(
            json!({ "name": "mine", "template": DATABASE_TEMPLATE, "image": OTHER_IMAGE }),
        ));
        assert_eq!(code, "refused", "{door}");
        assert_eq!(message, chosen, "{door}");
        // Another template is not the database template.
        let (code, message) = refusal(call(json!({ "name": "mine", "template": "app" })));
        assert_eq!(code, "refused", "{door}");
        assert_eq!(
            message,
            "template: a database is made from this node's postgres-autopg template, and from no other",
            "{door}"
        );
        // An image that is no digest is an argument error, and is not repeated.
        match call(json!({ "name": "mine", "image": "postgres:latest" })) {
            Err(CallError::Invalid(message)) => assert!(!message.contains("latest"), "{door}"),
            other => panic!("{door}: expected an argument error, got {other:?}"),
        }
    }
    assert_eq!(rig.entries(), entries, "a refused call records nothing");
    assert_eq!(rig.fake().writes().len(), writes, "Incus was not written");
    assert!(!password_file(&rig, "mine").exists());
    assert_eq!(
        rig.call("plan_list", json!({})).expect("plans")["plans"],
        json!([]),
        "no refused proposal waits"
    );
    // A doctor token's own create names no image and makes the template's service: what its own service_create
    // of that template makes, and nothing more. Stating exactly the node's image changes nothing.
    let made = rig
        .call("postgres_create", json!({ "name": "one" }))
        .expect("the direct create");
    assert_eq!(made["service"], true);
    assert_eq!(rig.entry("service-one")["source"], fetched_source(&rig));
    rig.call(
        "postgres_create",
        json!({ "name": "two", "template": DATABASE_TEMPLATE, "image": pin }),
    )
    .expect("the node's own image, stated");
    assert_eq!(rig.entry("service-two")["source"], fetched_source(&rig));

    // A node whose config names the image: that image and no other, through both doors.
    let rig = configured();
    let entries = rig.entries();
    let (code, message) = refusal(rig.call(
        "postgres_create",
        json!({ "name": "mine", "image": OTHER_IMAGE }),
    ));
    assert_eq!(code, "refused");
    assert_eq!(
        message,
        "image: a call cannot choose an image. This node makes a database from the image its config names (kernel.postgresImage), and from no other"
    );
    let (_, message) = refusal(rig.call(
        "postgres_create",
        json!({ "name": "mine", "template": DATABASE_TEMPLATE }),
    ));
    assert_eq!(
        message,
        "template: this node's config names kernel.postgresImage, so a database is made from that image and from no template"
    );
    assert!(matches!(
        propose(&rig, json!({ "name": "mine", "image": OTHER_IMAGE })),
        Err(CallError::Invalid(_))
    ));
    assert!(matches!(
        propose(
            &rig,
            json!({ "name": "mine", "template": DATABASE_TEMPLATE })
        ),
        Err(CallError::Invalid(_))
    ));
    assert_eq!(rig.entries(), entries);
    assert!(!rig.dir.join("secrets").join("postgres-mine").exists());
    // With no image stated the configured one is used; stating it changes nothing.
    rig.call("postgres_create", json!({ "name": "one" }))
        .expect("the configured image");
    rig.call(
        "postgres_create",
        json!({ "name": "two", "image": PG_IMAGE }),
    )
    .expect("the configured image, stated");
    for name in ["one", "two"] {
        assert_eq!(rig.entry(&format!("postgres-{name}"))["image"], PG_IMAGE);
        assert!(!has_entry(&rig, &format!("service-{name}")));
    }
}

#[test]
fn a_node_whose_config_names_the_image_keeps_its_databases_and_the_old_way_as_they_were() {
    let rig = configured();
    let proposal = propose(&rig, json!({ "name": "orders" })).expect("the proposal");
    // The card and the proposal are the ones they always were: no template, no new row.
    assert_eq!(
        proposal["args"],
        json!({ "name": "orders", "image": PG_IMAGE })
    );
    assert_eq!(
        proposal["details"],
        json!([
            { "label": "Database", "value": "orders" },
            { "label": "Image", "value": PG_IMAGE }
        ])
    );
    let made = approve(&rig, &proposal).expect("the approved create");
    assert_eq!(
        made,
        json!({ "name": "orders", "entry": "postgres-orders", "status": "pending" })
    );
    // The record is, byte for byte, the one every release wrote, and its instance is made from an image Incus holds
    // already, by fingerprint.
    let entry = rig
        .entries()
        .into_iter()
        .find(|entry| entry.id.0 == "postgres-orders")
        .expect("its entry");
    assert_eq!(entry.plugin.0, "arca.postgres");
    assert_eq!(
        serde_json::to_string(&entry.config).expect("json"),
        format!(
            r#"{{"allow":[],"bridge":"arcak-svcbr","image":"{PG_IMAGE}","name":"orders","pool":"pool1","project":"arcak-svc"}}"#
        )
    );
    let live = instance(&rig, "orders").expect("its instance");
    assert_eq!(
        live["config"]["environment.POSTGRES_PASSWORD"],
        fs::read_to_string(rig.dir.join("secrets/postgres-orders/password")).expect("its password")
    );
    assert!(live["config"].get("oci.entrypoint").is_none());
    let rows = databases(&rig);
    assert_eq!(rows.len(), 1);
    let keys: Vec<&str> = rows[0]
        .as_object()
        .expect("a row")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["address", "entry", "links", "name", "port", "status"],
        "a database of the old kind is shown as it always was"
    );
    assert!(!has_entry(&rig, "service-orders"));
    // A restart, as an upgrade is: every record, its digest and the list are the same, and nothing waits.
    let entries = rig.entries();
    let acl = rig.get("/1.0/network-acls/postgres-orders?project=default");
    let rig = rig.restart();
    assert_eq!(rig.entries(), entries);
    assert_eq!(databases(&rig), rows);
    assert_eq!(
        rig.get("/1.0/network-acls/postgres-orders?project=default"),
        acl
    );
    assert_eq!(
        rig.call("plan_list", json!({})).expect("plans")["plans"],
        json!([])
    );
    // A service made from the database template on such a node is listed too, and it alone carries the key.
    rig.call(
        "service_create",
        json!({ "name": "side", "template": DATABASE_TEMPLATE }),
    )
    .expect("a template database, by service_create");
    let rows = databases(&rig);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].get("service").is_none());
    assert_eq!(rows[1]["name"], "side");
    assert_eq!(rows[1]["service"], true);
}

#[test]
fn a_database_takes_a_name_nothing_holds_and_a_failed_create_leaves_no_password() {
    let rig = clean();
    rig.call(
        "service_create",
        json!({ "name": "web", "template": "app" }),
    )
    .expect("a service");
    rig.call("postgres_create", json!({ "name": "orders" }))
        .expect("a database");
    let password = fs::read_to_string(password_file(&rig, "orders")).expect("its password");
    let entries = rig.entries();
    for name in ["web", "orders"] {
        for door in ["direct", "proposed"] {
            let result = match door {
                "direct" => rig.call("postgres_create", json!({ "name": name })),
                _ => propose(&rig, json!({ "name": name })),
            };
            let (code, message) = refusal(result);
            assert_eq!(code, "refused", "{name} {door}");
            assert_eq!(
                message,
                format!(
                    "a service named {name} exists already: a database made from this node's postgres-autopg template is a service, and takes a name no service holds"
                ),
                "{name} {door}"
            );
        }
    }
    assert_eq!(rig.entries(), entries);
    assert!(
        !password_file(&rig, "web").exists(),
        "no password is made for a service that is no database"
    );
    assert_eq!(
        fs::read_to_string(password_file(&rig, "orders")).expect("its password"),
        password,
        "the database's own password is left as it was"
    );
}

#[test]
fn a_template_that_generates_no_password_makes_a_stopped_database_and_the_card_says_so() {
    // A node that still holds a database template from before the shipped one generated its password, or the
    // owner's own edit: `arca templates install` keeps a file that differs.
    let rig = clean();
    edit_database_template(&rig, |template| {
        template
            .as_object_mut()
            .expect("a template")
            .remove("generate");
    });
    let pin = pin(&rig);
    let why = "its template needs the secret POSTGRES_PASSWORD, which is not set yet";
    // The card claims no generated password, and carries the stopped row a create's card has.
    let proposal = propose(&rig, json!({ "name": "orders" })).expect("the proposal");
    assert_eq!(
        proposal["details"],
        json!([
            { "label": "Database", "value": "orders" },
            { "label": "Image", "value": pin },
            { "label": "Template", "value": DATABASE_TEMPLATE },
            { "label": "Made as", "value": MADE_AS_PLAIN },
            { "label": "Start", "value": format!("No: it is created stopped because {why}; it can be started later") },
            { "label": "Template content", "value": content_row(&content(&rig)) }
        ])
    );
    let made = approve(&rig, &proposal).expect("the approved create");
    assert_eq!(
        made,
        json!({
            "name": "orders", "entry": "service-orders", "status": "pending", "service": true,
            "stoppedBecause": why
        })
    );
    assert_eq!(rig.entry("service-orders")["start"], false);
    assert!(
        !password_file(&rig, "orders").exists(),
        "Arca invents no password the template does not ask it to generate"
    );
    // A direct call says the same.
    let direct = rig
        .call("postgres_create", json!({ "name": "bills" }))
        .expect("the direct create");
    assert_eq!(direct["stoppedBecause"], why);
}

fn waiting(rig: &Rig) -> Vec<Value> {
    rig.call("plan_list", json!({})).expect("plans")["plans"]
        .as_array()
        .expect("plans")
        .clone()
}

#[test]
fn a_waiting_databases_card_says_the_same_after_the_node_restarts() {
    // What the node read from its template when the database was proposed is not in the proposal's arguments. A
    // restart inside the proposal's fifteen minutes keeps it: the stopped row above all, so the owner never approves
    // a card that lost it.
    let rig = clean();
    edit_database_template(&rig, |template| {
        template
            .as_object_mut()
            .expect("a template")
            .remove("generate");
    });
    let stopped = propose(&rig, json!({ "name": "orders" })).expect("the proposal");
    assert_eq!(
        stopped["details"][4],
        json!({
            "label": "Start",
            "value": "No: it is created stopped because its template needs the secret POSTGRES_PASSWORD, which is not set yet; it can be started later"
        })
    );
    edit_database_template(&rig, |template| {
        template["generate"] = json!({ "POSTGRES_PASSWORD": "" });
    });
    let generated = propose(&rig, json!({ "name": "bills" })).expect("the proposal");
    assert_eq!(generated["details"][3]["value"], MADE_AS);
    let before = waiting(&rig);
    assert_eq!(before.len(), 2);
    let rig = rig.restart();
    let after = waiting(&rig);
    assert_eq!(after, before, "both cards are the ones the owner was shown");
    for (proposal, shown) in [("orders", &stopped), ("bills", &generated)] {
        let kept = after
            .iter()
            .find(|plan| plan["args"]["name"] == proposal)
            .unwrap_or_else(|| panic!("{proposal} still waits"));
        assert_eq!(kept["details"], shown["details"], "{proposal}");
        assert_eq!(kept["argsDigest"], shown["argsDigest"], "{proposal}");
    }
    // A proposal that waited through the restart is still approved and run.
    let made = approve(&rig, &generated).expect("the approved create");
    assert_eq!(made["name"], "bills");
    assert!(made.get("stoppedBecause").is_none());
}

#[test]
fn a_card_does_not_say_generated_when_a_password_is_already_stored_for_the_name() {
    // The create uses a password that is stored for the name and generates none, so the card claims nothing.
    let rig = clean();
    let file = password_file(&rig, "orders");
    fs::create_dir_all(file.parent().expect("its directory")).expect("the secrets directory");
    fs::write(&file, "a-password-stored-before").expect("a stored password");
    let proposal = propose(&rig, json!({ "name": "orders" })).expect("the proposal");
    assert_eq!(
        proposal["details"],
        json!([
            { "label": "Database", "value": "orders" },
            { "label": "Image", "value": pin(&rig) },
            { "label": "Template", "value": DATABASE_TEMPLATE },
            { "label": "Made as", "value": MADE_AS_PLAIN },
            { "label": "Template content", "value": content_row(&content(&rig)) }
        ])
    );
    // A name with no stored password still gets the claim: the template generates it.
    let other = propose(&rig, json!({ "name": "bills" })).expect("the proposal");
    assert_eq!(other["details"][3]["value"], MADE_AS);
}

#[test]
fn a_token_whose_tool_list_leaves_out_service_create_makes_no_database_from_the_template() {
    let rig = clean();
    let entries = rig.entries();
    let (code, message) = refusal(rig.call_as(
        "postgres_create",
        json!({ "name": "orders" }),
        Some(&["postgres_create", "postgres_list"]),
    ));
    assert_eq!(code, "refused");
    assert_eq!(
        message,
        "on this node a database is a service made from the postgres-autopg template, so postgres_create needs service_create too, and this token's tool list does not have it"
    );
    assert_eq!(rig.entries(), entries);
    assert!(!password_file(&rig, "orders").exists());
    // With both tools in its list the token makes it.
    rig.call_as(
        "postgres_create",
        json!({ "name": "orders" }),
        Some(&["postgres_create", "service_create"]),
    )
    .expect("a token with both tools");
    // On a node whose config names the image a database is no service, and the list needs postgres_create alone.
    let rig = configured();
    rig.call_as(
        "postgres_create",
        json!({ "name": "orders" }),
        Some(&["postgres_create"]),
    )
    .expect("the old kind needs no service_create");
}

#[test]
fn what_a_database_was_checked_against_and_what_it_is_created_from_are_one_read_of_the_template() {
    // The service handler is told the image and the content the database's create was checked against, and compares
    // both with the template it reads for the create: a template that is another by then makes nothing.
    let rig = clean();
    let image = pin(&rig);
    let entries = rig.entries();
    for stale in [
        json!({ "name": "orders", "template": DATABASE_TEMPLATE, "image": image, "templateDigest": "e".repeat(64) }),
        json!({
            "name": "orders", "template": DATABASE_TEMPLATE,
            "image": format!("ghcr.io/automagik-si/autopg@sha256:{}", "e".repeat(64)),
            "templateDigest": content(&rig)
        }),
    ] {
        let (code, message) = refusal(rig.call("service_create", stale));
        assert_eq!(code, "refused");
        assert_eq!(
            message,
            "the template postgres-autopg on this node is not the one this create names: it was changed or installed again since the card was shown, or the call states an image or a template content of its own. Nothing was created; propose it again"
        );
    }
    assert_eq!(rig.entries(), entries);
    assert!(!password_file(&rig, "orders").exists());
}

#[test]
fn a_create_the_service_handler_refuses_leaves_no_password_behind() {
    // An instance of that name that is not Arca's stands in the project: the service handler refuses the create,
    // and no password is left.
    let mut state = empty_state();
    state.instances = vec![json!({
        "name": "stray", "project": PROJECT, "status": "Running", "type": "container",
        "config": {}, "devices": {}
    })];
    let rig = Rig::with_kernel(kernel(None), state);
    install_templates(&rig);
    let (code, message) = refusal(rig.call("postgres_create", json!({ "name": "stray" })));
    assert_eq!(code, "refused");
    assert_eq!(
        message,
        "an instance named stray already exists that is not a service of Arca's; it is left alone"
    );
    assert!(!has_entry(&rig, "service-stray"));
    assert!(!password_file(&rig, "stray").exists());
    assert!(!rig.dir.join("secrets").join("service-stray").exists());
}

#[test]
fn a_node_whose_config_sets_up_no_services_refuses_a_database_in_those_words() {
    // The templates are there, the service subnet is not: the refusal is about the node, not about a template.
    let rig = Rig::with_kernel(
        json!({ "projects": [PROJECT], "droiddProjects": ["other"] }),
        empty_state(),
    );
    install_templates(&rig);
    for result in [
        rig.call("postgres_create", json!({ "name": "orders" })),
        propose(&rig, json!({ "name": "orders" })),
    ] {
        let (code, message) = refusal(result);
        assert_eq!(code, "refused");
        assert!(
            message.starts_with(
                "no service project: the node's config does not set kernel.serviceCidr."
            ),
            "{message}"
        );
    }
    assert!(!password_file(&rig, "orders").exists());
    assert!(!has_entry(&rig, "service-orders"));
}
