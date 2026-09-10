use crate::registry::{OPERATIONS, Operation, query_flag};
use clap::{Arg, ArgAction, ArgGroup, ArgMatches, Command};
use std::collections::BTreeSet;

fn text_arg(name: &str, help: &str) -> Arg {
    Arg::new(name.to_owned())
        .long(name.to_owned())
        .help(help.to_owned())
}
fn flag(name: &str, help: &str) -> Arg {
    text_arg(name, help).action(ArgAction::SetTrue)
}

pub fn command() -> Command {
    let mut root = Command::new("voltage")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Manage wallets, payments, and more with the Voltage API")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .arg(
            text_arg(
                "profile",
                "Use this named profile as a complete configuration",
            )
            .global(true),
        )
        .arg(text_arg("account", "Select a saved credential").global(true))
        .arg(text_arg("org", "Organization UUID").global(true))
        .arg(
            text_arg(
                "env",
                "Environment UUID (repeat for supported list filters)",
            )
            .action(ArgAction::Append)
            .global(true),
        )
        .arg(text_arg("wallet", "Wallet UUID or wallet filter").global(true))
        .arg(text_arg("webhook", "Parent webhook UUID").global(true))
        .arg(flag("json", "Write a stable JSON result envelope").global(true))
        .arg(
            text_arg("output", "Output format")
                .value_parser(["table", "json", "ndjson"])
                .global(true),
        )
        .arg(
            text_arg(
                "output-file",
                "Save the complete response to a new owner-only file",
            )
            .global(true),
        )
        .arg(flag("show-secrets", "Explicitly allow secrets in result output").global(true))
        .arg(
            flag("yes", "Approve consequential actions without prompting")
                .short('y')
                .global(true),
        )
        .arg(
            text_arg("timeout", "HTTP or wait deadline in seconds")
                .default_value("60")
                .value_parser(clap::value_parser!(u64).range(1..))
                .global(true),
        )
        .arg(
            text_arg(
                "config-dir",
                "Configuration directory (default: ~/.config/voltage)",
            )
            .global(true),
        )
        .arg(text_arg("api-url", "Explicit API base URL; never persisted").global(true))
        .arg(text_arg("auth-url", "Explicit auth base URL; never persisted").global(true));
    root = add_children(root, &[]);
    root = root.subcommand(
        Command::new("login")
            .about("Sign in through your browser")
            .arg(flag(
                "no-browser",
                "Print the approval URL without opening it",
            ))
            .arg(
                text_arg("credential-store", "Where to save credentials")
                    .value_parser(["keychain", "file"])
                    .default_value("keychain"),
            ),
    );
    root = root.subcommand(
        Command::new("logout")
            .about("Revoke and remove the selected CLI login")
            .arg(flag(
                "local",
                "Remove local credentials without server revocation",
            )),
    );
    root = root.subcommand(
        Command::new("auth")
            .subcommand_required(true)
            .subcommand(
                Command::new("status").about("Show credential identity and expiry without secrets"),
            )
            .subcommand(
                Command::new("import-key")
                    .about("Save an API key from hidden input or stdin")
                    .arg(flag("stdin", "Read the API key from stdin"))
                    .arg(
                        text_arg("credential-store", "Where to save credentials")
                            .value_parser(["keychain", "file"])
                            .default_value("keychain"),
                    ),
            ),
    );
    root = root
        .subcommand(
            Command::new("organizations")
                .subcommand_required(true)
                .subcommand(Command::new("list")),
        )
        .subcommand(
            Command::new("environments")
                .subcommand_required(true)
                .subcommand(Command::new("list")),
        );
    let mut profiles = Command::new("profiles").subcommand_required(true);
    for action in ["create", "get", "delete", "list"] {
        let mut cmd = Command::new(action);
        if action != "list" {
            cmd = cmd.arg(Arg::new("name").required(true));
        }
        profiles = profiles.subcommand(cmd);
    }
    root.subcommand(profiles).subcommand(
        Command::new("completions").arg(
            Arg::new("shell")
                .required(true)
                .value_parser(clap::value_parser!(clap_complete::Shell)),
        ),
    )
}

