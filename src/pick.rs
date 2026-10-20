//! Interactive scope: when a command lacks the organization, environment, or wallet it needs,
//! a person at a terminal picks one instead of copying a UUID. Scripts never see a prompt.

use crate::{
    Result, api,
    auth::{ApiCredential, Discovery, Resolver},
    cli::{BodySource, Command, GlobalFlags, LocalCommand, ProfileCommand},
    config::{Credential, Scope},
    output::OutputFormat,
    registry::{OperationId, ParameterLocation, ScopeParameter},
    terminal::{Choice, Terminal},
};
use serde_json::Value;
use uuid::Uuid;

/// The scope a command cannot run without.
#[derive(Debug, Default, Eq, PartialEq)]
struct Needs {
    org: bool,
    env: bool,
    wallet: bool,
}

impl Needs {
    /// An environment or wallet is listed within an organization, so either needs one.
    fn new(org: bool, env: bool, wallet: bool) -> Self {
        Self {
            org: org || env || wallet,
            env,
            wallet,
        }
    }
}

fn needs(command: &Command) -> Needs {
    match command {
        Command::Api(api) => {
            let operation = api.operation;
            let target = operation.target.map(|target| target.parameter_name());
            let scopes: Vec<ScopeParameter> = operation
                .parameters
                .iter()
                .filter(|parameter| parameter.location == ParameterLocation::Path)
                .filter(|parameter| Some(parameter.name.as_str()) != target)
                .filter_map(|parameter| parameter.scope())
                .collect();
            let friendly = matches!(api.body, Some(BodySource::Friendly(_)));
            Needs::new(
                scopes.contains(&ScopeParameter::Organization),
                scopes.contains(&ScopeParameter::Environment)
                    || (friendly && operation.id == OperationId::CreateWallet),
                scopes.contains(&ScopeParameter::Wallet)
                    || (friendly && operation.id == OperationId::CreatePayment),
            )
        }
        Command::Local(LocalCommand::Environments { .. }) => Needs::new(true, false, false),
        Command::Local(LocalCommand::Profiles {
            command: ProfileCommand::Create { .. },
        }) => Needs::new(true, true, false),
        // Listed by name so that a new local command must decide what scope it needs.
        Command::Local(
            LocalCommand::Login(_)
            | LocalCommand::Logout(_)
            | LocalCommand::Auth { .. }
            | LocalCommand::Organizations { .. }
            | LocalCommand::Profiles {
                command:
                    ProfileCommand::List | ProfileCommand::Get { .. } | ProfileCommand::Delete { .. },
            }
            | LocalCommand::Context
            | LocalCommand::Price(_)
            | LocalCommand::Convert(_)
            | LocalCommand::Completions { .. },
        ) => Needs::default(),
    }
}

/// Ask for each missing part of the scope the command needs, in the order organization,
/// environment, wallet. Without an interactive terminal, with `--no-input`, or for JSON
/// output, nothing is asked and the command reports what is missing as before. Esc, an empty
/// list, or a credential that cannot list choices also leaves the rest of the scope unset.
pub async fn fill(
    command: &Command,
    global: &GlobalFlags,
    resolver: Resolver<'_>,
    scope: &mut Scope,
) -> Result<()> {
    let needs = needs(command);
    let missing = Needs {
        org: needs.org && scope.org.is_none(),
        env: needs.env && scope.envs.is_empty(),
        wallet: needs.wallet && scope.wallet.is_none(),
    };
    let terminal = global.terminal();
    if missing == Needs::default()
        || global.output_format() != OutputFormat::Table
        || !terminal.can_pick()
    {
        return Ok(());
    }
    let picked = ask(missing, global, resolver, scope).await?;
    if !picked.is_empty() {
        let profile = match command {
            Command::Local(LocalCommand::Profiles { .. }) => "",
            _ => " or save a profile with voltage profiles create",
        };
        terminal.notice(format!("Next time, pass {}{profile}.", picked.join(" ")));
    }
    Ok(())
}

