use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use isideload::{
    anisette::remote_v3::RemoteV3AnisetteProvider,
    auth::apple_account::{AppleAccount, TwoFactorCallbackParams, TwoFactorCallbackResponse},
    dev::{
        app_ids::{AppIdsApi, ListAppIdsResponse},
        certificates::{CertificatesApi, DevelopmentCertificate},
        developer_session::DeveloperSession,
        teams::{DeveloperTeam, TeamsApi},
    },
    sideload::{
        SideloaderBuilder, TeamSelection,
        builder::{ExtensionsBehavior, ExtensionsBehaviorSelection, MaxCertsBehavior},
        sideloader::Sideloader,
    },
    util::{
        callbacks::{ExtensionsBehaviorCallbackBox, MaxCertsCallbackBox},
        fs_storage::FsStorage,
    },
};
use tracing::{info, warn};

use crate::{ReportExt, anisette, config::Config, paths::Paths};

/// Interactive decisions the login and signing flow can't make on its own.
/// The CLI answers on stdin, the GUI with dialogs.
#[async_trait]
pub trait Prompter: Send + Sync {
    async fn two_factor(
        &self,
        params: TwoFactorCallbackParams,
    ) -> Result<TwoFactorCallbackResponse>;

    /// Return the chosen team id, or `None` to cancel.
    async fn select_team(&self, teams: &[DeveloperTeam]) -> Result<Option<String>>;

    /// Free accounts can only hold a couple of development certificates. Return the serial
    /// numbers to revoke, or `None` to cancel.
    async fn revoke_certificates(
        &self,
        certs: Vec<DevelopmentCertificate>,
    ) -> Result<Option<Vec<String>>>;
}

pub struct LoginRequest {
    pub apple_id: String,
    pub password: String,
    /// Overrides the configured anisette server for this login.
    pub anisette_server: Option<String>,
}

pub type AppSideloader = Sideloader<MaxCertsCallbackBox, ExtensionsBehaviorCallbackBox>;

/// A logged-in Apple ID with a selected developer team, ready to sign apps.
pub struct Session {
    pub apple_id: String,
    pub team: DeveloperTeam,
    pub anisette_server: String,
    pub(crate) sideloader: AppSideloader,
    pub(crate) paths: Paths,
    /// Read by the extensions callback on every signing run.
    pub(crate) register_extensions: Arc<AtomicBool>,
    /// Read by the max-certs callback on every signing run.
    pub(crate) auto_revoke: Arc<AtomicBool>,
    created: std::time::Instant,
}

/// Apple rejected the password saved in the keychain, so it was forgotten. The user has to
/// sign in again (usually because they changed their Apple ID password).
#[derive(Debug)]
pub struct SavedPasswordRejected {
    pub apple_id: String,
}

impl std::fmt::Display for SavedPasswordRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Apple rejected the saved password for {} (was it changed?). Sign in again.",
            self.apple_id
        )
    }
}

impl std::error::Error for SavedPasswordRejected {}

/// Apple's "incorrect Apple ID or password" error.
pub fn is_wrong_password(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("-22406")
}

/// Log in and, with `remember`, keep the password in the OS keychain so the user stays signed
/// in. When the password came from the keychain and Apple rejects it, it is deleted and
/// [`SavedPasswordRejected`] is returned.
pub async fn login_and_remember(
    paths: &Paths,
    config: &mut Config,
    request: LoginRequest,
    from_keychain: bool,
    remember: bool,
    prompter: Arc<dyn Prompter>,
) -> Result<Session> {
    let apple_id = request.apple_id.trim().to_lowercase();
    let password = request.password.clone();
    match login(paths, config, request, prompter).await {
        Ok(session) => {
            if remember
                && !from_keychain
                && let Err(e) = crate::credentials::save_password(&apple_id, &password)
            {
                warn!("Couldn't save the password to the keychain: {e:#}");
            }
            Ok(session)
        }
        Err(e) if from_keychain && is_wrong_password(&e) => {
            let _ = crate::credentials::delete_password(&apple_id);
            Err(anyhow::Error::new(SavedPasswordRejected { apple_id }))
        }
        Err(e) => Err(e),
    }
}

