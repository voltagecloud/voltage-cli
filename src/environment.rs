//! `environments create`: describe the new environment, or send it to the auth service.
//!
//! Environments belong to the auth service, not the Voltage API, so this command is not in
//! the operation registry. It follows the same safety model as an API change: without
//! `--execute` or `VOLTAGE_EXECUTE=1`, it describes the request before authenticating and
//! exits 6. The CLI chooses the environment's ID, as the contract requires, so a described
//! and a sent run both report it in `resource_id`.

use crate::{
    Result,
    api::{self, Execution},
    auth::{self, NewEnvironment, Resolver},
    cli::{CreateEnvironmentFlags, GlobalFlags},
    config::{AUTH_URL, Scope, Settings, shell_word},
    dry_run,
    output::{Envelope, NextStep, Outcome, Output},
};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

const COMMAND: &str = "environments create";
const OMITTED: &str = "[OMITTED]";

/// The `data` of a described `environments create` (`outcome: dry_run`).
#[derive(Serialize)]
struct Description {
    operation: &'static str,
    method: &'static str,
    url: String,
    body: Value,
    /// The name was replaced with `[OMITTED]`.
    values_omitted: bool,
    /// A browser login's token, exchanged for the organization.
    authentication: &'static str,
    notes: Vec<&'static str>,
}

/// Run `environments create`: describe it and fail with exit code 6, or create it once.
pub async fn create(
    flags: &CreateEnvironmentFlags,
    execution: Execution,
    global: &GlobalFlags,
    resolver: Resolver<'_>,
    scope: &Scope,
    out: &mut Output,
) -> Result<()> {
    let environment = NewEnvironment {
        environment_id: flags.id.unwrap_or_else(Uuid::new_v4),
        organization_id: scope.require_org()?,
        name: flags.name.clone(),
    };
    let terminal = global.terminal();
    match execution {
        Execution::DescribeChange => {
            let described = describe(
                &environment,
                flags.id.is_none(),
                global,
                resolver.settings,
                scope,
            )?;
            if out.is_human() && terminal.decorates() {
                let facts = json!({"environment_id": environment.environment_id});
                dry_run::announce_change(terminal, "create an environment", &facts);
            }
            out.write(described, &[])?;
            return Err(api::not_sent());
        }
        Execution::SendFromEnvironment => api::sending_from_environment(terminal),
        Execution::Send => {}
    }
    let status = resolver.create_environment(scope, &environment).await?;
    let id = environment.environment_id;
    let created = Envelope::new(
        Some(status),
        serde_json::to_value(&environment)?,
        Some(id),
        Outcome::Succeeded,
    );
    out.write(
        Envelope {
            next: Some(NextStep {
                what: "Save it in a profile",
                command: format!(
                    "voltage profiles create PROFILE --org {} --env {id}{}",
                    environment.organization_id,
                    account_flag(scope)
                ),
            }),
            ..created
        },
        &[],
    )
}

/// The request a real run would send. The name is private unless `--show-secrets`.
fn describe(
    environment: &NewEnvironment,
    generated_id: bool,
    global: &GlobalFlags,
    settings: &Settings,
    scope: &Scope,
) -> Result<Envelope> {
    let mut body = serde_json::to_value(environment)?;
    if !global.show_secrets {
        body["name"] = Value::String(OMITTED.into());
    }
    let mut notes = vec![
        "No request was sent. Local validation does not prove the auth service will accept it.",
        "A real run needs a browser login; an API key cannot create environments.",
    ];
    if generated_id {
        notes.push("The environment_id was generated for this description; a real run generates a new one unless you pass --id.");
    }
    let description = Description {
        operation: COMMAND,
        method: "POST",
        url: format!(
            "{}/organizations/{}/environments",
            auth_url(global, settings, scope)?,
            environment.organization_id
        ),
        body,
        values_omitted: !global.show_secrets,
        authentication: "browser_login",
        notes,
    };
    Ok(Envelope::new(
        None,
        serde_json::to_value(description)?,
        Some(environment.environment_id),
        Outcome::DryRun,
    ))
}

/// The auth service a real run would call: the selected login's, which is where it was
/// created, else the one `--auth-url` or the default names.
fn auth_url(global: &GlobalFlags, settings: &Settings, scope: &Scope) -> Result<String> {
    let saved = settings
        .account_name(scope)
        .ok()
        .and_then(|name| settings.account(&name).ok())
        .map(|account| account.auth_url.as_str());
    auth::base_url(saved.unwrap_or(global.auth_url.as_deref().unwrap_or(AUTH_URL)))
}

/// `--account NAME` when the command named its login, so the next command uses it too.
fn account_flag(scope: &Scope) -> String {
    scope
        .account
        .as_deref()
        .map(|name| format!(" --account {}", shell_word(name)))
        .unwrap_or_default()
}
