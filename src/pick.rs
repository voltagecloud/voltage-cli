//! Interactive scope: when a command lacks the organization, environment, or wallet it needs,
//! a person at a terminal picks one instead of copying a UUID. The same pickers browse: a
//! wallet read without its wallet ID, and `organizations get` or `environments get` without a
//! selected one, pick it and show it. Scripts never see a prompt.

use crate::{
    Error, Result, api,
    api::{SubmissionState, profile_offer_interrupted},
    auth::{ApiCredential, Discovery, Resolver},
    cli::{
        BodySource, Command, DiscoveryCommand, GlobalFlags, LocalCommand, ProfileCommand,
        missing_wallet,
    },
    config::{Credential, Profile, Scope, Settings, shell_word},
    error::ErrorKind,
    output::OutputFormat,
    registry::{Method, OperationId, ParameterLocation, ScopeParameter},
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
        // `--all-orgs` lists every organization, so it needs none.
        Command::Api(api) if api.all_orgs => Needs::default(),
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
                    || (friendly && operation.id == OperationId::CreatePayment)
                    || (operation.targets_wallet() && api.resource_id.is_none()),
            )
        }
        Command::Local(LocalCommand::Organizations {
            command: DiscoveryCommand::Get,
        }) => Needs::new(true, false, false),
        Command::Local(LocalCommand::Environments { command }) => {
            Needs::new(true, *command == DiscoveryCommand::Get, false)
        }
        Command::Local(LocalCommand::Profiles {
            command: ProfileCommand::Create { .. },
        }) => Needs::new(true, true, false),
        // Listed by name so that a new local command must decide what scope it needs.
        Command::Local(
            LocalCommand::Login(_)
            | LocalCommand::Logout(_)
            | LocalCommand::Auth { .. }
            | LocalCommand::Organizations {
                command: DiscoveryCommand::List,
            }
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
/// environment, wallet. A wallet read without its own wallet asks for that wallet last and
/// stops when none is picked. Without an interactive terminal, with `--no-input`, or for JSON
/// output, nothing is asked and the command reports what is missing as before. Esc, an empty
/// list, or a credential that cannot list choices also leaves the rest of the scope unset.
pub async fn fill(
    command: &mut Command,
    global: &GlobalFlags,
    resolver: Resolver<'_>,
    scope: &mut Scope,
) -> Result<()> {
    let can_ask = can_ask(global);
    let target = wallet_target(command, can_ask)?;
    let needs = needs(command);
    let missing = Needs {
        org: needs.org && scope.org.is_none(),
        env: needs.env && scope.envs.is_empty(),
        // The wallet a wallet command acts on never comes from the scope.
        wallet: needs.wallet && (target || scope.wallet.is_none()),
    };
    let terminal = global.terminal();
    if missing == Needs::default() || !can_ask {
        return Ok(());
    }
    let picked = ask(missing, global, resolver, scope).await?;
    if !picked.flags.is_empty() {
        let profile = match command {
            Command::Local(LocalCommand::Profiles { .. }) => "",
            _ => " or save a profile with voltage profiles create",
        };
        terminal.notice(format!(
            "Next time, pass {}{profile}.",
            picked.flags.join(" ")
        ));
    }
    match command {
        Command::Api(api) if target => {
            api.resource_id = picked.wallet;
            // Esc, or nothing to pick, leaves the command without its wallet.
            match api.resource_id {
                Some(_) => Ok(()),
                None => Err(missing_wallet(api.operation)),
            }
        }
        _ => {
            scope.wallet = picked.wallet.or(scope.wallet);
            Ok(())
        }
    }
}

/// Whether the command is a wallet read that names no wallet, which a terminal picks. A
/// wallet change without one, or a read off a terminal, stops here, before any credential is
/// resolved or request is sent.
fn wallet_target(command: &Command, can_ask: bool) -> Result<bool> {
    let Command::Api(api) = command else {
        return Ok(false);
    };
    if !api.operation.targets_wallet() || api.resource_id.is_some() {
        return Ok(false);
    }
    if !can_ask || api.operation.method != Method::Get {
        return Err(missing_wallet(api.operation));
    }
    Ok(true)
}

/// Whether a missing scope is picked rather than reported: only for table output on an
/// interactive terminal without `--no-input`.
pub fn can_ask(global: &GlobalFlags) -> bool {
    global.output_format() == OutputFormat::Table && global.terminal().can_pick()
}

/// After `voltage login` on a terminal, offer to save a profile that pins the login to an
/// organization and environment, so later commands need only `--profile NAME`. Nothing is
/// offered off a terminal, or when a profile already uses this login. The login is already
/// saved, so a failure here is a notice and never fails the login. Ctrl-C ends the command,
/// and its error says that the login is saved.
pub async fn offer_profile(
    settings: &mut Settings,
    account: &str,
    global: &GlobalFlags,
    submission: &SubmissionState,
) -> Result<Option<String>> {
    let used = settings
        .config
        .profiles
        .values()
        .any(|profile| profile.account == account);
    if used || !can_ask(global) {
        return Ok(None);
    }
    let terminal = global.terminal();
    terminal.notice(
        "Save a profile for this login, so later commands need only --profile NAME. Press Esc to skip.",
    );
    let later = format!(
        "Create one later with `voltage profiles create NAME --account {}`.",
        shell_word(account)
    );
    let skipped = |why: String| terminal.notice(format!("{why}{later}"));
    // Ctrl-C from here on, at a prompt or during a request, reports the saved login.
    submission.offering_profile(Some(account));
    let pinned = pin(settings, account, global, submission).await;
    submission.offering_profile(None);
    match pinned {
        Ok(Some(name)) => {
            terminal.notice(format!(
                "Saved profile {name}. Use it with --profile {name}."
            ));
            Ok(Some(name))
        }
        Ok(None) => {
            skipped("No profile was saved. ".into());
            Ok(None)
        }
        Err(error) if error.kind == ErrorKind::Interrupted => {
            Err(profile_offer_interrupted(account))
        }
        Err(error) => {
            skipped(format!("No profile was saved: {}. ", error.message));
            Ok(None)
        }
    }
}

/// Pick the organization and environment, taking each when it is the only one, then ask for
/// the profile's name and save it. `None` means Esc.
async fn pin(
    settings: &mut Settings,
    account: &str,
    global: &GlobalFlags,
    submission: &SubmissionState,
) -> Result<Option<String>> {
    let mut scope = Scope {
        account: Some(account.to_owned()),
        ..Scope::default()
    };
    let resolver = Resolver {
        settings,
        flags: global,
        submission,
        credential_sources: None,
    };
    ask(Needs::new(true, true, false), global, resolver, &mut scope).await?;
    let (Some(organization_id), [environment_id]) = (scope.org, scope.envs.as_slice()) else {
        return Ok(None);
    };
    let Some(name) = global.terminal().text("Profile name:", "default").await? else {
        return Ok(None);
    };
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(Error::usage("A profile name cannot be empty"));
    }
    let profile = Profile {
        organization_id,
        environment_id: *environment_id,
        account: account.to_owned(),
    };
    // The prompts can stay open for a long time, so the configuration is re-read under the
    // lock before the profile is added.
    settings
        .create_profile(&name, global.terminal(), |_| Ok(profile))
        .await?;
    Ok(Some(name))
}