/// Log in with the remembered Apple ID and its saved password, if there are both.
pub async fn login_saved(
    paths: &Paths,
    config: &mut Config,
    prompter: Arc<dyn Prompter>,
) -> Result<Option<Session>> {
    let Some(apple_id) = config.apple_id.clone() else {
        return Ok(None);
    };
    let Some(password) = crate::credentials::get_password(&apple_id)? else {
        return Ok(None);
    };
    let request = LoginRequest {
        apple_id,
        password,
        anisette_server: None,
    };
    login_and_remember(paths, config, request, true, true, prompter)
        .await
        .map(Some)
}

fn to_report(e: anyhow::Error) -> rootcause::Report {
    rootcause::report!("{e:#}")
}

/// Log in, trying the fallback anisette servers if Apple rejects the first one.
pub async fn login(
    paths: &Paths,
    config: &mut Config,
    request: LoginRequest,
    prompter: Arc<dyn Prompter>,
) -> Result<Session> {
    let apple_id = request.apple_id.trim().to_lowercase();
    let preferred = request
        .anisette_server
        .clone()
        .unwrap_or_else(|| config.anisette_server.clone());
    let servers = anisette::fallback_order(&preferred, config.anisette_auto_fallback);

    let mut last_error = None;
    let mut account = None;
    for server in &servers {
        info!("Logging in as {apple_id} using anisette server {server}");
        match login_with_server(
            paths,
            &apple_id,
            &request.password,
            server,
            prompter.clone(),
        )
        .await
        {
            Ok(acc) => {
                account = Some((acc, server.clone()));
                break;
            }
            Err(e) => {
                let message = format!("{e:#}");
                if anisette::is_anisette_related(&message) || message.contains("Failed to request")
                {
                    warn!("Login via {server} failed: {message}");
                    last_error = Some(e);
                    continue;
                }
                return Err(with_hint(e));
            }
        }
    }
    let Some((mut account, server)) = account else {
        return Err(with_hint(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("No anisette servers configured")
        })));
    };

    if server != anisette::normalize(&config.anisette_server) {
        info!("Remembering {server} as the working anisette server");
        config.anisette_server = server.clone();
    }
    config.apple_id = Some(apple_id.clone());

    let mut dev_session = DeveloperSession::from_account(&mut account)
        .await
        .anyhow()
        .context("Failed to start a developer session")?;

    let team = choose_team(&mut dev_session, config, &apple_id, prompter.as_ref()).await?;
    config.save(paths)?;

    let register_extensions = Arc::new(AtomicBool::new(false));
    let auto_revoke = Arc::new(AtomicBool::new(false));

    let ext_flag = register_extensions.clone();
    let extensions_callback: ExtensionsBehaviorCallbackBox = Box::new(move |_bundles| {
        let register = ext_flag.load(Ordering::Relaxed);
        Box::pin(async move {
            // Reusing the main profile saves App IDs, which free accounts can only create 10 of per week
            Ok(if register {
                ExtensionsBehavior::RegisterAll
            } else {
                ExtensionsBehavior::ReuseMain
            })
        })
    });

    let revoke_flag = auto_revoke.clone();
    let cert_prompter = prompter.clone();
    let machine_name = config.machine_name.clone();
    let max_certs_callback: MaxCertsCallbackBox = Box::new(move |certs| {
        let auto = revoke_flag.load(Ordering::Relaxed);
        let prompter = cert_prompter.clone();
        let machine_name = machine_name.clone();
        Box::pin(async move {
            if auto {
                return Ok(pick_certificate_to_revoke(&certs, &machine_name).map(|s| vec![s]));
            }
            prompter.revoke_certificates(certs).await.map_err(to_report)
        })
    });

    let signing_dir = paths.signing_dir();
    std::fs::create_dir_all(&signing_dir)?;
    let sideloader = SideloaderBuilder::new(dev_session, apple_id.clone())
        .team_selection(TeamSelection::First)
        .max_certs_behavior(MaxCertsBehavior::Prompt(max_certs_callback))
        .extensions_behavior(ExtensionsBehaviorSelection::Prompt(extensions_callback))
        .storage(Box::new(FsStorage::new(signing_dir)))
        .machine_name(config.machine_name.clone())
        .delete_app_after_install(false)
        .build();

    Ok(Session {
        apple_id,
        team,
        anisette_server: server,
        sideloader,
        paths: paths.clone(),
        register_extensions,
        auto_revoke,
        created: std::time::Instant::now(),
    })
}

