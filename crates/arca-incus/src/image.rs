//! Closed same-daemon image copies into one tenant's own image store. No image download or arbitrary endpoint.
use crate::{
    IncusError, IncusRead, UnixIncus,
    read::checked_envelope,
    write::{ProjectFamily, UnixIncusWriter},
};
use arca_kernel::{desired::EntryId, ownership::tenant_of_project};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

/// The one source store for this closed writer.
pub const SOURCE_PROJECT: &str = "arcak-svc";

/// A checked property replacement. A stale observation or rejected conditional write changes nothing.
pub enum PropertyUpdate {
    Stale,
    Unchanged,
    Sent(Result<Option<String>, IncusError>),
}

pub struct ImageWriter {
    socket: PathBuf,
    project: String,
    fingerprint: String,
    alias: String,
}

pub fn fingerprint_ok(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn alias_ok(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

impl ImageWriter {
    pub fn new(
        socket: PathBuf,
        project: &str,
        fingerprint: &str,
        alias: &str,
    ) -> Result<Self, IncusError> {
        if tenant_of_project(project).is_none() || !fingerprint_ok(fingerprint) || !alias_ok(alias)
        {
            return Err(IncusError::Refused("invalid tenant image target".into()));
        }
        Ok(Self {
            socket,
            project: project.into(),
            fingerprint: fingerprint.into(),
            alias: alias.into(),
        })
    }

    fn transport(&self) -> Result<UnixIncusWriter, IncusError> {
        Ok(UnixIncusWriter::new(
            self.socket.clone(),
            EntryId("tenant-image-import".into()),
            BTreeMap::new(),
            ProjectFamily::new([], []).map_err(IncusError::Refused)?,
        )
        .with_operation_deadline(Duration::from_secs(600)))
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Option<String>, IncusError> {
        self.send_if_match(method, path, None, body)
    }

    fn send_if_match(
        &self,
        method: &str,
        path: &str,
        if_match: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Option<String>, IncusError> {
        let framed = self.transport()?.send_checked(
            method,
            path,
            if_match,
            body,
            Duration::from_secs(30),
        )?;
        let reply = checked_envelope(&framed.into_raw())?;
        match reply
            .get("operation")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            None => Ok(None),
            Some(url) => {
                let id = url
                    .strip_prefix("/1.0/operations/")
                    .filter(|id| {
                        !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
                    })
                    .ok_or_else(|| IncusError::Decode("invalid image operation".into()))?;
                Ok(Some(id.into()))
            }
        }
    }

    pub fn wait(&self, operation: &str) -> Result<(), IncusError> {
        self.transport()?
            .wait_on_in(operation, Some(&self.project))
            .map(|_| ())
    }

    pub fn image(&self) -> Result<Option<Value>, IncusError> {
        self.read(&format!(
            "/1.0/images/{}?project={}",
            self.fingerprint, self.project
        ))
    }

    pub fn alias(&self) -> Result<Option<Value>, IncusError> {
        self.read(&format!(
            "/1.0/images/aliases/{}?project={}",
            self.alias, self.project
        ))
    }

    fn read(&self, path: &str) -> Result<Option<Value>, IncusError> {
        match UnixIncus::new(self.socket.clone()).get(path) {
            Ok(value) => Ok(Some(value)),
            Err(IncusError::Status { code: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn copy(&self, properties: &Value) -> Result<Option<String>, IncusError> {
        self.send("POST", &format!("/1.0/images?project={}", self.project), Some(&json!({
            "source": { "type": "image", "fingerprint": self.fingerprint, "project": SOURCE_PROJECT },
            "properties": properties, "profiles": [], "public": false, "auto_update": false
        })))
    }

    /// Replace only the map approved for replacement, under the final read's ETag.
    pub fn set_properties(
        &self,
        expected: &Value,
        properties: &Value,
    ) -> Result<PropertyUpdate, IncusError> {
        let path = format!("/1.0/images/{}?project={}", self.fingerprint, self.project);
        let framed =
            self.transport()?
                .send_checked("GET", &path, None, None, Duration::from_secs(30))?;
        let etag = framed.header("etag").map(str::to_owned);
        let envelope = match checked_envelope(&framed.into_raw()) {
            Ok(envelope) => envelope,
            Err(IncusError::Status { code: 404, .. }) => return Ok(PropertyUpdate::Stale),
            Err(error) => return Err(error),
        };
        let image = &envelope["metadata"];
        if image["fingerprint"] != self.fingerprint || &image["properties"] != expected {
            return Ok(PropertyUpdate::Stale);
        }
        let etag = etag.ok_or_else(|| IncusError::Decode("image carries no ETag".into()))?;
        // PUT replaces these fields too; approval changes only the property map.
        let body = json!({ "properties": properties,
            "public": image["public"].as_bool().unwrap_or(false),
            "auto_update": image["auto_update"].as_bool().unwrap_or(false),
            "profiles": image.get("profiles").cloned().unwrap_or(json!([])) });
        match self.send_if_match("PUT", &path, Some(&etag), Some(&body)) {
            Ok(operation) => Ok(PropertyUpdate::Sent(Ok(operation))),
            Err(IncusError::Status { code: 412, .. }) => {
                // A rejected conditional PUT wrote nothing. Another writer may already have completed it.
                Ok(match self.image() {
                    Ok(Some(found))
                        if found["fingerprint"] == self.fingerprint
                            && &found["properties"] == properties =>
                    {
                        PropertyUpdate::Unchanged
                    }
                    _ => PropertyUpdate::Stale,
                })
            }
            Err(error) => Ok(PropertyUpdate::Sent(Err(error))),
        }
    }

    pub fn add_alias(&self) -> Result<Option<String>, IncusError> {
        self.send(
            "POST",
            &format!("/1.0/images/aliases?project={}", self.project),
            Some(&json!({ "name": self.alias, "target": self.fingerprint })),
        )
    }
}