/// What the pickers chose: the flags that select it again, and the wallet, which the caller
/// places in the scope or as the command's own wallet.
#[derive(Default)]
struct Picked {
    flags: Vec<String>,
    wallet: Option<Uuid>,
}

/// The pickers themselves, with one credential resolution and at most one organization token
/// exchange. An API key cannot list organizations or environments, so only its wallets are
/// offered. The organization and environment go into `scope`. Esc or an empty list can cut
/// the picks short.
async fn ask(
    missing: Needs,
    global: &GlobalFlags,
    resolver: Resolver<'_>,
    scope: &mut Scope,
) -> Result<Picked> {
    let terminal = global.terminal();
    // Without a credential there is nothing to list. That, not the missing scope, is what the
    // person must fix first, so report it.
    let credential = resolver.resolve(scope).await?;
    let mut picked = Picked::default();
    if missing.org {
        let Credential::Login(login) = &credential else {
            terminal.notice("An API key cannot list organizations, so there is nothing to pick.");
            return Ok(picked);
        };
        let listed = resolver
            .list(scope, Discovery::Organizations, &login.access_token)
            .await?;
        let Some(org) = choose(terminal, "Organization:", "organizations", &listed, true).await?
        else {
            return Ok(picked);
        };
        scope.org = Some(org);
        picked.flags.push(format!("--org {org}"));
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
        let Some(env) = choose(terminal, "Environment:", "environments", &listed, true).await?
        else {
            return Ok(picked);
        };
        scope.envs = vec![env];
        picked.flags.push(format!("--env {env}"));
    }
    if missing.wallet {
        let listed = api::wallets(global, credential, resolver.credential_sources, scope).await?;
        let Some(wallet) = choose(terminal, "Wallet:", "wallets", &listed, false).await? else {
            return Ok(picked);
        };
        picked.wallet = Some(wallet);
        picked.flags.push(format!("--wallet {wallet}"));
    }
    Ok(picked)
}