fn add_children(mut parent: Command, prefix: &[String]) -> Command {
    let names: BTreeSet<_> = OPERATIONS
        .iter()
        .filter(|o| o.command.starts_with(prefix))
        .filter_map(|o| o.command.get(prefix.len()).cloned())
        .collect();
    for name in names {
        let mut path = prefix.to_vec();
        path.push(name.clone());
        let cmd = if let Some(op) = OPERATIONS.iter().find(|o| o.command == path) {
            endpoint(Command::new(name), op)
        } else {
            add_children(
                Command::new(name)
                    .subcommand_required(true)
                    .arg_required_else_help(true),
                &path,
            )
        };
        parent = parent.subcommand(cmd);
    }
    if prefix == ["payments"] {
        for name in ["send", "receive"] {
            parent = parent.subcommand(endpoint(
                Command::new(name).about(format!("Create a {name} payment using friendly flags")),
                crate::registry::operation("create_payment"),
            ));
        }
    }
    parent
}

fn endpoint(mut cmd: Command, op: &Operation) -> Command {
    cmd = cmd.about(op.description.clone());
    if !op.path.contains("{environment_id}")
        && !op
            .parameters
            .iter()
            .any(|p| p.name.starts_with("environment_id"))
    {
        cmd = cmd.after_help(if op.path.contains("{wallet_id}") {
            "Wallets have organization scope. --env validates the wallet on reads and mutations; it does not change this endpoint's scope."
        } else {
            "This endpoint has no environment filter. An environment in a profile does not narrow this operation."
        });
    }
    if let Some(target) = &op.target {
        cmd = cmd.arg(
            Arg::new("resource-id")
                .required(true)
                .value_name(target.to_uppercase()),
        );
    }
    let mut used: BTreeSet<String> = ["env", "wallet", "webhook"]
        .into_iter()
        .map(String::from)
        .collect();
    for param in op
        .parameters
        .iter()
        .filter(|p| p.location == "query" && p.name != "stream_token")
    {
        let name = query_flag(&param.name);
        if !used.insert(name.clone()) {
            continue;
        }
        let mut arg = text_arg(&name, &param.description).action(ArgAction::Append);
        if param.schema["type"] == "boolean"
            || param.schema["type"]
                .as_array()
                .is_some_and(|types| types.iter().any(|t| t == "boolean"))
        {
            arg = arg
                .num_args(0..=1)
                .default_missing_value("true")
                .value_parser(["true", "false"]);
        }
        cmd = cmd.arg(arg);
    }
    cmd = cmd.arg(
        text_arg(
            "query",
            "Additional documented query parameter, NAME=VALUE; repeatable",
        )
        .action(ArgAction::Append),
    );
    if op.parameters.iter().any(|p| p.name == "limit") {
        cmd = cmd.arg(flag(
            "all",
            "Fetch all pages; NDJSON streams page envelopes",
        ));
    }
    if op.auth == "checkout_session" || op.auth == "checkout_stream" {
        cmd = cmd.arg(text_arg(
            "token-file",
            "Read the checkout credential from a file or - for stdin",
        ));
    }
    if op.auth.starts_with("checkout") || op.id == "create_event_stream_token" {
        cmd = cmd.arg(text_arg("origin", "Exact checkout browser origin"));
    }
    if op.id == "create_payment" || op.id == "create_treasury_movement" || op.id == "get_payment" {
        cmd = cmd.arg(
            text_arg("wait", "Wait for invoice readiness or payment completion")
                .value_parser(["ready", "completed"]),
        );
    }
    if op.body {
        cmd = cmd.arg(
            text_arg("data", "Complete JSON object from @file or - for stdin")
                .value_name("@FILE|-"),
        );
        let fields = friendly_fields(&op.id);
        for (name, help) in &fields {
            if used.contains(*name) {
                continue;
            }
            let mut arg = text_arg(name, help);
            if op.id == "create_wallet" && *name == "network" {
                arg = arg.value_parser(["mainnet", "testnet3", "mutinynet"]);
            }
            if ["metadata", "event"].contains(name) {
                arg = arg.action(ArgAction::Append);
            }
            cmd = cmd.arg(arg);
        }
        if !fields.is_empty() {
            cmd = cmd.group(
                ArgGroup::new("body-flags")
                    .args(fields.iter().map(|(n, _)| *n))
                    .multiple(true)
                    .conflicts_with("data"),
            );
        }
    }
    cmd
}

