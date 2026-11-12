//! `init --enable-changes --config <file>` and `init --watch-only --config <file>`: one step from a node that only looks to
//! a node that may change things, and one step back.
//!
//! A node may change things when its config names the project it writes in and the network its services join. Every
//! change still waits for the owner: an assistant proposes, and nothing runs until the owner approves it with a passkey on
//! the approval page. `init --enable-changes` writes the keys that are missing and changes nothing that is set:
//!
//! - `kernel.projects`: `["arcak-svc"]`, the one project Arca's services live in;
//! - `kernel.droiddProjects`: `["default"]` where the list is empty. Arca never writes in `default`; the entry only names
//!   it as not Arca's, which the config requires of a node that names a project of its own;
//! - `kernel.serviceCidr`: a /24 that nothing on the machine uses, read from the machine and never guessed;
//! - `approvalOrigin`: `http://localhost:<port>` on the loopback door the config already opens, and only where the config
//!   names no approval page. A page that is set, of either kind, is kept as it is.
//!
//! It opens no door: where the config has no loopback door and no approval page, it stops and says so. It asks before it
//! writes, keeps a copy of the config as it was, installs the shipped templates, restarts the installer's own service and
//! no other, and ends with what Arca may now touch, the page, and the one command that goes back.
//!
//! `init --watch-only` empties `kernel.projects` and nothing else. Nothing in Incus is removed and no passkey is touched.
//!
//! Both write a node's config, so both take `--config` as a flag and read no default. Neither runs as root, and neither
//! runs another program but `systemctl --user`, to restart the installer's unit.

use std::fs;
use std::io::{BufRead, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use arca_incus::{IncusRead, UnixIncus};
use serde_json::{Value, json};

use crate::cmd_call::{Addressed, read_token, run_call};
use crate::config::{self, NodeConfig};
use crate::{Chosen, Failure, Flags, Origin};

/// The project Arca's services live in, and the bridge they join.
const PROJECT: &str = arca_netproject::PROJECT;
/// The Incus project every machine has, which Arca never writes in.
const DEFAULT_PROJECT: &str = "default";
/// The first line of a unit the installer wrote.
const UNIT_MARK: &str = "# Automagik Arca: written by scripts/install.sh.";
/// The installer's unit.
const UNIT: &str = "arca.service";
/// The kernel's IPv4 routing table and its table of the machine's own addresses.
const ROUTES: &str = "/proc/net/route";
const FIB_TRIE: &str = "/proc/net/fib_trie";
/// How long a restarted node has to answer with its new mode.
const RESTART_WAIT: Duration = Duration::from_secs(30);

pub fn init(flags: &Flags) -> Result<Option<Value>, Failure> {
    let (enable, back) = (flags.switch("enable-changes"), flags.switch("watch-only"));
    if enable == back {
        return Err(Failure::Usage(
            "init takes one of --enable-changes (let this node make changes, each after the owner's approval) and --watch-only (back to a node that only looks)"
                .to_string(),
        ));
    }
    if back && (flags.value("service-cidr").is_some() || flags.switch("yes")) {
        return Err(Failure::Usage(
            "--service-cidr and --yes belong to --enable-changes".to_string(),
        ));
    }
    let Some(path) = flags.value("config").map(PathBuf::from) else {
        return Err(Failure::Usage(
            "--config is required: init writes a node's config, so it reads no default config"
                .to_string(),
        ));
    };
    if arca_approval::effective_uid().is_ok_and(|uid| uid == 0) {
        return Err(Failure::Refused(
            "this is root, and Arca's node belongs to an ordinary user: run init as the user who runs the node, without sudo"
                .to_string(),
        ));
    }
    // Init replaces the config by rename. A config that is a link would be replaced by a file while the link's target,
    // which the node may be reading, stayed as it was.
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(Failure::Refused(format!(
            "init changed nothing: {} is a link. Name the file it points to with --config, so init writes the file the node reads",
            path.display()
        )));
    }
    let cfg = config::load(&path)?;
    let text = fs::read_to_string(&path).map_err(|error| {
        Failure::Runtime(format!(
            "the config {} cannot be read ({})",
            path.display(),
            error.kind()
        ))
    })?;
    let document: Value = serde_json::from_str(&text)
        .map_err(|_| Failure::Config("the config is not JSON".to_string()))?;
    let node = Node {
        path,
        cfg,
        document,
        text,
        flags,
    };
    if enable {
        node.enable_changes()
    } else {
        node.watch_only()
    }
}

/// One node's config, as the file holds it and as `serve` reads it.
struct Node<'a> {
    path: PathBuf,
    cfg: NodeConfig,
    document: Value,
    text: String,
    flags: &'a Flags,
}