/// Pick from a listing, or say why there is nothing to pick. With `take_only`, the only
/// choice is taken without asking. A wallet is always asked for, because a payment spends
/// from it.
async fn choose(
    terminal: Terminal,
    question: &'static str,
    plural: &str,
    listed: &Value,
    take_only: bool,
) -> Result<Option<Uuid>> {
    let mut choices = choices(listed);
    if choices.is_empty() {
        terminal.notice(format!("No {plural} to choose from."));
        return Ok(None);
    }
    if take_only && choices.len() == 1 {
        let only = choices.remove(0);
        terminal.notice(format!("{question} {only} (the only one)"));
        return Ok(Some(only.id()));
    }
    terminal.pick(question, choices).await
}

/// The listed resources that carry a UUID `id`, sorted by name. A wallet also shows its
/// currency when all of its balances report the same one.
fn choices(listed: &Value) -> Vec<Choice> {
    let mut choices: Vec<Choice> = items(listed)
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.parse().ok()?;
            let name = item.get("name").and_then(Value::as_str).unwrap_or("-");
            let choice = Choice::new(id, name);
            Some(match currency(item) {
                Some(currency) => choice.with_currency(currency),
                None => choice,
            })
        })
        .collect();
    choices.sort_by_key(|choice| choice.name().to_lowercase());
    choices
}

/// A listing's items: the response itself when it is an array, else its `items`.
pub fn items(listed: &Value) -> &[Value] {
    listed
        .as_array()
        .or_else(|| listed.get("items").and_then(Value::as_array))
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// The one currency a wallet's balances report, or `None` when they report none or disagree.
fn currency(item: &Value) -> Option<&str> {
    let mut currencies = item
        .get("balances")?
        .as_array()?
        .iter()
        .map(|balance| balance.get("currency").and_then(Value::as_str));
    let first = currencies.next()??;
    currencies
        .all(|currency| currency == Some(first))
        .then_some(first)
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
            needs_of(&["wallets", "list", "--all-orgs"]),
            Needs::default()
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
        // A typed target is not picked; a missing wallet target is.
        assert_eq!(
            needs_of(&["wallets", "get", ID]),
            Needs::new(true, false, false)
        );
        assert_eq!(needs_of(&["wallets", "get"]), Needs::new(true, false, true));
        assert_eq!(
            needs_of(&["organizations", "get"]),
            Needs::new(true, false, false)
        );
        assert_eq!(
            needs_of(&["environments", "get"]),
            Needs::new(true, true, false)
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
    fn only_a_wallet_read_on_a_terminal_picks_its_missing_wallet() {
        let target = |args: &[&str], can_ask: bool| {
            let mut full = vec!["voltage"];
            full.extend_from_slice(args);
            wallet_target(&Invocation::try_parse_from(full).unwrap().command, can_ask)
        };
        assert!(target(&["wallets", "get"], true).unwrap());
        assert!(target(&["wallets", "policies", "get"], true).unwrap());
        // A named wallet, or a command that acts on something else, needs no pick.
        assert!(!target(&["wallets", "get", ID], true).unwrap());
        assert!(!target(&["wallets", "get", "--wallet", ID], false).unwrap());
        assert!(!target(&["wallets", "list"], false).unwrap());
        assert!(!target(&["organizations", "get"], false).unwrap());
        // A change always names its wallet, and a script is told how.
        for (args, can_ask) in [
            (&["wallets", "get"][..], false),
            (&["wallets", "delete"], true),
            (&["wallets", "update", "--name", "a"], true),
        ] {
            let error = target(args, can_ask).unwrap_err();
            assert_eq!(error.kind, ErrorKind::Usage, "{args:?}");
            assert!(error.message.ends_with("needs a wallet ID"), "{args:?}");
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

    #[test]
    fn wallet_choices_show_a_currency_only_when_their_balances_agree() {
        let wallet = |balances: Value| json!([{"id": ID, "name": "Payroll", "balances": balances}]);
        let label = |listed: Value| choices(&listed)[0].to_string();
        let usd = json!({"currency": "usd", "available": 1});
        assert_eq!(
            label(wallet(json!([usd, usd]))),
            format!("Payroll  USD  {ID}")
        );
        assert_eq!(
            label(wallet(json!([{"currency": "btc\u{1b}[2J"}]))),
            format!("Payroll  BTC [2J  {ID}")
        );
        for balances in [
            json!([]),
            json!([usd, {"currency": "btc"}]),
            json!([usd, {"available": 1}]),
            json!([{"available": 1}]),
            json!("usd"),
        ] {
            assert_eq!(
                label(wallet(balances.clone())),
                format!("Payroll  {ID}"),
                "{balances}"
            );
        }
    }
}