/// The pickers themselves, with one credential resolution and at most one organization token
/// exchange. An API key cannot list organizations or environments, so only its wallets are
/// offered. Returns the flags for what was picked, which Esc or an empty list can cut short.
async fn ask(
    missing: Needs,
    global: &GlobalFlags,
    resolver: Resolver<'_>,
    scope: &mut Scope,
) -> Result<Vec<String>> {
    let terminal = global.terminal();
    // Without a credential there is nothing to list. That, not the missing scope, is what the
    // person must fix first, so report it.
    let credential = resolver.resolve(scope).await?;
    let mut picked = Vec::new();
    if missing.org {
        let Credential::Login(login) = &credential else {
            terminal.notice("An API key cannot list organizations, so there is nothing to pick.");
            return Ok(picked);
        };
        let listed = resolver
            .list(scope, Discovery::Organizations, &login.access_token)
            .await?;
        let Some(org) = choose(terminal, "Organization:", "organizations", &listed).await? else {
            return Ok(picked);
        };
        scope.org = Some(org);
        picked.push(format!("--org {org}"));
    }
    if !missing.env && !missing.wallet {
        return Ok(picked);
    }
    let credential = resolver.exchange(scope, credential).await?;
    if missing.env {
        let ApiCredential::OrganizationToken(token) = &credential else {
            terminal.notice("An API key cannot list environments, so there is nothing to pick.");
            return Ok(picked);
        };
        let listed = resolver.list(scope, Discovery::Environments, token).await?;
        let Some(env) = choose(terminal, "Environment:", "environments", &listed).await? else {
            return Ok(picked);
        };
        scope.envs = vec![env];
        picked.push(format!("--env {env}"));
    }
    if missing.wallet {
        let listed = api::wallets(global, credential, scope).await?;
        let Some(wallet) = choose(terminal, "Wallet:", "wallets", &listed).await? else {
            return Ok(picked);
        };
        scope.wallet = Some(wallet);
        picked.push(format!("--wallet {wallet}"));
    }
    Ok(picked)
}

/// Pick from a listing, or say why there is nothing to pick.
async fn choose(
    terminal: Terminal,
    question: &'static str,
    plural: &str,
    listed: &Value,
) -> Result<Option<Uuid>> {
    let choices = choices(listed);
    if choices.is_empty() {
        terminal.notice(format!("No {plural} to choose from."));
        return Ok(None);
    }
    terminal.pick(question, choices).await
}

/// The listed resources that carry a UUID `id`, sorted by name.
fn choices(listed: &Value) -> Vec<Choice> {
    let items = listed
        .as_array()
        .or_else(|| listed.get("items").and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut choices: Vec<Choice> = items
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.parse().ok()?;
            let name = item.get("name").and_then(Value::as_str).unwrap_or("-");
            Some(Choice::new(id, name))
        })
        .collect();
    choices.sort_by_key(|choice| choice.name().to_lowercase());
    choices
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Invocation;
    use serde_json::json;

    const ID: &str = "11111111-1111-4111-8111-111111111111";

    fn needs_of(args: &[&str]) -> Needs {
        let mut full = vec!["voltage"];
        full.extend_from_slice(args);
        needs(&Invocation::try_parse_from(full).unwrap().command)
    }

    #[test]
    fn commands_ask_only_for_the_scope_they_cannot_run_without() {
        assert_eq!(
            needs_of(&["wallets", "list"]),
            Needs::new(true, false, false)
        );
        assert_eq!(
            needs_of(&["payments", "list"]),
            Needs::new(true, true, false)
        );
        assert_eq!(
            needs_of(&[
                "payments",
                "send",
                "--currency",
                "btc",
                "--invoice",
                "lnbc1"
            ]),
            Needs::new(true, true, true)
        );
        assert_eq!(
            needs_of(&["wallets", "create", "--name", "a"]),
            Needs::new(true, true, false)
        );
        // The positional target is typed, not picked.
        assert_eq!(
            needs_of(&["wallets", "get", ID]),
            Needs::new(true, false, false)
        );
        assert_eq!(
            needs_of(&["environments", "list"]),
            Needs::new(true, false, false)
        );
        assert_eq!(
            needs_of(&["profiles", "create", "prod"]),
            Needs::new(true, true, false)
        );
        for args in [&["organizations", "list"][..], &["context"], &["price"]] {
            assert_eq!(needs_of(args), Needs::default(), "{args:?}");
        }
    }

    #[test]
    fn choices_keep_identified_items_scrub_names_and_sort_by_name() {
        let listed = json!([
            {"id": ID, "name": "zeta\u{1b}[2J"},
            {"id": "22222222-2222-4222-8222-222222222222", "name": "Alpha"},
            {"id": "33333333-3333-4333-8333-333333333333"},
            {"id": "not-a-uuid", "name": "skipped"},
            {"name": "no id"}
        ]);
        let labels: Vec<String> = choices(&listed).iter().map(ToString::to_string).collect();
        assert_eq!(
            labels,
            [
                "-  33333333-3333-4333-8333-333333333333",
                "Alpha  22222222-2222-4222-8222-222222222222",
                &format!("zeta [2J  {ID}"),
            ]
        );
        assert_eq!(choices(&json!({"items": [{"id": ID}]})).len(), 1);
        assert!(choices(&json!({"unexpected": true})).is_empty());
    }
}