impl Node<'_> {
    fn enable_changes(self) -> Result<Option<Value>, Failure> {
        let kernel = &self.cfg.kernel;
        // What init does not understand, it does not touch: each of these is a node set up by hand for something else.
        let stop = |why: &str| {
            Err(Failure::Refused(format!(
                "init changed nothing: {why}. The operating guide's section \"The config file\" says what each key may hold"
            )))
        };
        if kernel.scratch {
            return stop("kernel.scratch is true, and a scratch node takes only scratch projects");
        }
        if kernel.bridged.is_some() {
            return stop(
                "kernel.bridged is set: this is a bridged node, which runs virtual machines on host bridges and is set up by hand",
            );
        }
        if !kernel.adopt.is_empty() {
            return stop("kernel.adopt is set: such a config is for arca adopt alone");
        }
        if !kernel.projects.is_empty() && !kernel.projects.iter().any(|name| name == PROJECT) {
            return stop(&format!(
                "kernel.projects names projects and {PROJECT} is not among them; add it by hand beside the others"
            ));
        }
        if kernel.droidd_projects.iter().any(|name| name == PROJECT) {
            return stop(&format!(
                "kernel.droiddProjects lists {PROJECT}, which says the project is not Arca's"
            ));
        }

        // The page that approves: the one the config names, kept as it is, or the page of this machine alone on the
        // loopback door the config already opens. Init opens no door: the owner may have closed it.
        let (origin, page_is_new) = match &self.cfg.approval_origin {
            Some(origin) => (origin.clone(), false),
            None => match self.cfg.listen.loopback {
                // The page's origin is the name localhost, which stands for these two addresses and no other.
                Some(door)
                    if door.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST)
                        || door.ip() == IpAddr::V6(Ipv6Addr::LOCALHOST) =>
                {
                    (format!("http://localhost:{}", door.port()), true)
                }
                Some(_) => {
                    return Err(Failure::Refused(
                        "init changed nothing: the loopback door is not on 127.0.0.1 or [::1], the addresses the name localhost stands for, so it cannot carry the approval page. Set approvalOrigin to an https:// page of your own, or move the door"
                            .to_string(),
                    ));
                }
                None => {
                    return Err(Failure::Refused(format!(
                        "init changed nothing: this config has no approval page and no loopback door for one, and without a page nothing could ever be approved. Init opens no door: you may have closed it. Either open it (sh install.sh --door, or \"loopback\": \"127.0.0.1:7471\" in \"listen\" of {}) and run init again, or set approvalOrigin to an https:// page of your own",
                        self.path.display()
                    )));
                }
            },
        };

        // The machine: Incus answers, and no object on it carries another manager's marks.
        let incus = UnixIncus::new(self.cfg.incus.socket.clone());
        if incus.get("/1.0").is_err() {
            return Err(Failure::Runtime(format!(
                "init changed nothing: Incus does not answer on {}. This user must be able to open that socket (on most machines through the group that owns it, which is the administrator's to grant)",
                self.cfg.incus.socket.display()
            )));
        }
        let others = other_managers(&incus)?;
        if !others.is_empty() {
            return Err(Failure::Refused(format!(
                "init changed nothing: this machine holds objects with another manager's marks ({}). List that manager's projects in kernel.droiddProjects of {} by hand, so Arca stays out of them; init fills that key only on a machine that has none",
                others.join(", "),
                self.path.display()
            )));
        }

        // The service network: the one the config names, or a /24 nothing on the machine uses.
        let network = match &kernel.service_cidr {
            Some(cidr) => Network::Kept(cidr.clone()),
            None => self.free_network(&incus)?,
        };

        let adds_project = kernel.projects.is_empty();
        let adds_default = kernel.droidd_projects.is_empty();
        let nothing_to_write =
            !adds_project && !adds_default && !page_is_new && matches!(network, Network::Kept(_));
        let cidr = network.cidr().to_string();

        say("This will let Arca make changes on this machine.");
        say("");
        say(&format!("  config    {}", self.path.display()));
        if nothing_to_write {
            say("            it already says all of it: nothing is written");
        } else {
            if adds_project {
                say(&format!(
                    "            kernel.projects: [\"{PROJECT}\"], the one Incus project Arca's services live in"
                ));
            }
            if adds_default {
                say(&format!(
                    "            kernel.droiddProjects: [\"{DEFAULT_PROJECT}\"]. Arca never writes in \"{DEFAULT_PROJECT}\"; the entry only names it as not Arca's"
                ));
            }
            match &network {
                Network::Kept(_) => {}
                Network::Picked { sources, .. } => {
                    say(&format!(
                        "            kernel.serviceCidr: {cidr}, for the bridge {} Arca makes for its services",
                        arca_netproject::BRIDGE
                    ));
                    say(&format!(
                        "            (nothing on this machine uses it: {})",
                        sources.join("; ")
                    ));
                }
                Network::Named { sources, .. } => {
                    say(&format!(
                        "            kernel.serviceCidr: {cidr}, the one --service-cidr names"
                    ));
                    say(&format!(
                        "            (nothing on this machine uses it: {})",
                        sources.join("; ")
                    ));
                }
            }
            if page_is_new {
                say(&format!(
                    "            approvalOrigin: {origin}, the approval page on this machine's own loopback door"
                ));
            }
        }
        if !page_is_new {
            say(&format!(
                "  page      {origin}/approve, the page this config already names: it is kept as it is"
            ));
        }
        say("");
        let direct = self.direct_tokens();
        say("Arca will then be able to create and change what is in its own project.");
        say(
            "What an assistant proposes waits for you: nothing of it runs until you approve it with your passkey on the",
        );
        say("approval page.");
        for line in direct.words(&self.path, self.cfg.kernel.tenants.is_some()) {
            say(&line);
        }
        say("");

        // --yes answers the two questions for the node the installer leaves, whose one token only looks and proposes. It
        // does not answer for a token that would change things with no card and no passkey: that takes a person.
        if !nothing_to_write && self.flags.switch("yes") && !direct.none() {
            return Err(Failure::Refused(format!(
                "init changed nothing: --yes does not answer for {}, which would then change things without your approval. Run it again without --yes and answer yourself, or take such tokens away first (arca token revoke <id> --config {})",
                match direct {
                    DirectTokens::Read(_) => "the tokens named above",
                    DirectTokens::Unread(_) => "tokens it could not read",
                },
                self.path.display()
            )));
        }
        if !nothing_to_write {
            if !self.flags.switch("yes") {
                if !ask(direct.question(), false) {
                    return Err(Failure::Refused(
                        "init changed nothing: the answer was not yes. To answer on the command line, pass --yes"
                            .to_string(),
                    ));
                }
                if matches!(network, Network::Picked { .. })
                    && !ask(
                        &format!("Service network {cidr} is free. Use it? [Y/n] "),
                        true,
                    )
                {
                    return Err(Failure::Refused(
                        "init changed nothing: name the service network with --service-cidr <a.b.c.0/24> and run it again"
                            .to_string(),
                    ));
                }
            }
            let mut document = self.document.clone();
            if adds_project {
                document["kernel"]["projects"] = json!([PROJECT]);
            }
            if adds_default {
                document["kernel"]["droiddProjects"] = json!([DEFAULT_PROJECT]);
            }
            if !matches!(network, Network::Kept(_)) {
                document["kernel"]["serviceCidr"] = json!(cidr);
            }
            if page_is_new {
                document["approvalOrigin"] = json!(origin);
            }
            self.write(&document)?;
        }

        let templates =
            crate::cmd_templates::install_into(&self.cfg.state_dir.join("templates"), false)?;
        let installed = templates["installed"].as_array().map_or(0, Vec::len);
        let restarted = self.restart("kernel");

        if matches!(restarted, Restart::Answered(_)) {
            say("Arca may now make changes on this machine.");
        } else {
            say(
                "The config now lets Arca make changes on this machine. The node takes that up when it next starts.",
            );
        }
        say("");
        say(&format!(
            "  It may touch: the Incus project {PROJECT}, and the bridge {} on {cidr}.",
            arca_netproject::BRIDGE
        ));
        say(
            "  And, each only from a card you approve: snapshots (the pools they are kept in and the project",
        );
        say("  arcak-snapshots) and the GPU devices of its own instances. Nothing else.");
        say(&format!(
            "  Arca never writes in \"{DEFAULT_PROJECT}\"; the entry in kernel.droiddProjects only names it as not Arca's."
        ));
        say(
            "  What an assistant proposes waits for you: nothing of it runs until you approve it with your",
        );
        say(&format!("  passkey on the page: {origin}/approve"));
        for line in direct.words(&self.path, self.cfg.kernel.tenants.is_some()) {
            say(&format!("  {line}"));
        }
        if self.passkey_is_trusted() {
            say("  A passkey is already trusted on this node: it approves.");
        } else {
            say(
                "  No passkey is trusted yet, so nothing can be approved until you enrol the first one. Open the page and",
            );
            say("  press \"Get a setup code\"; on a machine with no screen, this prints the code:");
            say(&format!(
                "    arca setup-code --config {}",
                self.path.display()
            ));
        }
        if arca_approval::localhost_origin_port(&origin).is_some() {
            say(
                "  That page is for a machine where every account is trusted. Before you type a setup code or approve for",
            );
            say(
                "  the first time, check that the node says its page is open (\"page\": { \"open\": true } under \"loopback\"):",
            );
            say(&format!(
                "    arca status --json --config {}",
                self.path.display()
            ));
        }
        if installed > 0 {
            say(&format!(
                "  The shipped templates are in {} ({installed} new).",
                self.cfg.state_dir.join("templates").display()
            ));
        }
        self.say_restart(&restarted);
        if !nothing_to_write {
            say(&format!(
                "  The config as it was is kept at {}.",
                self.backup().display()
            ));
        }
        say("");
        say("To turn it back to a node that only looks:");
        say(&format!(
            "  arca init --watch-only --config {}",
            self.path.display()
        ));
        Ok(None)
    }

    fn watch_only(self) -> Result<Option<Value>, Failure> {
        let projects = &self.cfg.kernel.projects;
        if projects.is_empty() {
            say(
                "This node already only looks: its config names no kernel project. Nothing was written.",
            );
            return Ok(None);
        }
        if projects.iter().any(|name| name != PROJECT) {
            return Err(Failure::Refused(format!(
                "init changed nothing: kernel.projects names projects other than {PROJECT}, which init did not write. Empty the list by hand to make the node watch-only"
            )));
        }
        let mut document = self.document.clone();
        document["kernel"]["projects"] = json!([]);
        self.write(&document)?;
        let restarted = self.restart("watch-only");

        // The node's own word, or none: a node that was not restarted, or did not answer, still runs on the config as it
        // was, and may still be changing things.
        if matches!(restarted, Restart::Answered(_)) {
            say("Arca now only looks on this machine: it changes nothing.");
        } else {
            say(
                "The config now says watch-only. The node that is running STILL MAKES CHANGES until it is restarted:",
            );
            say("it reads its config only when it starts.");
        }
        say("");
        say(&format!(
            "  Nothing was removed. The Incus project {PROJECT}, its bridge, the services in it and your passkeys are"
        ));
        say(
            "  as they were, and so is every other key of the config. Arca no longer starts, stops, creates or removes",
        );
        say("  anything, and a proposal that waits can no longer run.");
        say(
            "  One thing reads oddly on such a node: service_list answers \"no service project\", though the services",
        );
        say("  are still there; watch_report lists them.");
        self.say_restart(&restarted);
        say(&format!(
            "  The config as it was is kept at {}.",
            self.backup().display()
        ));
        say("");
        say("To let it make changes again:");
        say(&format!(
            "  arca init --enable-changes --config {}",
            self.path.display()
        ));
        Ok(None)
    }

    /// Where the config as it was is kept.
    fn backup(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".before-init");
        PathBuf::from(name)
    }

    /// Writes `document` as the config, after keeping the file as it was beside it. The new text is what `serve` would
    /// read: it is parsed and checked first, so init never leaves a config the node refuses. Both files are the user's
    /// alone, and the config is replaced in one step.
    fn write(&self, document: &Value) -> Result<(), Failure> {
        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(document).map_err(|_| Failure::Runtime(
                "the config cannot be written as JSON".to_string()
            ))?
        );
        let parsed: NodeConfig = serde_json::from_str(&text).map_err(|_| {
            Failure::Runtime(
                "init would have written a config the node cannot read; nothing was written"
                    .to_string(),
            )
        })?;
        config::check(&parsed).map_err(|error| {
            Failure::Refused(format!(
                "init changed nothing: the config it would write is refused ({error})"
            ))
        })?;
        let failed = |what: &str, error: std::io::Error| {
            Failure::Runtime(format!("{what} ({}); nothing was changed", error.kind()))
        };
        // A new file of the user's own at `path`, which must not exist: no link is followed and no file is truncated,
        // so a name another user planted in the config's directory is refused, never written through.
        let private = |path: &Path, text: &str| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                .mode(0o600)
                .open(path)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()
        };
        // The copy from an earlier run is replaced: a regular file is removed first, and anything else under that
        // name (a link, a directory) is not init's to touch.
        let backup = self.backup();
        match fs::symlink_metadata(&backup) {
            Ok(meta) if meta.is_file() => fs::remove_file(&backup).map_err(|error| {
                failed("the earlier copy of the config cannot be replaced", error)
            })?,
            Ok(_) => {
                return Err(Failure::Refused(format!(
                    "init changed nothing: {} is there and is not a file of init's (a link or a directory). Move it away and run init again",
                    backup.display()
                )));
            }
            Err(_) => {}
        }
        private(&backup, &self.text)
            .map_err(|error| failed("the copy of the config as it was cannot be written", error))?;
        let mut staged = self.path.as_os_str().to_owned();
        staged.push(format!(".init-{}", std::process::id()));
        let staged = PathBuf::from(staged);
        match private(&staged, &text) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(Failure::Runtime(format!(
                    "init changed nothing: {} is in the way, left by a run that was cut short. Remove it and run init again",
                    staged.display()
                )));
            }
            Err(error) => return Err(failed("the config cannot be written", error)),
        }
        fs::rename(&staged, &self.path).map_err(|error| {
            let _ = fs::remove_file(&staged);
            failed("the config cannot be written", error)
        })?;
        // The rename is in the directory: without this sync a power cut could bring the old config back. The config
        // is changed either way, so a sync that fails is said as that, and init goes on.
        let parent = self.path.parent().filter(|dir| !dir.as_os_str().is_empty());
        if let Err(error) =
            fs::File::open(parent.unwrap_or(Path::new("."))).and_then(|dir| dir.sync_all())
        {
            eprintln!(
                "arca: the config was changed. Making that durable failed ({}), so a power cut in the next seconds could bring the config back as it was: check it after one",
                error.kind()
            );
        }
        Ok(())
    }

    /// The tokens of this node that may change things without the approval page once it may change things at all.
    fn direct_tokens(&self) -> DirectTokens {
        match arca_mcp::TokenStore::load(&self.cfg.tokens.hash_dir) {
            Ok(store) => DirectTokens::Read(
                store
                    .ids()
                    .into_iter()
                    .filter(|(_, scope, _)| *scope >= arca_api::Scope::Doctor)
                    .map(|(id, scope, tools)| match tools {
                        Some(tools) => format!(
                            "{id} ({}, limited to: {})",
                            scope.as_str(),
                            tools.join(", ")
                        ),
                        None => format!("{id} ({})", scope.as_str()),
                    })
                    .collect(),
            ),
            Err(_) => DirectTokens::Unread(self.cfg.tokens.hash_dir.clone()),
        }
    }

    /// The /24 for the service network: the one `--service-cidr` names, checked against the machine, or the first of
    /// init's own list that nothing on the machine uses. Every source is read, and each is named in the answer. A
    /// source that cannot be read is never guessed around: the person is asked, and under `--yes` init stops.
    fn free_network(&self, incus: &UnixIncus) -> Result<Network, Failure> {
        let mut used = Used::default();
        let mut unread: Vec<String> = Vec::new();
        match fs::read_to_string(ROUTES) {
            Ok(text) => used.routes(&text),
            Err(_) => unread.push(format!("the routing table {ROUTES}")),
        }
        match fs::read_to_string(FIB_TRIE) {
            Ok(text) => used.addresses(&text),
            Err(_) => unread.push(format!("this machine's own addresses in {FIB_TRIE}")),
        }
        // Incus is asked twice: for the networks it manages, whose config names their range, and for the addresses of
        // every interface it lists, which is the machine's addresses as Incus itself sees them.
        match (
            incus.get("/1.0/networks?recursion=1"),
            arca_providers::incus::host_addresses(incus),
        ) {
            (Ok(networks), Ok(addresses)) => {
                used.incus_networks(&networks);
                used.incus_addresses(&addresses);
            }
            _ => unread
                .push("Incus's networks and the addresses of the interfaces it lists".to_string()),
        }
        used.reserved(&self.cfg.kernel.reserved_cidrs);
        let sources = vec![
            format!("{} route(s) in {ROUTES}", used.route_count),
            format!(
                "{} address(es) of this machine in {FIB_TRIE}",
                used.address_count
            ),
            format!(
                "{} network(s) Incus manages and {} address(es) on the interfaces it lists",
                used.incus_count, used.incus_address_count
            ),
            "nothing inside the tailnet range 100.64.0.0/10".to_string(),
        ];
        unread_sources(
            &unread,
            self.flags.switch("yes"),
            self.flags.value("service-cidr").is_some(),
            &|question| ask(question, false),
        )?;
        match self.flags.value("service-cidr") {
            Some(named) => {
                let Some(network) = Cidr::parse(named).filter(|cidr| cidr.prefix == 24 && cidr.is_network()) else {
                    return Err(Failure::Usage(
                        "--service-cidr takes an IPv4 /24 network, such as 10.47.0.0/24".to_string(),
                    ));
                };
                // The tailnet's range has its own words, which say more than "not private".
                if !network.overlaps(&TAILNET)
                    && !PRIVATE.iter().any(|range| range.contains(network.address))
                {
                    return Err(Failure::Refused(format!(
                        "init changed nothing: {network} is not a private network. A service network is taken from the private ranges of RFC 1918 (10/8, 172.16/12 and the /16 of home routers): any other would hide real addresses from this machine"
                    )));
                }
                if let Some(why) = used.conflict(&network) {
                    return Err(Failure::Refused(format!(
                        "init changed nothing: {network} is not free on this machine: {why}"
                    )));
                }
                Ok(Network::Named {
                    cidr: network.to_string(),
                    sources,
                })
            }
            None => candidates()
                .find(|candidate| used.conflict(candidate).is_none())
                .map(|cidr| Network::Picked {
                    cidr: cidr.to_string(),
                    sources,
                })
                .ok_or_else(|| {
                    Failure::Refused(
                        "init changed nothing: none of the networks it tries (10.47.0.0/24 to 10.47.255.0/24) is free on this machine. Name one with --service-cidr <a.b.c.0/24>"
                            .to_string(),
                    )
                }),
        }
    }

    /// Whether a passkey is already trusted for this node: its own state file holds one, or the root-owned file is there.
    fn passkey_is_trusted(&self) -> bool {
        let own = self.cfg.state_dir.join(arca_approval::STATE_PASSKEYS_FILE);
        arca_approval::load_state_passkeys(&own).is_ok_and(|lines| !lines.is_empty())
            || fs::metadata(arca_approval::OWNER_PASSKEYS).is_ok_and(|meta| meta.len() > 0)
    }

    /// Restarts the installer's own service when this config is the one it runs, and asks the node for its mode. A
    /// node started any other way is the person's to restart: init starts and stops nothing it cannot name.
    fn restart(&self, want: &str) -> Restart {
        let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
            return Restart::NotOurs;
        };
        let unit = Path::new(&home).join(".config/systemd/user").join(UNIT);
        // The unit names its config by its whole path; this command may have been given a shorter one.
        let config = fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        let ours = fs::read_to_string(&unit).is_ok_and(|text| {
            text.starts_with(UNIT_MARK)
                && text.lines().any(|line| {
                    line.strip_prefix("ExecStart=")
                        .and_then(|command| command.split_once(" serve --config "))
                        .is_some_and(|(_, named)| {
                            Path::new(named) == config || Path::new(named) == self.path
                        })
                })
        });
        if !ours {
            return Restart::NotOurs;
        }
        let ran = Command::new("systemctl")
            .args(["--user", "restart", UNIT])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !ran {
            return Restart::Failed;
        }
        // The node's own word for what it now is. Without a token to ask with, the restart is all init can vouch for.
        let chosen = Chosen {
            path: self.path.clone(),
            origin: Origin::Flag,
        };
        // The hints this builds name the node by its config alone: the command that makes a node read its tokens again
        // is `arca token reload --config <path>`, which needs no state directory.
        let addressed = Addressed::new(&chosen, false);
        let Ok(token) =
            crate::token_path(self.flags).and_then(|file| read_token(&file, &addressed))
        else {
            return Restart::Unasked;
        };
        let deadline = Instant::now() + RESTART_WAIT;
        loop {
            let answer = run_call(
                &self.cfg.listen.unix,
                None,
                &token,
                "arca_status",
                json!({}),
                &addressed,
            );
            if let Ok(Some(status)) = answer
                && status["mode"] == json!(want)
            {
                return Restart::Answered(status);
            }
            if Instant::now() >= deadline {
                return Restart::Silent;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn say_restart(&self, restarted: &Restart) {
        let start = format!("arca serve --config {}", self.path.display());
        match restarted {
            Restart::Answered(status) => {
                say(&format!(
                    "  The node was restarted and says: mode {}.",
                    status["mode"].as_str().unwrap_or("not stated")
                ));
                if status["loopback"]["page"]["open"] == json!(false) {
                    say("  Its approval page is CLOSED right now:");
                    say(&format!(
                        "  {}",
                        status["loopback"]["page"]["reason"]
                            .as_str()
                            .unwrap_or("the node does not say why")
                    ));
                }
            }
            Restart::Unasked => say(
                "  The node was restarted. There is no token to ask it with, so ask it yourself: arca status",
            ),
            Restart::Silent => {
                say(
                    "  The node was restarted and has not answered with its new mode yet. Look at:",
                );
                say(&format!("    systemctl --user status {UNIT}"));
            }
            Restart::Failed => {
                say(&format!(
                    "  `systemctl --user restart {UNIT}` failed, so the node still runs on the config as it was. Run it yourself."
                ));
            }
            Restart::NotOurs => {
                say(
                    "  The node reads its config when it starts, and this is not the installer's service, so init did not",
                );
                say("  restart it. Stop the node and start it again for this to take effect:");
                say(&format!("    {start}"));
            }
        }
    }
}

/// The doctor and operator tokens of a node, which do not wait for the approval page.
enum DirectTokens {
    /// The token directory was read: each such token, by its id and scope. Empty when every token only looks and
    /// proposes.
    Read(Vec<String>),
    /// The token directory could not be read, so init cannot say.
    Unread(PathBuf),
}

impl DirectTokens {
    /// What init says of them before it asks and after it wrote: who may change things with no card and no passkey.
    fn words(&self, config: &Path, tenants: bool) -> Vec<String> {
        match self {
            Self::Read(tokens) if tokens.is_empty() => vec![
                "No token of this node may do more than look and propose, so every change waits for your passkey."
                    .to_string(),
            ],
            Self::Read(tokens) => {
                let mut lines = vec![format!(
                    "NOT every change waits for you. This node has {} token(s) that may then change things directly,",
                    tokens.len()
                )];
                lines.push("with no card and no passkey:".to_string());
                lines.extend(tokens.iter().map(|token| format!("    {token}")));
                lines.push(
                    "Whoever holds one may create a service from a template, connect and disconnect services, set a"
                        .to_string(),
                );
                lines.push(
                    "service's secret, unpublish one, start an instance, link and unlink a database, create a database"
                        .to_string(),
                );
                lines.push(
                    "from an image the token names, remove a database's record, password and links, and use the Omni verbs."
                        .to_string(),
                );
                if tenants {
                    lines.push(
                        "This node holds tenants: such a token may also create and remove a tenant.".to_string(),
                    );
                }
                lines.push(format!(
                    "To take one away first: arca token revoke <id> --config {}",
                    config.display()
                ));
                lines
            }
            Self::Unread(dir) => vec![
                format!(
                    "The tokens in {} could not be read, so init cannot say whether one of them may change things",
                    dir.display()
                ),
                "directly, with no card and no passkey: a doctor or an operator token may.".to_string(),
            ],
        }
    }

    /// Whether every token was read and none of them does more than look and propose.
    fn none(&self) -> bool {
        matches!(self, Self::Read(tokens) if tokens.is_empty())
    }

    /// The question, which says the same.
    fn question(&self) -> &'static str {
        match self {
            Self::Read(tokens) if tokens.is_empty() => {
                "Let Arca make changes on this machine, each only after you approve it? [y/N] "
            }
            Self::Read(_) => {
                "Let Arca make changes on this machine: what is proposed after your approval, and directly for the tokens named above? [y/N] "
            }
            Self::Unread(_) => {
                "Let Arca make changes on this machine: what is proposed after your approval, and directly for any doctor or operator token it has? [y/N] "
            }
        }
    }
}

