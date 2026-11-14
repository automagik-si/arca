//! Closed service commands and file destinations. This power is outside the effect journal and is never exposed by Provider.
//! Every command uses an absolute binary in the system template and an in-instance timeout; Arca adds no environment.

use crate::read::{check_path, checked_envelope, exchange_capped};
use crate::write::operation_id;
use crate::{IncusError, Owned, UnixIncusWriter};
use arca_kernel::ownership::IncusKind;
use serde_json::{Value, json};
use std::time::Duration;

pub const OUTPUT_CAP: usize = 64 * 1024;
pub const SERVICE_PROJECT: &str = "arcak-svc";

fn refused() -> IncusError {
    IncusError::Refused("invalid service exec target or argument".into())
}
fn exec_error(error: IncusError) -> IncusError {
    match error {
        IncusError::Timeout => IncusError::Timeout,
        IncusError::TooLarge => IncusError::TooLarge,
        _ => bad(),
    }
}
fn bad() -> IncusError {
    IncusError::Decode("invalid service exec answer".into())
}

#[derive(Clone, Copy, Debug)]
pub struct JournalLines(u16);
impl JournalLines {
    pub fn new(lines: u64) -> Result<Self, IncusError> {
        if !(1..=200).contains(&lines) {
            return Err(refused());
        }
        Ok(Self(lines as u16))
    }
    pub fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Debug)]
