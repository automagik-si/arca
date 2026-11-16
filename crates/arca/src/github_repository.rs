//! Workspace Git credentials from a separate repository App. Runner credentials never enter this path.
use reqwest::Method;
use serde_json::{Value, json};

use crate::github_api::{Auth, Secret};
use crate::github_app::AppStart;
use crate::github_signer::{Mode, Signer, SignerError, Status};

/// An installation credential and verified GitHub bot identity, for guest-file delivery only.
#[derive(Debug)]
pub struct RepositoryCredential {
    pub token: Secret,
    pub bot_id: u64,
    pub bot_login: String,
}

/// A refusal whose words contain no credential or upstream response body.
#[derive(Debug)]
pub struct RepositoryError {
    pub why: String,
    pub retry: bool,
}
impl From<&str> for RepositoryError {
    fn from(why: &str) -> Self {
        Self {
            why: why.into(),
            retry: false,
        }
    }
}
impl RepositoryError {
    fn later(why: &str) -> Self {
        Self {
            why: why.into(),
            retry: true,
        }
    }
    fn answered(why: &str, status: u16) -> Self {
        Self {
            why: why.into(),
            retry: status == 429 || status >= 500,
        }
    }
}

pub fn permissions() -> Value {
    json!({"contents":"write", "pull_requests":"write", "metadata":"read"})
}

/// The pilot accepts only a credential-free GitHub HTTPS repository URL.
pub fn repository(url: &str) -> Option<(String, String)> {
    let path = url
        .strip_prefix("https://github.com/")?
        .to_ascii_lowercase();
    let (owner, repo) = path.split_once('/')?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if owner.is_empty()
        || repo.is_empty()
        || matches!(repo, "." | "..")
        || !owner
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        || !repo
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    {
        return None;
    }
    Some((owner.to_ascii_lowercase(), repo.to_ascii_lowercase()))
}

impl Signer {
    /// Reuses the manifest callbacks, with a distinct purpose that can never mint runner credentials.
    pub fn repository_app_begin(
        &self,
        account: &str,
        origin: &str,
        caller: &str,
    ) -> Result<AppStart, SignerError> {
        self.app_begin_for(account, &[], origin, caller, true)
    }

    /// Mints without a shared installation-token cache: every request names exactly one repository and permissions.
    /// No tool returns this credential; the guest writer consumes it in process.
    pub async fn repository_credential(
        &self,
        repo_url: &str,
    ) -> Result<RepositoryCredential, RepositoryError> {
        let (owner, repo) =
            repository(repo_url).ok_or("workspace repoUrl must be a GitHub HTTPS repository")?;
        let bindings: Vec<_> = self
            .bindings()
            .into_iter()
            .filter(|b| {
                b.repository_access
                    && b.mode == Mode::App
                    && b.status == Status::Active
                    && b.account.eq_ignore_ascii_case(&owner)
            })
            .collect();
        if bindings.len() != 1 {
            return Err(
                "the owner must connect exactly one active repository App for this organisation"
                    .into(),
            );
        }
        let binding = &bindings[0];
        let installation = binding
            .installation
            .as_ref()
            .ok_or("the repository App is not installed")?;
        let app = binding
            .app
            .as_ref()
            .ok_or("the repository App has no record")?;
        let jwt = self
            .app_jwt(&binding.id)
            .map_err(|_| "the repository App could not sign")?;
        let http = &self.inner.http;
        // Check that this repository belongs to this installation before minting. A transferred or removed repo
        // must never select another installation just because its name matches.
        let answer = http
            .send(
                Method::GET,
                &http.api(&format!("/repos/{owner}/{repo}/installation")),
                Auth::Bearer(&jwt),
                None,
            )
            .await
            .map_err(|_| {
                RepositoryError::later("GitHub could not verify the repository installation")
            })?;
        if !answer.ok() || answer.json()["id"].as_u64() != Some(installation.id) {
            return Err(RepositoryError::answered(
                "the repository is not in the owner's repository App installation",
                answer.status,
            ));
        }
        let expected_login = format!("{}[bot]", app.slug);
        let (bot_id, bot_login) = if let (Some(id), Some(login)) = (
            app.bot_id.filter(|id| *id > 0),
            app.bot_login
                .as_ref()
                .filter(|login| login.eq_ignore_ascii_case(&expected_login)),
        ) {
            (id, login.clone())
        } else {
            let answer = http
                .send(
                    Method::GET,
                    &http.api(&format!("/users/{}%5Bbot%5D", app.slug)),
                    Auth::None,
                    None,
                )
                .await
                .map_err(|_| {
                    RepositoryError::later("GitHub could not verify the repository App bot")
                })?;
            let user = answer.json();
            if !answer.ok()
                || !user["login"]
                    .as_str()
                    .is_some_and(|login| login.eq_ignore_ascii_case(&expected_login))
                || user["type"].as_str() != Some("Bot")
            {
                return Err(RepositoryError::answered(
                    "GitHub did not verify the repository App bot identity",
                    answer.status,
                ));
            }
            let bot_login = user["login"]
                .as_str()
                .ok_or("GitHub returned no repository App bot login")?
                .to_string();
            let bot_id = user["id"]
                .as_u64()
                .filter(|id| *id > 0)
                .ok_or("GitHub returned no repository App bot user ID")?;
            let mut verified = binding.clone();
            if let Some(app) = verified.app.as_mut() {
                app.bot_id = Some(bot_id);
                app.bot_login = Some(bot_login.clone());
            }
            verified.updated_at = self.now();
            self.save(verified)
                .map_err(|_| "the verified repository bot identity could not be saved")?;
            (bot_id, bot_login)
        };
        let body = json!({"repositories":[repo], "permissions":permissions()});
        let answer = http
            .send(
                Method::POST,
                &http.api(&format!(
                    "/app/installations/{}/access_tokens",
                    installation.id
                )),
                Auth::Bearer(&jwt),
                Some(&body),
            )
            .await
            .map_err(|_| {
                RepositoryError::later("GitHub could not mint the repository credential")
            })?;
        if !answer.ok() {
            return Err(RepositoryError::answered(
                "GitHub refused the one-repository credential; the owner must check the App installation",
                answer.status,
            ));
        }
        let token = answer.json()["token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(Secret::new)
            .ok_or("GitHub returned no repository credential")?;
        self.audit(json!({"kind":"repositoryMint", "binding":binding.id, "repository":format!("{owner}/{repo}")}));
        Ok(RepositoryCredential {
            token,
            bot_id,
            bot_login,
        })
    }
}