/// How the restart went.
enum Restart {
    /// The installer's service was restarted and the node answered with the mode init wrote.
    Answered(Value),
    /// It was restarted, and there is no token to ask it with.
    Unasked,
    /// It was restarted and did not answer with the new mode in time.
    Silent,
    /// `systemctl` failed.
    Failed,
    /// The node is not the installer's service for this config.
    NotOurs,
}

/// The service network init settled on, and how.
enum Network {
    /// The config names one already.
    Kept(String),
    /// The first of init's list that nothing on the machine uses, and the sources that say so.
    Picked { cidr: String, sources: Vec<String> },
    /// The one `--service-cidr` names, checked the same way.
    Named { cidr: String, sources: Vec<String> },
}

impl Network {
    fn cidr(&self) -> &str {
        match self {
            Self::Kept(cidr) | Self::Picked { cidr, .. } | Self::Named { cidr, .. } => cidr,
        }
    }
}

/// What init does when it could not read every source that says which networks are in use. It never guesses. A network
/// the owner named is the owner's word: it is still checked against what could be read, and init goes on. With none
/// named and `yes` (no person to ask) it stops; otherwise it asks, with no as the default, and stops on anything but a
/// yes. With every source read it goes on.
fn unread_sources(
    unread: &[String],
    yes: bool,
    named: bool,
    ask: &dyn Fn(&str) -> bool,
) -> Result<(), Failure> {
    if unread.is_empty() {
        return Ok(());
    }
    let what = unread.join(" and ");
    if named {
        eprintln!(
            "arca: could not read {what}. The network --service-cidr names is taken on your word, checked against what could be read."
        );
        return Ok(());
    }
    if yes {
        return Err(Failure::Runtime(format!(
            "init changed nothing: it could not read {what}, so it cannot tell which network is free, and it does not guess. Name one you know to be free: --service-cidr <a.b.c.0/24> goes on with what could be read"
        )));
    }
    if ask(&format!(
        "Could not read {what}, so a network that looks free may be in use. Go on with what could be read? [y/N] "
    )) {
        return Ok(());
    }
    Err(Failure::Refused(
        "init changed nothing: name a network you know to be free with --service-cidr <a.b.c.0/24>"
            .to_string(),
    ))
}