pub struct TailnetNode(String);
impl TailnetNode {
    pub fn new(node: &str) -> Result<Self, IncusError> {
        if !node.starts_with("arca-") || !node_ok(node) {
            return Err(refused());
        }
        Ok(Self(node.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub fn node_ok(node: &str) -> bool {
    (1..=63).contains(&node.len())
        && !node.ends_with('-')
        && node.bytes().next().is_some_and(|c| c.is_ascii_lowercase())
        && node
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

#[derive(Clone, Copy, Debug)]
pub struct DashPort(u16);
impl DashPort {
    pub fn new(port: u16) -> Result<Self, IncusError> {
        if port == 0 {
            return Err(refused());
        }
        Ok(Self(port))
    }
}

/// No command accepts a raw argv, environment, executable, path or shell program.
/// ```compile_fail
/// use arca_incus::exec::{ExecCommand, TailnetNode};
/// let command = ExecCommand::Join("anything".into());
/// ```
/// ```compile_fail
/// use arca_incus::exec::JournalLines;
/// let lines = JournalLines(201);
/// ```
#[derive(Clone, Debug)]
pub enum ExecCommand {
    Journal(JournalLines),
    TailscaleVersion,
    StartDaemon,
    StopDaemon,
    ClearKeyDir,
    MakeKeyDir,
    Join(TailnetNode),
    TailnetStatus,
    ServeOn(DashPort),
    ServeStatus,
    ServeOff,
    Logout,
    /// Caddy in an edge service reloads the Caddyfile Arca wrote ([`ServiceFile::EdgeConfig`]). Caddy checks the new
    /// config before it swaps, so one it refuses leaves the old one serving.
    EdgeReload,
}
impl ExecCommand {
    pub fn step(&self) -> &'static str {
        match self {
            Self::Journal(_) => "journal",
            Self::TailscaleVersion => "version",
            Self::StartDaemon => "start",
            Self::StopDaemon => "stop",
            Self::ClearKeyDir => "clear_key",
            Self::MakeKeyDir => "make_key",
            Self::Join(_) => "join",
            Self::TailnetStatus => "tailnet_status",
            Self::ServeOn(_) => "serve_on",
            Self::ServeStatus => "serve_status",
            Self::ServeOff => "serve_off",
            Self::Logout => "logout",
            Self::EdgeReload => "edge_reload",
        }
    }
    pub fn seconds(&self) -> u64 {
        match self {
            Self::Journal(_) => 10,
            Self::TailscaleVersion => 15,
            Self::StartDaemon | Self::Logout => 60,
            Self::Join(_) => 120,
            _ => 30,
        }
    }
    pub fn cleanup(&self) -> bool {
        matches!(self, Self::ClearKeyDir | Self::StopDaemon)
    }
    pub fn lines(&self) -> Option<u16> {
        if let Self::Journal(lines) = self {
            Some(lines.get())
        } else {
            None
        }
    }
    pub fn argv(&self) -> Vec<String> {
        let args: Vec<String> = match self {
            Self::Journal(n) => vec![
                "/usr/bin/journalctl".into(),
                "--no-pager".into(),
                "--quiet".into(),
                "-o".into(),
                "short-iso".into(),
                "-n".into(),
                n.0.to_string(),
            ],
            Self::TailscaleVersion => vec!["/usr/bin/tailscale".into(), "version".into()],
            Self::StartDaemon | Self::StopDaemon => vec![
                "/usr/bin/systemctl".into(),
                if matches!(self, Self::StartDaemon) {
                    "start"
                } else {
                    "stop"
                }
                .into(),
                "tailscaled".into(),
            ],
            Self::ClearKeyDir => vec![
                "/usr/bin/rm".into(),
                "-rf".into(),
                "--".into(),
                "/run/arca-ts".into(),
            ],
            Self::MakeKeyDir => vec![
                "/usr/bin/mkdir".into(),
                "-m".into(),
                "0700".into(),
                "--".into(),
                "/run/arca-ts".into(),
            ],
            Self::Join(node) => vec![
                "/usr/bin/tailscale".into(),
                "up".into(),
                "--auth-key=file:/run/arca-ts/authkey".into(),
                format!("--hostname={}", node.0),
                "--advertise-tags=tag:arca-svc".into(),
                "--accept-routes=false".into(),
                "--accept-dns=false".into(),
                "--timeout=60s".into(),
            ],
            Self::TailnetStatus => vec![
                "/usr/bin/tailscale".into(),
                "status".into(),
                "--json".into(),
            ],
            Self::ServeOn(port) => vec![
                "/usr/bin/tailscale".into(),
                "serve".into(),
                "--bg".into(),
                "--https=8443".into(),
                format!("http://127.0.0.1:{}", port.0),
            ],
            Self::ServeStatus => vec![
                "/usr/bin/tailscale".into(),
                "serve".into(),
                "status".into(),
                "--json".into(),
            ],
            Self::ServeOff => vec![
                "/usr/bin/tailscale".into(),
                "serve".into(),
                "--https=8443".into(),
                "off".into(),
            ],
            Self::Logout => vec!["/usr/bin/tailscale".into(), "logout".into()],
            Self::EdgeReload => vec![
                "/usr/bin/caddy".into(),
                "reload".into(),
                "--config".into(),
                EDGE_CONFIG.into(),
                "--adapter".into(),
                "caddyfile".into(),
                "--force".into(),
            ],
        };
        let mut wrapped = vec![
            "/usr/bin/timeout".into(),
            "-k".into(),
            "2".into(),
            self.seconds().to_string(),
        ];
        wrapped.extend(args);
        wrapped
    }
    pub fn body(&self) -> Value {
        json!({"command":self.argv(), "environment":{}, "wait-for-websocket":false,
            "record-output":true, "interactive":false, "width":0, "height":0, "user":0,"group":0,"cwd":""})
    }
}

/// Where a virtual machine's secrets are written inside its guest: a directory of root's, mode 0700, on the guest's own
/// disk, with one file of root's, mode 0600, per secret. Incus writes both through the guest agent.
pub const GUEST_SECRETS_DIR: &str = "/etc/arca-secrets";

/// A destination is constructed only by a fixed variant or the secrets-path validator.
#[derive(Clone, Debug)]
pub enum ServiceFile {
    TailnetKey,
    Secret(SecretName),
    /// [`GUEST_SECRETS_DIR`] itself, made as a directory.
    GuestSecretsDir,
    /// One secret's file in [`GUEST_SECRETS_DIR`].
    GuestSecret(SecretName),
    /// The Caddyfile of an edge service ([`EDGE_CONFIG`]), which Arca renders whole from the node's routes.
    EdgeConfig,
    /// The copy of that Caddyfile an edge keeps on its data volume ([`EDGE_CONFIG_KEPT`]), written only while it runs. The
    /// template's entrypoint puts it back in place when an update or a re-create left the image's own file there.
    EdgeConfigKept,
}

/// Where an edge service's Caddy reads its config: the path the `arca-edge` template runs Caddy with.
pub const EDGE_CONFIG: &str = "/etc/caddy/Caddyfile";
/// Where an edge keeps a copy of its Caddyfile on its data volume, so a rebuilt root gets its routes back.
pub const EDGE_CONFIG_KEPT: &str = "/data/Caddyfile";
#[derive(Clone, Debug)]
pub struct SecretName(String);
impl SecretName {
    pub fn new(name: &str) -> Result<Self, IncusError> {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphabetic() || c.is_ascii_digit() || c == b'_')
        {
            return Err(refused());
        }
        Ok(Self(name.into()))
    }
}
impl ServiceFile {
    pub fn path(&self) -> String {
        match self {
            Self::TailnetKey => "/run/arca-ts/authkey".into(),
            Self::Secret(key) => format!("/run/secrets/{}", key.0),
            Self::GuestSecretsDir => GUEST_SECRETS_DIR.into(),
            Self::GuestSecret(key) => format!("{GUEST_SECRETS_DIR}/{}", key.0),
            Self::EdgeConfig => EDGE_CONFIG.into(),
            Self::EdgeConfigKept => EDGE_CONFIG_KEPT.into(),
        }
    }
    /// The Incus file type and mode the destination is written with: a private directory, or a private file.
    fn kind(&self) -> (&'static str, &'static str) {
        match self {
            Self::GuestSecretsDir => ("directory", "0700"),
            _ => ("file", "0600"),
        }
    }
}

/// What a read of [`GUEST_SECRETS_DIR`] in a guest found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestSecretsDir {
    /// Nothing is at the path.
    Missing,
    /// A directory of root's that only root enters: mode 0700, uid 0, gid 0.
    Private,
    /// Something else is at the path: a file, a link, or a directory with another owner or mode.
    NotPrivate,
}

pub fn valid_operation_id(id: &str) -> bool {
    operation_id(id)
}
fn target(owned: &Owned) -> Result<String, IncusError> {
    if owned.target().kind != IncusKind::Instance
        || owned.target().project != SERVICE_PROJECT
        || owned.target().name.is_empty()
        || owned.uuid().is_none()
        || owned.tombstone().is_some()
    {
        return Err(refused());
    }
    Ok(format!("/1.0/instances/{}", owned.target().name))
}

/// The instance path and the project of a guest-file request: the project is the owned instance's own, as the writer
/// read it under its family, and is the node's service project or a tenant's project and nothing else. Only the two
/// guest-secret destinations are sent to it. A command and every other file stay with [`target`], and a recording is
/// read and deleted by its own fixed path ([`UnixIncusWriter::service_output`]): all three name the
/// node's own project.
fn guest_target(owned: &Owned) -> Result<(String, &str), IncusError> {
    let object = owned.target();
    if object.kind != IncusKind::Instance
        || object.name.is_empty()
        || owned.uuid().is_none()
        || owned.tombstone().is_some()
        || !(object.project == SERVICE_PROJECT
            || arca_kernel::ownership::tenant_of_project(&object.project).is_some())
    {
        return Err(refused());
    }
    Ok((format!("/1.0/instances/{}", object.name), &object.project))
}

impl UnixIncusWriter {
    /// Starts only a closed command on this entry's freshly checked instance. The caller audits its returned id before waiting.
    pub fn start_service_exec(
        &self,
        owned: &Owned,
        command: &ExecCommand,
        timeout: Duration,
    ) -> Result<String, IncusError> {
        let path = target(owned)?;
        if owned.entry() != self.entry() {
            return Err(refused());
        }
        let fresh = self.read_owned(owned.target())?;
        if fresh.uuid() != owned.uuid() || fresh.tombstone().is_some() {
            return Err(refused());
        }
        let framed = self.send_checked(
            "POST",
            &format!("{path}/exec?project={SERVICE_PROJECT}"),
            None,
            Some(&command.body()),
            timeout,
        )?;
        let answer = checked_envelope(&framed.into_raw()).map_err(exec_error)?;
        // Incus's exec endpoint returns the task operation directly, rather than an async envelope.
        let op = if answer.get("id").is_some() {
            &answer
        } else {
            answer.get("metadata").ok_or_else(bad)?
        };
        if op["class"] != "task" {
            return Err(bad());
        }
        op["id"]
            .as_str()
            .filter(|id| operation_id(id))
            .map(str::to_owned)
            .ok_or_else(bad)
    }
    pub fn wait_service_exec(&self, id: &str) -> Result<i64, IncusError> {
        self.wait_on_in(id, Some(SERVICE_PROJECT))
            .map_err(exec_error)?["return"]
            .as_i64()
            .filter(|n| (0..=255).contains(n))
            .ok_or_else(bad)
    }
    /// Reads just one capped recording. Its contents never enter IncusError.
    pub fn service_output(
        &self,
        owned: &Owned,
        id: &str,
        stderr: bool,
    ) -> Result<Vec<u8>, IncusError> {
        self.recording(&owned.target().name, id, stderr, false)
    }
    pub fn delete_service_output(
        &self,
        instance: &str,
        id: &str,
        stderr: bool,
    ) -> Result<(), IncusError> {
        self.recording(instance, id, stderr, true).map(|_| ())
    }
    fn recording(
        &self,
        instance: &str,
        id: &str,
        stderr: bool,
        delete: bool,
    ) -> Result<Vec<u8>, IncusError> {
        if !node_ok(instance) || !operation_id(id) {
            return Err(refused());
        }
        let extension = if stderr { "stderr" } else { "stdout" };
        let path = format!(
            "/1.0/instances/{instance}/logs/exec-output/exec_{id}.{extension}?project={SERVICE_PROJECT}"
        );
        let method = if delete { "DELETE" } else { "GET" };
        let request =
            format!("{method} {path} HTTP/1.1\r\nHost: incus\r\nConnection: close\r\n\r\n");
        let framed = exchange_capped(
            &self.socket,
            self.request_timeout(Duration::from_secs(3)),
            request.as_bytes(),
            OUTPUT_CAP,
        )?;
        if !(200..300).contains(&framed.status) && !(delete && framed.status == 404) {
            return Err(bad());
        }
        Ok(framed.body)
    }
    /// Reads what is at [`GUEST_SECRETS_DIR`] in this entry's freshly checked instance, in the node's own project or in
    /// a tenant's ([`guest_target`]): its type, mode and owner, from
    /// the answer's headers. The answer's body, a listing of names, is not read. An answer that is neither a success
    /// nor "not found" is an error: a guest whose agent does not run yet answers so.
    pub fn guest_secrets_dir(&self, owned: &Owned) -> Result<GuestSecretsDir, IncusError> {
        let (path, project) = guest_target(owned)?;
        if owned.entry() != self.entry() {
            return Err(refused());
        }
        let fresh = self.read_owned(owned.target())?;
        if fresh.uuid() != owned.uuid() || fresh.tombstone().is_some() {
            return Err(refused());
        }
        let path = format!("{path}/files?project={project}&path={GUEST_SECRETS_DIR}");
        check_path(&path)?;
        let request = format!("GET {path} HTTP/1.1\r\nHost: incus\r\nConnection: close\r\n\r\n");
        let framed = exchange_capped(
            &self.socket,
            self.request_timeout(Duration::from_secs(10)),
            request.as_bytes(),
            OUTPUT_CAP,
        )?;
        match framed.status {
            404 => Ok(GuestSecretsDir::Missing),
            200 => {
                // Incus prints the mode as four octal digits; a leading zero more or less is the same mode.
                let mode = framed
                    .header("x-incus-mode")
                    .and_then(|mode| u32::from_str_radix(mode, 8).ok());
                let private = framed.header("x-incus-type") == Some("directory")
                    && mode == Some(0o700)
                    && framed.header("x-incus-uid") == Some("0")
                    && framed.header("x-incus-gid") == Some("0");
                Ok(if private {
                    GuestSecretsDir::Private
                } else {
                    GuestSecretsDir::NotPrivate
                })
            }
            _ => Err(bad()),
        }
    }

    /// The payload is sent only as the files request body, with root ownership and mode 0600. A directory destination
    /// takes no payload and is made with mode 0700; Incus leaves one that exists as it is.
    pub fn write_service_file(
        &self,
        owned: &Owned,
        file: &ServiceFile,
        bytes: &[u8],
    ) -> Result<(), IncusError> {
        // A guest secret goes to the owned instance's own project, which may be a tenant's. Every other file goes to
        // an instance of the node's own project only.
        let (path, project) = match file {
            ServiceFile::GuestSecretsDir | ServiceFile::GuestSecret(_) => guest_target(owned)?,
            ServiceFile::TailnetKey
            | ServiceFile::Secret(_)
            | ServiceFile::EdgeConfig
            | ServiceFile::EdgeConfigKept => (target(owned)?, SERVICE_PROJECT),
        };
        let (kind, mode) = file.kind();
        if bytes.len() > OUTPUT_CAP
            || owned.entry() != self.entry()
            || (kind == "directory" && !bytes.is_empty())
        {
            return Err(refused());
        }
        let fresh = self.read_owned(owned.target())?;
        if fresh.uuid() != owned.uuid() || fresh.tombstone().is_some() {
            return Err(refused());
        }
        let path = format!("{path}/files?project={project}&path={}", file.path());
        check_path(&path)?;
        let mut request = format!("POST {path} HTTP/1.1\r\nHost: incus\r\nConnection: close\r\nX-Incus-type: {kind}\r\nX-Incus-uid: 0\r\nX-Incus-gid: 0\r\nX-Incus-mode: {mode}\r\nX-Incus-write: overwrite\r\nContent-Length: {}\r\n\r\n", bytes.len()).into_bytes();
        request.extend_from_slice(bytes);
        let framed = exchange_capped(
            &self.socket,
            self.request_timeout(Duration::from_secs(10)),
            &request,
            OUTPUT_CAP,
        )?;
        checked_envelope(&framed.into_raw())
            .map(|_| ())
            .map_err(exec_error)
    }
}