fn friendly_fields(id: &str) -> Vec<(&'static str, &'static str)> {
    match id {
        "create_wallet" => vec![
            ("id", "Optional new resource UUID"),
            ("name", "Wallet name"),
            ("credit-line", "Backing line of credit UUID"),
            ("network", "Explicit wallet network"),
            (
                "limit",
                "Credit limit in the line of credit's integer base units",
            ),
            ("metadata", "KEY=VALUE, repeatable"),
        ],
        "update_wallet" => vec![("name", "New wallet name")],
        "create_payment" => vec![
            ("id", "Optional payment UUID"),
            (
                "currency",
                "Wallet currency for sends, receive currency for any-amount receives",
            ),
            ("invoice", "BOLT11 invoice to pay"),
            ("address", "On-chain address to pay"),
            ("kind", "Receive kind: bolt11, onchain, bip21"),
            ("amount", "Exact decimal amount; pair with --unit"),
            ("unit", "msats, sats, btc, cents, usd"),
            (
                "max-fee",
                "Maximum network/provider fee; processing fees are additional",
            ),
            ("fee-unit", "Fee unit: msats, sats, btc"),
            ("quote", "Quote UUID required by USD payment flows"),
            ("description", "Payment description"),
            ("expiration", "Receive expiration in seconds"),
            ("metadata", "KEY=VALUE, repeatable"),
        ],
        "request_a_quote" => vec![
            ("id", "Optional quote UUID"),
            ("credit-line", "Line of credit UUID"),
            ("network", "Explicit network"),
            ("amount", "Exact decimal amount"),
            ("unit", "msats, sats, btc, cents, usd"),
            ("to", "Target currency: btc or usd"),
        ],
        "create_webhook" => vec![
            ("id", "Optional webhook UUID"),
            ("name", "Webhook name"),
            ("url", "Delivery URL"),
            (
                "event",
                "Event selection such as receive.completed; repeatable",
            ),
        ],
        "update_webhook" => vec![(
            "event",
            "Complete replacement event selection such as receive.completed; repeatable",
        )],
        _ => vec![],
    }
}

pub fn leaf(matches: &ArgMatches) -> (Vec<String>, &ArgMatches) {
    let mut path = Vec::new();
    let mut leaf = matches;
    while let Some((name, sub)) = leaf.subcommand() {
        path.push(name.into());
        leaf = sub;
    }
    (path, leaf)
}
pub fn value(m: &ArgMatches, name: &str) -> Option<String> {
    m.try_get_one::<String>(name).ok().flatten().cloned()
}
pub fn values(m: &ArgMatches, name: &str) -> Vec<String> {
    m.try_get_many::<String>(name)
        .ok()
        .flatten()
        .map(|v| v.cloned().collect())
        .unwrap_or_default()
}
pub fn enabled(m: &ArgMatches, name: &str) -> bool {
    m.try_get_one::<bool>(name)
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_tree_is_valid() {
        command().debug_assert();
    }
    #[test]
    fn raw_body_conflicts_with_friendly_fields() {
        assert!(
            command()
                .try_get_matches_from([
                    "voltage", "wallets", "create", "--data", "@a.json", "--name", "hello"
                ])
                .is_err()
        );
    }
    #[test]
    fn global_flags_work_after_subcommand() {
        let m = command()
            .try_get_matches_from([
                "voltage",
                "wallets",
                "list",
                "--profile",
                "staging",
                "--env",
                "a",
                "--env",
                "b",
            ])
            .unwrap();
        assert_eq!(values(leaf(&m).1, "env"), ["a", "b"]);
    }
}