/// The instances on the machine that carry another manager's marks, as `project/name`, from one read of the watch.
fn other_managers(incus: &UnixIncus) -> Result<Vec<String>, Failure> {
    let report = crate::handlers::watch_snapshot_scoped(
        incus.clone(),
        &Default::default(),
        &Default::default(),
        crate::handlers::now_ms(),
        true,
    )
    .map_err(|_| {
        Failure::Runtime(
            "init changed nothing: the machine's instances could not be read from Incus, so it cannot tell whether another manager owns any"
                .to_string(),
        )
    })?;
    let mut seen = Vec::new();
    for project in report["projects"].as_array().into_iter().flatten() {
        for instance in project["instances"].as_array().into_iter().flatten() {
            if matches!(
                instance["owner"]["class"].as_str(),
                Some("droidd" | "contested")
            ) {
                seen.push(format!(
                    "{}/{}",
                    project["project"].as_str().unwrap_or("?"),
                    instance["name"].as_str().unwrap_or("?")
                ));
            }
        }
    }
    seen.truncate(8);
    Ok(seen)
}

fn say(line: &str) {
    println!("{line}");
}

/// One yes-or-no question on the terminal. An empty answer, and no answer at all (a standard input that is closed or
/// not a terminal), is the default.
fn ask(question: &str, default: bool) -> bool {
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return default;
    }
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    }
}