/// Prefer one of Sideward's own older certificates, so apps signed by other tools keep working;
/// otherwise the one that expires first.
fn pick_certificate_to_revoke(
    certs: &[DevelopmentCertificate],
    machine_name: &str,
) -> Option<String> {
    let expiry = |c: &&DevelopmentCertificate| c.expiration_date.map(std::time::SystemTime::from);
    certs
        .iter()
        .filter(|c| c.machine_name.as_deref() == Some(machine_name))
        .min_by_key(expiry)
        .or_else(|| certs.iter().min_by_key(expiry))
        .and_then(|c| c.serial_number.clone())
}

async fn login_with_server(
    paths: &Paths,
    apple_id: &str,
    password: &str,
    server: &str,
    prompter: Arc<dyn Prompter>,
) -> Result<AppleAccount> {
    let storage_dir = paths.anisette_dir(server);
    std::fs::create_dir_all(&storage_dir)?;

    let provider = RemoteV3AnisetteProvider::default()
        .anyhow()?
        .set_url(server)
        .set_serial_number("0".to_string())
        .set_storage(Box::new(FsStorage::new(storage_dir)));

    let two_factor = move |params: TwoFactorCallbackParams| {
        let prompter = prompter.clone();
        async move { prompter.two_factor(params).await.map_err(to_report) }
    };

    AppleAccount::builder(apple_id)
        .anisette_provider(provider)
        .login(password, two_factor)
        .await
        .anyhow()
}

fn with_hint(e: anyhow::Error) -> anyhow::Error {
    let message = format!("{e:#}");
    match anisette::hint_for_error(&message) {
        Some(hint) => e.context(hint),
        None => e,
    }
}

async fn choose_team(
    dev_session: &mut DeveloperSession,
    config: &mut Config,
    apple_id: &str,
    prompter: &dyn Prompter,
) -> Result<DeveloperTeam> {
    let teams = dev_session.list_teams().await.anyhow()?;
    if teams.is_empty() {
        bail!(
            "This Apple ID has no developer teams. Sign in once at developer.apple.com to accept the terms."
        );
    }
    if let Some(saved) = config.teams.get(apple_id)
        && let Some(team) = teams.iter().find(|t| &t.team_id == saved)
    {
        return Ok(team.clone());
    }
    let team = if teams.len() == 1 {
        teams[0].clone()
    } else {
        let selected = prompter
            .select_team(&teams)
            .await?
            .context("No team selected")?;
        teams
            .iter()
            .find(|t| t.team_id == selected)
            .cloned()
            .with_context(|| format!("No team with id {selected}"))?
    };
    config
        .teams
        .insert(apple_id.to_string(), team.team_id.clone());
    Ok(team)
}

impl Session {
    /// How long ago this session logged in. Apple's developer session tokens don't last
    /// forever, so long-running callers log in again after a while.
    pub fn age(&self) -> std::time::Duration {
        self.created.elapsed()
    }

    pub fn dev_session(&mut self) -> &mut DeveloperSession {
        self.sideloader.get_dev_session()
    }

    pub async fn list_certificates(&mut self) -> Result<Vec<DevelopmentCertificate>> {
        let team = self.team.clone();
        self.dev_session()
            .list_all_development_certs(&team, None)
            .await
            .anyhow()
    }

    pub async fn revoke_certificate(&mut self, serial_number: &str) -> Result<()> {
        let team = self.team.clone();
        self.dev_session()
            .revoke_development_cert(&team, serial_number, None)
            .await
            .anyhow()
    }

    pub async fn list_app_ids(&mut self) -> Result<ListAppIdsResponse> {
        let team = self.team.clone();
        self.dev_session().list_app_ids(&team, None).await.anyhow()
    }

    pub async fn delete_app_id(&mut self, app_id_id: &str) -> Result<()> {
        let team = self.team.clone();
        self.dev_session()
            .delete_app_id(&team, app_id_id, None)
            .await
            .anyhow()
    }
}