/// An IPv4 network: its address and prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cidr {
    address: u32,
    prefix: u32,
}

impl Cidr {
    fn parse(text: &str) -> Option<Cidr> {
        let (address, prefix) = text.split_once('/')?;
        let address = u32::from(address.parse::<Ipv4Addr>().ok()?);
        let prefix = prefix.parse::<u32>().ok().filter(|prefix| *prefix <= 32)?;
        Some(Cidr { address, prefix })
    }

    fn mask(&self) -> u32 {
        u32::MAX.checked_shl(32 - self.prefix).unwrap_or(0)
    }

    /// Whether the address is the network's own first address, with no host bits set.
    fn is_network(&self) -> bool {
        self.address & !self.mask() == 0
    }

    fn contains(&self, address: u32) -> bool {
        address & self.mask() == self.address & self.mask()
    }

    fn overlaps(&self, other: &Cidr) -> bool {
        let mask = self.mask() & other.mask();
        self.address & mask == other.address & mask
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{}",
            Ipv4Addr::from(self.address & self.mask()),
            self.prefix
        )
    }
}

/// The private ranges of RFC 1918. A service network named on the command line must lie in one: a public, multicast
/// or link-local /24 would pass every other check and then hide those addresses from the machine.
const PRIVATE: [Cidr; 3] = [
    Cidr {
        address: 10 << 24,
        prefix: 8,
    },
    Cidr {
        address: (172 << 24) | (16 << 16),
        prefix: 12,
    },
    Cidr {
        address: (192 << 24) | (168 << 16),
        prefix: 16,
    },
];

/// The tailnet's IPv4 range: no service network is ever taken from it, whatever the machine's tables say.
const TAILNET: Cidr = Cidr {
    address: 0x6440_0000,
    prefix: 10,
};

/// The networks init tries, in order: 10.47.0.0/24 to 10.47.255.0/24.
fn candidates() -> impl Iterator<Item = Cidr> {
    (0..=255u32).map(|third| Cidr {
        address: (10 << 24) | (47 << 16) | (third << 8),
        prefix: 24,
    })
}

/// What the machine already uses: its routes, its own addresses, Incus's networks and the config's reserved ranges.
#[derive(Default)]
struct Used {
    networks: Vec<(Cidr, String)>,
    addresses: Vec<u32>,
    route_count: usize,
    address_count: usize,
    incus_count: usize,
    incus_address_count: usize,
}

impl Used {
    /// The kernel's IPv4 routing table: one route a line after the heading, the destination and the mask as eight
    /// hexadecimal digits in the machine's byte order. The default route is every address and says nothing of what is
    /// in use, so it is not counted.
    fn routes(&mut self, table: &str) {
        for line in table.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (Some(interface), Some(destination), Some(mask)) =
                (fields.first(), fields.get(1), fields.get(7))
            else {
                continue;
            };
            let (Ok(destination), Ok(mask)) = (
                u32::from_str_radix(destination, 16),
                u32::from_str_radix(mask, 16),
            ) else {
                continue;
            };
            // The kernel prints each as the number its four network-order bytes make in the machine's own byte order.
            let (destination, mask) = (u32::from_be(destination), u32::from_be(mask));
            if mask == 0 {
                continue;
            }
            self.route_count += 1;
            self.networks.push((
                Cidr {
                    address: destination,
                    prefix: mask.count_ones(),
                },
                format!("a route through {interface}"),
            ));
        }
    }

    /// The kernel's table of local addresses: an address line (`|-- 10.1.2.3`) followed by `/32 host LOCAL` is one of
    /// the machine's own.
    fn addresses(&mut self, trie: &str) {
        let mut last: Option<u32> = None;
        for line in trie.lines() {
            let line = line.trim();
            if let Some(address) = line.strip_prefix("|-- ") {
                last = address.parse::<Ipv4Addr>().ok().map(u32::from);
            } else if line.contains("host LOCAL")
                && let Some(address) = last
                && !self.addresses.contains(&address)
            {
                self.addresses.push(address);
            }
        }
        self.address_count = self.addresses.len();
    }

    /// Incus's networks, as `GET /1.0/networks?recursion=1` lists them: each one's `ipv4.address`, an address with its
    /// prefix, is a network in use.
    fn incus_networks(&mut self, networks: &Value) {
        for network in networks.as_array().into_iter().flatten() {
            let Some(cidr) = network["config"]["ipv4.address"]
                .as_str()
                .and_then(Cidr::parse)
            else {
                continue;
            };
            self.incus_count += 1;
            self.networks.push((
                cidr,
                format!(
                    "Incus's network {}",
                    network["name"].as_str().unwrap_or("with no name")
                ),
            ));
        }
    }

    /// The IPv4 addresses on the interfaces Incus lists: each is an address of this machine.
    fn incus_addresses(&mut self, addresses: &[IpAddr]) {
        for address in addresses {
            if let IpAddr::V4(address) = address {
                self.incus_address_count += 1;
                let address = u32::from(*address);
                if !self.addresses.contains(&address) {
                    self.addresses.push(address);
                }
            }
        }
    }

    fn reserved(&mut self, reserved: &[String]) {
        for cidr in reserved.iter().filter_map(|text| Cidr::parse(text)) {
            self.networks
                .push((cidr, "a range kernel.reservedCidrs names".to_string()));
        }
    }

    /// Why `network` is not free, or `None` when nothing uses it.
    fn conflict(&self, network: &Cidr) -> Option<String> {
        if network.overlaps(&TAILNET) {
            return Some("it is inside the tailnet range 100.64.0.0/10".to_string());
        }
        if let Some((used, what)) = self
            .networks
            .iter()
            .find(|(used, _)| used.overlaps(network))
        {
            return Some(format!("it overlaps {used}, {what}"));
        }
        self.addresses
            .iter()
            .find(|address| network.contains(**address))
            .map(|address| format!("{} is an address of this machine", Ipv4Addr::from(*address)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(text: &str) -> Cidr {
        Cidr::parse(text).expect("a network")
    }

    #[test]
    fn a_network_is_free_only_when_no_route_no_address_no_incus_network_and_no_reserved_range_meets_it()
     {
        let mut used = Used::default();
        // The kernel writes the destination and the mask in the machine's byte order: 192.0.2.0/24 and 10.47.0.0/16.
        used.routes(
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
             eth0\t00000000\t010200C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
             eth0\t000200C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
             br0\t00002F0A\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n",
        );
        assert_eq!(
            used.route_count, 2,
            "the default route is not a network in use"
        );
        assert_eq!(
            used.conflict(&cidr("192.0.2.0/24")).as_deref(),
            Some("it overlaps 192.0.2.0/24, a route through eth0")
        );
        // A /24 inside a wider route is in use, and so is every one of init's own list under 10.47.0.0/16.
        assert!(used.conflict(&cidr("10.47.9.0/24")).is_some());
        assert_eq!(candidates().find(|c| used.conflict(c).is_none()), None);
        assert_eq!(used.conflict(&cidr("10.48.0.0/24")), None);

        // An address of the machine with no route of its own, as a tailnet interface has.
        let mut used = Used::default();
        used.addresses(
            "Main:\n  +-- 0.0.0.0/0 3 0 5\n     |-- 0.0.0.0\n        /0 universe UNICAST\n\
             Local:\n  +-- 10.47.3.0/24 2 0 2\n     |-- 10.47.3.7\n        /32 host LOCAL\n\
             \x20    |-- 10.47.3.255\n        /32 link BROADCAST\n     |-- 127.0.0.1\n        /32 host LOCAL\n",
        );
        assert_eq!(used.address_count, 2);
        assert_eq!(
            used.conflict(&cidr("10.47.3.0/24")).as_deref(),
            Some("10.47.3.7 is an address of this machine")
        );
        assert_eq!(used.conflict(&cidr("10.47.4.0/24")), None);
        assert_eq!(
            candidates().find(|c| used.conflict(c).is_none()),
            Some(cidr("10.47.0.0/24")),
            "the first of the list that is free"
        );

        // Incus's own networks, and the config's reserved ranges.
        let mut used = Used::default();
        used.incus_networks(&json!([
            { "name": "incusbr0", "config": { "ipv4.address": "10.47.0.1/24" } },
            { "name": "plain", "config": { "ipv4.address": "none" } },
            { "name": "bare", "config": {} }
        ]));
        used.reserved(&["10.47.1.0/24".to_string()]);
        assert_eq!(used.incus_count, 1);
        assert_eq!(
            used.conflict(&cidr("10.47.0.0/24")).as_deref(),
            Some("it overlaps 10.47.0.0/24, Incus's network incusbr0")
        );
        assert!(used.conflict(&cidr("10.47.1.0/24")).is_some());
        assert_eq!(
            candidates().find(|c| used.conflict(c).is_none()),
            Some(cidr("10.47.2.0/24"))
        );
    }

    #[test]
    fn a_source_that_cannot_be_read_is_asked_about_or_stops_init_and_is_never_guessed_around() {
        let unread = vec![
            "the routing table /proc/net/route".to_string(),
            "Incus's networks".to_string(),
        ];
        let never = |_: &str| -> bool { panic!("nothing is asked") };
        // Everything was read: nothing to ask, whoever runs it.
        assert!(unread_sources(&[], true, false, &never).is_ok());
        assert!(unread_sources(&[], false, false, &never).is_ok());
        // A network the owner named: taken on the owner's word with or without --yes, and nothing is asked. This is
        // the way on that the stop below names, so it must not stop the same way.
        assert!(unread_sources(&unread, true, true, &never).is_ok());
        assert!(unread_sources(&unread, false, true, &never).is_ok());
        // Nobody to ask: it stops, names what it could not read, and says how to go on.
        let stopped = unread_sources(&unread, true, false, &never).expect_err("it stops");
        assert_eq!(stopped.code(), 1);
        assert!(
            stopped.message().contains(
                "it could not read the routing table /proc/net/route and Incus's networks"
            ),
            "{}",
            stopped.message()
        );
        assert!(stopped.message().contains("it does not guess"));
        assert!(stopped.message().contains("--service-cidr"));
        // A person: asked once, by name of what is missing, and only a yes goes on.
        let asked = std::cell::RefCell::new(Vec::new());
        let answer = |yes: bool| {
            let asked = &asked;
            move |question: &str| {
                asked.borrow_mut().push(question.to_string());
                yes
            }
        };
        assert!(unread_sources(&unread, false, false, &answer(true)).is_ok());
        let refused =
            unread_sources(&unread, false, false, &answer(false)).expect_err("a no stops it");
        assert_eq!(refused.code(), 2);
        let asked = asked.borrow();
        assert_eq!(asked.len(), 2);
        assert!(
            asked[0].starts_with(
                "Could not read the routing table /proc/net/route and Incus's networks"
            )
        );
        assert!(asked[0].ends_with("[y/N] "));
    }

    #[test]
    fn nothing_inside_the_tailnet_range_is_ever_free() {
        let used = Used::default();
        for inside in ["100.64.0.0/24", "100.100.100.0/24", "100.127.255.0/24"] {
            assert_eq!(
                used.conflict(&cidr(inside)).as_deref(),
                Some("it is inside the tailnet range 100.64.0.0/10"),
                "{inside}"
            );
        }
        for outside in ["100.63.255.0/24", "100.128.0.0/24"] {
            assert_eq!(used.conflict(&cidr(outside)), None, "{outside}");
        }
    }

    #[test]
    fn a_network_is_read_and_written_as_its_own_first_address() {
        assert_eq!(cidr("10.47.3.0/24").to_string(), "10.47.3.0/24");
        assert!(cidr("10.47.3.0/24").is_network());
        assert!(!cidr("10.47.3.1/24").is_network());
        assert_eq!(Cidr::parse("10.47.3.0"), None);
        assert_eq!(Cidr::parse("10.47.3.0/33"), None);
        assert_eq!(Cidr::parse("ten/24"), None);
        assert_eq!(candidates().count(), 256);
        assert_eq!(candidates().last(), Some(cidr("10.47.255.0/24")));
    }
}
