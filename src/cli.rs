//! Command tree and typed argument parsing.
//!
//! Local commands are clap derive types. API commands are generated from the operation
//! registry so help, completions, and dispatch share one source; their options are derive
//! types attached only where an operation supports them, and their documented filters are
//! validated against the contract's value sets. This is the only module that reads
//! `ArgMatches`; everything downstream receives an `Invocation`.

use crate::{
    Error, Result,
    config::{CredentialStore, InputSource, ScopeSelection},
    output::OutputFormat,
    payment::{AmountUnit, Currency, Network, PaymentDirection, ReceiveKind, WaitTarget},
    price::ConversionRequest,
    registry::{Method, OPERATIONS, Operation, OperationId, Parameter, ResourceTarget, operation},
    terminal::Terminal,
};
use clap::{
    Arg, ArgAction, ArgMatches, Args, Command as ClapCommand, FromArgMatches, Subcommand,
    builder::{PossibleValuesParser, TypedValueParser},
    error::ErrorKind,
};
use clap_complete::Shell;
use std::{collections::BTreeSet, ffi::OsStr, path::PathBuf, time::Duration};
use uuid::Uuid;

/// Friendly payment commands beside `create_payment`'s generated command.
const PAYMENT_ALIASES: [(&str, PaymentDirection); 2] = [
    ("send", PaymentDirection::Send),
    ("receive", PaymentDirection::Receive),
];

/// Options accepted before or after any subcommand.
#[derive(Debug, Args)]
pub struct GlobalFlags {
    /// Use this named profile as a complete configuration
    #[arg(long, global = true, help_heading = "Scope")]
    pub profile: Option<String>,
    /// Select a saved credential
    #[arg(long, global = true, help_heading = "Scope")]
    pub account: Option<String>,
    /// Organization UUID
    #[arg(long, global = true, value_name = "UUID", help_heading = "Scope")]
    pub org: Option<Uuid>,
    /// Environment UUID (repeat for supported list filters)
    #[arg(long, global = true, value_name = "UUID", action = ArgAction::Append, help_heading = "Scope")]
    pub env: Vec<Uuid>,
    /// Wallet UUID or wallet filter
    #[arg(long, global = true, value_name = "UUID", help_heading = "Scope")]
    pub wallet: Option<Uuid>,
    /// Parent webhook UUID
    #[arg(long, global = true, value_name = "UUID", help_heading = "Scope")]
    pub webhook: Option<Uuid>,
    /// Write a stable JSON result envelope
    #[arg(long, global = true, help_heading = "Output")]
    pub json: bool,
    /// Output format
    #[arg(short = 'o', long, global = true, value_enum, help_heading = "Output")]
    pub output: Option<OutputFormat>,
    /// Save the complete response to a new owner-only file
    #[arg(long, global = true, value_name = "PATH", help_heading = "Output")]
    pub output_file: Option<PathBuf>,
    /// Explicitly allow secrets in result output
    #[arg(long, global = true, help_heading = "Output")]
    pub show_secrets: bool,
    /// Never prompt for a secret; supply it with --stdin or the environment instead
    #[arg(long, global = true, help_heading = "Safety")]
    pub no_input: bool,
    /// Hide optional progress and notices, not errors, results, or recovery IDs
    #[arg(short = 'q', long, global = true, help_heading = "Output")]
    pub quiet: bool,
    /// HTTP or wait deadline in seconds
    #[arg(long, global = true, value_name = "SECONDS", default_value = "60", value_parser = parse_timeout, help_heading = "Request")]
    pub timeout: Duration,
    /// Configuration directory (default: VOLTAGE_CONFIG_DIR, else $XDG_CONFIG_HOME/voltage, else ~/.config/voltage)
    #[arg(
        long,
        global = true,
        value_name = "DIR",
        help_heading = "Configuration"
    )]
    pub config_dir: Option<PathBuf>,
    /// Explicit API base URL; never persisted
    #[arg(
        long,
        global = true,
        value_name = "URL",
        help_heading = "Configuration"
    )]
    pub api_url: Option<String>,
    /// Explicit auth base URL; never persisted
    #[arg(
        long,
        global = true,
        value_name = "URL",
        help_heading = "Configuration"
    )]
    pub auth_url: Option<String>,
    /// Explicit price service base URL; never persisted
    #[arg(
        long,
        global = true,
        value_name = "URL",
        help_heading = "Configuration"
    )]
    pub price_url: Option<String>,
}

impl GlobalFlags {
    pub fn output_format(&self) -> OutputFormat {
        OutputFormat::select(self.json, self.output)
    }

    pub fn terminal(&self) -> Terminal {
        Terminal::new(self.quiet, self.no_input)
    }

    pub fn scope_selection(&self) -> ScopeSelection {
        ScopeSelection {
            profile: self.profile.clone(),
            account: self.account.clone(),
            org: self.org,
            envs: self.env.clone(),
            wallet: self.wallet,
            webhook: self.webhook,
        }
    }
}

/// Commands that manage local state or the login session rather than call the API.
#[derive(Debug, Subcommand)]
pub enum LocalCommand {
    /// Sign in through your browser
    Login(LoginFlags),
    /// Revoke and remove the selected CLI login
    Logout(LogoutFlags),
    /// Inspect and import credentials
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Organizations visible to the login
    Organizations {
        #[command(subcommand)]
        command: ListCommand,
    },
    /// Environments in the selected organization
    Environments {
        #[command(subcommand)]
        command: ListCommand,
    },
    /// Named organization, environment, and credential selections
    Profiles {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Show the effective profile, scope, credential, and config path without secrets
    Context,
    /// Show the BTC/USD price and the sats one dollar buys
    Price(PriceFlags),
    /// Convert an amount between BTC and USD at the current or a past price
    Convert(ConvertFlags),
    /// Print shell completions
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Debug, Args)]
pub struct PriceFlags {
    /// Price at this UTC time, such as 2026-09-18T17:30:00Z, instead of now
    #[arg(long, value_name = "RFC3339")]
    pub at: Option<String>,
}

#[derive(Debug, Args)]
pub struct ConvertFlags {
    /// Exact decimal amount
    pub amount: String,
    /// Unit of the amount
    #[arg(value_enum)]
    pub unit: AmountUnit,
    /// Currency to convert into
    #[arg(long, value_enum)]
    pub to: Currency,
    /// Price at this UTC time, such as 2026-09-18T17:30:00Z, instead of now
    #[arg(long, value_name = "RFC3339")]
    pub at: Option<String>,
}

impl ConvertFlags {
    pub fn request(&self) -> ConversionRequest {
        ConversionRequest {
            amount: self.amount.clone(),
            unit: self.unit,
            to: self.to,
        }
    }
}

#[derive(Debug, Args)]
pub struct LoginFlags {
    /// Print the approval URL without opening it
    #[arg(long)]
    pub no_browser: bool,
    /// Where to save credentials; by default the keychain, or the store of the login it replaces
    #[arg(long, value_enum)]
    pub credential_store: Option<CredentialStore>,
}

#[derive(Debug, Args)]
pub struct LogoutFlags {
    /// Remove local credentials without server revocation
    #[arg(long)]
    pub local: bool,
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Show credential identity and expiry without secrets
    Status,
    /// Save an API key from hidden input or stdin
    ImportKey(ImportKeyFlags),
}

#[derive(Debug, Args)]
pub struct ImportKeyFlags {
    /// Read the API key from stdin
    #[arg(long)]
    pub stdin: bool,
    /// Where to save credentials
    #[arg(long, value_enum, default_value_t = CredentialStore::Keychain)]
    pub credential_store: CredentialStore,
}

#[derive(Debug, Subcommand)]
pub enum ListCommand {
    List,
}

#[derive(Debug, Subcommand, Eq, PartialEq)]
pub enum ProfileCommand {
    /// Show every saved profile
    List,
    /// Show one profile
    Get { name: String },
    /// Save the current scope and credential selection under a name
    Create { name: String },
    /// Remove a profile
    Delete { name: String },
}

/// A browser origin: scheme, host, and optional port only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Origin(String);

impl Origin {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn parse_origin(value: &str) -> std::result::Result<Origin, String> {
    let url = url::Url::parse(value).map_err(|_| "Invalid Origin".to_string())?;
    if url.origin().ascii_serialization() != value {
        return Err("Origin must contain only scheme, host, and optional port".into());
    }
    Ok(Origin(value.to_owned()))
}

fn parse_timeout(value: &str) -> std::result::Result<Duration, String> {
    match value.parse::<u64>() {
        Ok(seconds) if seconds >= 1 => Ok(Duration::from_secs(seconds)),
        _ => Err("expected a whole number of seconds, at least 1".into()),
    }
}

fn parse_token_source(value: &str) -> std::result::Result<InputSource, String> {
    Ok(match value {
        "-" => InputSource::Stdin,
        path => InputSource::File(path.into()),
    })
}

fn parse_data_source(value: &str) -> std::result::Result<InputSource, String> {
    match value {
        "-" => Ok(InputSource::Stdin),
        _ => value
            .strip_prefix('@')
            .map(|path| InputSource::File(path.into()))
            .ok_or_else(|| "Use --data @file or --data -".into()),
    }
}

/// One `--query NAME=VALUE` override.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryOverride {
    pub name: String,
    pub value: String,
}

fn parse_query_override(value: &str) -> std::result::Result<QueryOverride, String> {
    value
        .split_once('=')
        .map(|(name, value)| QueryOverride {
            name: name.into(),
            value: value.into(),
        })
        .ok_or_else(|| "--query requires NAME=VALUE".into())
}

/// Positional resource identifier for operations with a target. `--id` names the same
/// identifier, because `quotes create --id` and `payments send --id` take it as a flag.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct TargetFlags {
    #[arg(help_heading = "Arguments")]
    resource_id: Option<Uuid>,
    /// The same ID as the argument, given as a flag
    #[arg(long = "id", id = "target_id", value_name = "ID")]
    target_id: Option<Uuid>,
}

impl TargetFlags {
    fn id(&self) -> Option<Uuid> {
        self.resource_id.or(self.target_id)
    }
}

#[derive(Debug, Args)]
struct QueryFlags {
    /// Additional documented query parameter, NAME=VALUE; repeatable
    #[arg(long, value_name = "NAME=VALUE", action = ArgAction::Append, value_parser = parse_query_override, help_heading = "Query")]
    query: Vec<QueryOverride>,
}

#[derive(Debug, Args)]
struct PaginationFlags {
    /// Fetch all pages; NDJSON streams page envelopes
    #[arg(long, help_heading = "Query")]
    all: bool,
}

#[derive(Debug, Args)]
struct AllOrgsFlags {
    /// List the wallets in every organization the browser login can see
    #[arg(long, conflicts_with_all = ["org", "env"], help_heading = "Scope")]
    all_orgs: bool,
}

#[derive(Debug, Args)]
struct CheckoutFlags {
    /// Read the checkout credential from a file or - for stdin
    #[arg(long, value_name = "PATH|-", value_parser = parse_token_source, help_heading = "Authentication")]
    token_file: Option<InputSource>,
}

#[derive(Debug, Args)]
struct OriginFlags {
    /// Exact checkout browser origin
    #[arg(long, value_parser = parse_origin, help_heading = "Authentication")]
    origin: Option<Origin>,
}

#[derive(Debug, Args)]
struct WaitFlags {
    /// Wait for invoice readiness or payment completion
    #[arg(long, value_enum, help_heading = "Waiting and presentation")]
    wait: Option<WaitTarget>,
}

#[derive(Debug, Args)]
struct InvoiceFlags {
    /// Render a ready BOLT11 invoice as a compact terminal QR code; implies --wait ready
    #[arg(long, help_heading = "Waiting and presentation")]
    qr: bool,
    /// Copy a ready BOLT11 invoice to the clipboard; implies --wait ready
    #[arg(long, help_heading = "Waiting and presentation")]
    copy: bool,
}

#[derive(Debug, Args)]
struct ExecuteFlags {
    /// Send this change. Without it or VOLTAGE_EXECUTE=1, the change is only described
    #[arg(long, help_heading = "Safety")]
    execute: bool,
}

/// What the command line asked for: `--execute`, or nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Requested {
    Execute,
    Unspecified,
}

#[derive(Debug, Args)]
struct DataFlags {
    /// Complete JSON object from @file or - for stdin
    #[arg(long, value_name = "@FILE|-", value_parser = parse_data_source, help_heading = "Request body")]
    data: Option<InputSource>,
}

/// Friendly flags for `wallets create`.
#[derive(Debug, Args)]
#[command(next_help_heading = "Wallet")]
#[group(id = "body-flags", multiple = true, conflicts_with = "data")]
pub struct WalletFlags {
    /// Optional new resource UUID
    #[arg(long)]
    pub id: Option<Uuid>,
    /// Wallet name
    #[arg(long)]
    pub name: Option<String>,
    /// Backing line of credit UUID
    #[arg(long)]
    pub credit_line: Option<Uuid>,
    /// Explicit wallet network
    #[arg(long, value_enum)]
    pub network: Option<Network>,
    /// Credit limit in the line of credit's integer base units
    #[arg(long)]
    pub limit: Option<u64>,
    /// KEY=VALUE, repeatable
    #[arg(long, action = ArgAction::Append)]
    pub metadata: Vec<String>,
}

/// Friendly flags for `wallets update`.
#[derive(Debug, Args)]
#[command(next_help_heading = "Wallet")]
#[group(id = "body-flags", multiple = true, conflicts_with = "data")]
pub struct UpdateWalletFlags {
    /// New wallet name
    #[arg(long)]
    pub name: Option<String>,
}

/// Friendly flags for `payments create`, `payments send`, and `payments receive`.
#[derive(Debug, Args)]
#[command(next_help_heading = "Payment")]
#[group(id = "body-flags", multiple = true, conflicts_with = "data")]
pub struct PaymentFlags {
    /// Optional payment UUID
    #[arg(long)]
    pub id: Option<Uuid>,
    /// Wallet currency for sends, receive currency for any-amount receives. Defaults to the
    /// wallet's line of credit currency (reads the wallet)
    #[arg(long, value_enum)]
    pub currency: Option<Currency>,
    /// BOLT11 invoice to pay
    #[arg(long)]
    pub invoice: Option<String>,
    /// On-chain address to pay; on-chain sends need the on_chain feature enabled by Voltage
    #[arg(long)]
    pub address: Option<String>,
    /// Receive kind; onchain and bip21 need the on_chain and bip21 features enabled by Voltage
    #[arg(long, value_enum)]
    pub kind: Option<ReceiveKind>,
    /// Exact decimal amount, in --unit
    #[arg(long)]
    pub amount: Option<String>,
    /// Unit of --amount. Defaults to the currency's base unit: msats or cents
    #[arg(long, value_enum)]
    pub unit: Option<AmountUnit>,
    /// Maximum network/provider fee; processing fees are additional
    #[arg(long)]
    pub max_fee: Option<String>,
    /// Unit of --max-fee: msats, sats, or btc. Defaults to msats
    #[arg(long, value_enum)]
    pub fee_unit: Option<AmountUnit>,
    /// Quote UUID required by USD payment flows
    #[arg(long)]
    pub quote: Option<Uuid>,
    /// Payment description
    #[arg(long)]
    pub description: Option<String>,
    /// Receive expiration in seconds
    #[arg(long)]
    pub expiration: Option<u64>,
    /// KEY=VALUE, repeatable
    #[arg(long, action = ArgAction::Append)]
    pub metadata: Vec<String>,
}

/// Friendly flags for `quotes create`.
#[derive(Debug, Args)]
#[command(next_help_heading = "Quote")]
#[group(id = "body-flags", multiple = true, conflicts_with = "data")]
pub struct QuoteFlags {
    /// Optional quote UUID
    #[arg(long)]
    pub id: Option<Uuid>,
    /// Line of credit UUID
    #[arg(long)]
    pub credit_line: Option<Uuid>,
    /// Explicit network
    #[arg(long, value_enum)]
    pub network: Option<Network>,
    /// Exact decimal amount to convert
    #[arg(long)]
    pub amount: Option<String>,
    /// Unit of --amount; its currency is the one converted from
    #[arg(long, value_enum)]
    pub unit: Option<AmountUnit>,
    /// Currency to convert into: the other of btc and usd
    #[arg(long, value_enum)]
    pub to: Option<Currency>,
}

/// Friendly flags for `webhooks create`.
#[derive(Debug, Args)]
#[command(next_help_heading = "Webhook")]
#[group(id = "body-flags", multiple = true, conflicts_with = "data")]
pub struct WebhookFlags {
    /// Optional webhook UUID
    #[arg(long)]
    pub id: Option<Uuid>,
    /// Webhook name
    #[arg(long)]
    pub name: Option<String>,
    /// Delivery URL
    #[arg(long)]
    pub url: Option<String>,
    /// Event selection such as receive.completed; repeatable
    #[arg(long, action = ArgAction::Append)]
    pub event: Vec<String>,
}

/// Friendly flags for `webhooks update`.
#[derive(Debug, Args)]
#[command(next_help_heading = "Webhook")]
#[group(id = "body-flags", multiple = true, conflicts_with = "data")]
pub struct UpdateWebhookFlags {
    /// Complete replacement event selection such as receive.completed; repeatable
    #[arg(long, action = ArgAction::Append)]
    pub event: Vec<String>,
}

/// Body-building flags for the operations that offer them.
#[derive(Debug)]
pub enum FriendlyFlags {
    CreateWallet(WalletFlags),
    UpdateWallet(UpdateWalletFlags),
    Payment(PaymentFlags),
    Quote(QuoteFlags),
    CreateWebhook(WebhookFlags),
    UpdateWebhook(UpdateWebhookFlags),
}

impl FriendlyFlags {
    /// A create builds a fresh UUID for its body because `--id` was not given.
    pub fn generates_id(&self) -> bool {
        match self {
            Self::CreateWallet(flags) => flags.id.is_none(),
            Self::Payment(flags) => flags.id.is_none(),
            Self::Quote(flags) => flags.id.is_none(),
            Self::CreateWebhook(flags) => flags.id.is_none(),
            Self::UpdateWallet(_) | Self::UpdateWebhook(_) => false,
        }
    }
}

/// Where a body-bearing operation gets its payload.
#[derive(Debug)]
pub enum BodySource {
    /// A complete JSON object, preserved byte for byte.
    Raw(InputSource),
    Friendly(FriendlyFlags),
}

/// One documented query filter and the values its generated flag received.
#[derive(Debug)]
pub struct Filter {
    pub parameter: &'static Parameter,
    pub values: Vec<String>,
}

/// A registry operation with everything its command line supplied.
#[derive(Debug)]
pub struct ApiInvocation {
    pub operation: &'static Operation,
    /// `payments send` or `payments receive`, which build friendly bodies for `create_payment`.
    pub alias: Option<PaymentDirection>,
    pub resource_id: Option<Uuid>,
    pub filters: Vec<Filter>,
    pub query: Vec<QueryOverride>,
    pub all: bool,
    /// `wallets list --all-orgs`: every organization the login can see, not the scope's.
    pub all_orgs: bool,
    pub token_file: Option<InputSource>,
    pub origin: Option<Origin>,
    pub wait: Option<WaitTarget>,
    pub qr: bool,
    pub copy: bool,
    pub body: Option<BodySource>,
    pub requested: Requested,
}

impl ApiInvocation {
    /// `--qr` or `--copy` was requested.
    pub fn presents_invoice(&self) -> bool {
        self.qr || self.copy
    }

    /// The command words as typed: `payments send` rather than `payments create`.
    pub fn command_name(&self) -> String {
        let alias = PAYMENT_ALIASES
            .iter()
            .find(|(_, direction)| Some(*direction) == self.alias);
        match alias {
            Some((name, _)) => [
                payment_alias_parent(self.operation).join(" "),
                (*name).into(),
            ]
            .join(" "),
            None => self.operation.command.join(" "),
        }
    }
}

#[derive(Debug)]
pub enum Command {
    Local(LocalCommand),
    Api(Box<ApiInvocation>),
}

/// A fully parsed command line.
#[derive(Debug)]
pub struct Invocation {
    pub global: GlobalFlags,
    pub command: Command,
}

impl Invocation {
    /// Parse process arguments without letting clap terminate the process. Startup decides
    /// whether parse failures need clap's human rendering or the JSON error envelope.
    pub fn from_env() -> std::result::Result<Self, ParseFailure> {
        let matches = command().try_get_matches().map_err(ParseFailure::Clap)?;
        Self::from_matches(&matches).map_err(ParseFailure::Validation)
    }

    #[cfg(test)]
    pub fn try_parse_from<I, T>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let matches = command()
            .try_get_matches_from(args)
            .map_err(|error| Error::usage(error.to_string()))?;
        Self::from_matches(&matches)
    }

    fn from_matches(root: &ArgMatches) -> Result<Self> {
        let (path, leaf) = leaf(root);
        let global = parse::<GlobalFlags>(leaf)?;
        let command = match LocalCommand::from_arg_matches(root) {
            Ok(local) => Command::Local(local),
            Err(error) if error.kind() == ErrorKind::InvalidSubcommand => {
                let (operation, alias) =
                    resolve_operation(&path).ok_or_else(|| Error::usage("Unknown command"))?;
                Command::Api(Box::new(api_invocation(operation, alias, leaf)?))
            }
            Err(error) => return Err(Error::usage(error.to_string())),
        };
        Ok(Self { global, command })
    }
}

/// A command line can fail in clap itself or in validation of its typed matches.
#[derive(Debug)]
pub enum ParseFailure {
    Clap(clap::Error),
    Validation(Error),
}

/// `--json` affects parse-error rendering only when ordinary parsing cannot produce globals.
/// Stop at `--` so a positional value with that spelling is not treated as an option.
pub fn json_errors_requested<I, T>(args: I) -> bool
where
    I: IntoIterator<Item = T>,
    T: AsRef<OsStr>,
{
    args.into_iter()
        .skip(1)
        .take_while(|arg| arg.as_ref() != OsStr::new("--"))
        .any(|arg| arg.as_ref() == OsStr::new("--json"))
}

fn parse<T: FromArgMatches>(matches: &ArgMatches) -> Result<T> {
    T::from_arg_matches(matches).map_err(|error| Error::usage(error.to_string()))
}

/// Parse an option group only when the operation defines it.
fn parse_if<T: FromArgMatches>(present: bool, matches: &ArgMatches) -> Result<Option<T>> {
    present.then(|| parse(matches)).transpose()
}

/// The registry operation for a command path, or a payment alias's direction.
fn resolve_operation(path: &[String]) -> Option<(&'static Operation, Option<PaymentDirection>)> {
    if let Some(operation) = OPERATIONS
        .iter()
        .find(|operation| operation.command == path)
    {
        return Some((operation, None));
    }
    let create_payment = operation(OperationId::CreatePayment);
    let (last, parent) = path.split_last()?;
    if parent != payment_alias_parent(create_payment) {
        return None;
    }
    PAYMENT_ALIASES
        .iter()
        .find(|(name, _)| name == last)
        .map(|(_, direction)| (create_payment, Some(*direction)))
}

fn payment_alias_parent(create_payment: &Operation) -> &[String] {
    create_payment
        .command
        .split_last()
        .map(|(_, parent)| parent)
        .unwrap_or_default()
}

/// The operation's resource ID: its argument or `--id`. A wallet operation also takes an
/// explicit `--wallet`, which names the wallet everywhere else; a profile's or
/// `VOLTAGE_WALLET_ID`'s wallet never selects it, so a delete always names its wallet.
fn target_id(
    operation: &Operation,
    target: Option<TargetFlags>,
    m: &ArgMatches,
) -> Result<Option<Uuid>> {
    let id = target.and_then(|target| target.id());
    if !operation.targets_wallet() {
        return Ok(id);
    }
    match (id, m.get_one::<Uuid>("wallet").copied()) {
        (Some(id), Some(wallet)) if id != wallet => Err(Error::usage(format!(
            "WALLET_ID {id} and --wallet {wallet} name different wallets"
        ))
        .with_hint("Pass the wallet once, as WALLET_ID or --wallet.")),
        (Some(id), _) | (None, Some(id)) => Ok(Some(id)),
        (None, None) => {
            let command = operation.command.join(" ");
            Err(
                Error::usage(format!("{command} needs a wallet ID")).with_hint(format!(
                    "Pass it as an argument or with --wallet, as in `voltage {command} WALLET_ID`. `voltage wallets list` shows the IDs."
                )),
            )
        }
    }
}

fn api_invocation(
    operation: &'static Operation,
    alias: Option<PaymentDirection>,
    m: &ArgMatches,
) -> Result<ApiInvocation> {
    let filters = operation
        .query_filters()
        .filter(|parameter| parameter.scope().is_none())
        .map(|parameter| Filter {
            parameter,
            values: filter_values(parameter, m),
        })
        .filter(|filter| !filter.values.is_empty())
        .collect();
    let target = parse_if::<TargetFlags>(operation.target.is_some(), m)?;
    let query = parse::<QueryFlags>(m)?;
    let execute = parse_if::<ExecuteFlags>(operation.method != Method::Get, m)?;
    let pagination = parse_if::<PaginationFlags>(operation.has_parameter("limit"), m)?;
    let all_orgs = parse_if::<AllOrgsFlags>(operation.id.lists_all_organizations(), m)?;
    let checkout = parse_if::<CheckoutFlags>(operation.auth.is_checkout(), m)?;
    let origin = parse_if::<OriginFlags>(takes_origin(operation), m)?;
    let wait = parse_if::<WaitFlags>(operation.id.supports_wait(), m)?;
    let invoice = parse_if::<InvoiceFlags>(operation.id.presents_invoice(), m)?;
    let data = parse_if::<DataFlags>(operation.body, m)?;
    let body = match data {
        Some(DataFlags { data: Some(source) }) => Some(BodySource::Raw(source)),
        Some(DataFlags { data: None }) => {
            friendly_flags(operation.id, m)?.map(BodySource::Friendly)
        }
        None => None,
    };
    Ok(ApiInvocation {
        operation,
        alias,
        resource_id: target_id(operation, target, m)?,
        filters,
        query: query.query,
        all: pagination.is_some_and(|flags| flags.all),
        all_orgs: all_orgs.is_some_and(|flags| flags.all_orgs),
        token_file: checkout.and_then(|flags| flags.token_file),
        origin: origin.and_then(|flags| flags.origin),
        wait: wait.and_then(|flags| flags.wait),
        qr: invoice.as_ref().is_some_and(|flags| flags.qr),
        copy: invoice.as_ref().is_some_and(|flags| flags.copy),
        body,
        requested: if execute.is_some_and(|flags| flags.execute) {
            Requested::Execute
        } else {
            Requested::Unspecified
        },
    })
}

/// Values a documented filter received; booleans are re-spelled for the query string.
fn filter_values(parameter: &Parameter, m: &ArgMatches) -> Vec<String> {
    let flag = parameter.flag();
    if parameter.is_boolean() {
        m.try_get_many::<bool>(&flag)
            .ok()
            .flatten()
            .map(|values| values.map(bool::to_string).collect())
            .unwrap_or_default()
    } else {
        m.try_get_many::<String>(&flag)
            .ok()
            .flatten()
            .map(|values| values.cloned().collect())
            .unwrap_or_default()
    }
}

fn friendly_flags(id: OperationId, m: &ArgMatches) -> Result<Option<FriendlyFlags>> {
    Ok(Some(match id {
        OperationId::CreateWallet => FriendlyFlags::CreateWallet(parse(m)?),
        OperationId::UpdateWallet => FriendlyFlags::UpdateWallet(parse(m)?),
        OperationId::CreatePayment => FriendlyFlags::Payment(parse(m)?),
        OperationId::RequestAQuote => FriendlyFlags::Quote(parse(m)?),
        OperationId::CreateWebhook => FriendlyFlags::CreateWebhook(parse(m)?),
        OperationId::UpdateWebhook => FriendlyFlags::UpdateWebhook(parse(m)?),
        _ => return Ok(None),
    }))
}

/// Checkout operations and stream-token creation send a browser origin.
fn takes_origin(operation: &Operation) -> bool {
    operation.auth.is_checkout() || operation.id == OperationId::CreateEventStreamToken
}

/// The git ref of the docs that match this binary: its release tag for a clean release
/// build, else `master`. `build.rs` decides, with the same test as `--version`.
const DOCS_REF: &str = env!("VOLTAGE_DOCS_REF");

/// The root help's common commands and where the documentation starts.
const ROOT_HELP: &str = concat!(
    "Common commands:
  voltage login                     Sign in through your browser
  voltage context                   Show the profile, scope, and credential in use
  voltage payments receive --help   Create an invoice or address to get paid
  voltage payments send --help      Pay an invoice or address
  voltage webhooks create --help    Send payment events to your server

Changes are only described until you add --execute.
Documentation: https://github.com/voltagecloud/voltage-cli/tree/",
    env!("VOLTAGE_DOCS_REF"),
    "/docs"
);

/// The complete command tree, for parsing, help, and completions.
pub fn command() -> ClapCommand {
    let root = ClapCommand::new("voltage")
        .version(env!("VOLTAGE_VERSION"))
        .subcommand_required(true)
        .arg_required_else_help(true);
    let root = GlobalFlags::augment_args(root);
    let mut root = LocalCommand::augment_subcommands(root);
    for name in root
        .get_subcommands()
        .map(|sub| sub.get_name().to_owned())
        .collect::<Vec<_>>()
    {
        root = root.mut_subcommand(&name, |sub| describe_local(sub, &mut vec![name.clone()]));
    }
    // Derived types apply their own doc comments as `about`, so the command's text goes last.
    add_children(root, &[])
        .about("Manage wallets, payments, and more with the Voltage API")
        .after_help(ROOT_HELP)
}

/// Add every registry command below `prefix`, recursing through shared prefixes.
fn add_children(mut parent: ClapCommand, prefix: &[String]) -> ClapCommand {
    let names: BTreeSet<_> = OPERATIONS
        .iter()
        .filter(|operation| operation.command.starts_with(prefix))
        .filter_map(|operation| operation.command.get(prefix.len()).cloned())
        .collect();
    for name in names {
        let mut path = prefix.to_vec();
        path.push(name.clone());
        let cmd = match OPERATIONS
            .iter()
            .find(|operation| operation.command == path)
        {
            Some(operation) => endpoint(
                ClapCommand::new(name),
                operation,
                operation.description.clone(),
            ),
            None => add_children(
                ClapCommand::new(name)
                    .subcommand_required(true)
                    .arg_required_else_help(true),
                &path,
            )
            .about(group_about(&path))
            .after_long_help(learn_more(
                &path.iter().map(String::as_str).collect::<Vec<_>>(),
            )),
        };
        parent = parent.subcommand(cmd);
    }
    let create_payment = operation(OperationId::CreatePayment);
    if prefix == payment_alias_parent(create_payment) {
        for (name, _) in PAYMENT_ALIASES {
            parent = parent.subcommand(endpoint(
                ClapCommand::new(name),
                create_payment,
                format!("Create a {name} payment using friendly flags"),
            ));
        }
    }
    parent
}

/// The command for one operation: its positional target, documented filters, and options.
/// `--all` pages through a list that takes `--limit`. A list without paging accepts `--all`
/// too and ignores it, because its one response already holds every item.
fn list_args(cmd: ClapCommand, operation: &Operation) -> ClapCommand {
    if operation.has_parameter("limit") {
        PaginationFlags::augment_args(cmd)
    } else if operation.lists_everything_at_once() {
        cmd.arg(
            Arg::new("all_at_once")
                .long("all")
                .action(ArgAction::SetTrue)
                .help("No effect: this list returns every item in one response")
                .help_heading("Query"),
        )
    } else {
        cmd
    }
}

/// The resource ID argument and `--id`, one of which is required. A wallet command can
/// name its wallet with `--wallet` instead, so `target_id` checks that one was given.
fn target_args(cmd: ClapCommand, target: ResourceTarget, wallet: bool) -> ClapCommand {
    let cmd = TargetFlags::augment_args(cmd)
        .mut_arg("resource_id", |arg| arg.value_name(target.value_name()));
    if wallet {
        cmd.mut_group("TargetFlags", |group| group.required(false))
    } else {
        cmd
    }
}

fn endpoint(mut cmd: ClapCommand, operation: &Operation, about: String) -> ClapCommand {
    let command_name = cmd.get_name().to_owned();
    let mut notes = Vec::new();
    if operation.body {
        notes.push(
            "Request body:\n  Use either friendly flags or --data @FILE/--data - for a complete JSON object; they cannot be combined."
                .to_owned(),
        );
    }
    if let Some(requirements) = friendly_requirements(operation.id, &command_name) {
        notes.push(format!("Friendly flags:\n  {requirements}"));
    }
    if let Some(examples) = endpoint_examples(operation.id, &command_name) {
        notes.push(format!("Examples:\n{examples}"));
    }
    if !operation.is_environment_scoped() {
        notes.push(if operation.targets_wallet() {
            "Scope:\n  Wallets have organization scope. --env validates the wallet on reads and mutations; it does not change this endpoint's scope. --wallet ID names the wallet in place of WALLET_ID.".to_owned()
        } else {
            "Scope:\n  This endpoint has no environment filter. An environment in a profile does not narrow this operation.".to_owned()
        });
    }
    if let Some(feature) = operation.id.gated_feature() {
        notes.push(format!(
            "Feature requirement:\n  Voltage must enable the {} feature for the organization; otherwise the API rejects the request with feature_flag_disabled.",
            feature.as_str()
        ));
    }
    notes.push(learn_more(
        &operation
            .command
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    ));
    // `-h` stays short; `--help` adds the notes.
    cmd = cmd.after_long_help(notes.join("\n\n"));
    if let Some(target) = operation.target {
        cmd = target_args(cmd, target, operation.targets_wallet());
    }
    for parameter in operation
        .query_filters()
        .filter(|parameter| parameter.scope().is_none())
    {
        cmd = cmd.arg(filter_arg(parameter));
    }
    cmd = QueryFlags::augment_args(cmd);
    if operation.method != Method::Get {
        cmd = ExecuteFlags::augment_args(cmd);
    }
    cmd = list_args(cmd, operation);
    if operation.id.lists_all_organizations() {
        cmd = AllOrgsFlags::augment_args(cmd);
    }
    if operation.auth.is_checkout() {
        cmd = CheckoutFlags::augment_args(cmd);
    }
    if takes_origin(operation) {
        cmd = OriginFlags::augment_args(cmd);
    }
    if operation.id.supports_wait() {
        cmd = WaitFlags::augment_args(cmd);
    }
    if operation.id.presents_invoice() {
        cmd = InvoiceFlags::augment_args(cmd);
    }
    if operation.body {
        cmd = DataFlags::augment_args(cmd);
        cmd = match operation.id {
            OperationId::CreateWallet => WalletFlags::augment_args(cmd),
            OperationId::UpdateWallet => UpdateWalletFlags::augment_args(cmd),
            OperationId::CreatePayment => PaymentFlags::augment_args(cmd),
            OperationId::RequestAQuote => QuoteFlags::augment_args(cmd),
            OperationId::CreateWebhook => WebhookFlags::augment_args(cmd),
            OperationId::UpdateWebhook => UpdateWebhookFlags::augment_args(cmd),
            _ => cmd,
        };
    }
    // Derived option groups apply their own doc comments as `about`, so the text goes last.
    cmd.about(about)
}

/// Human descriptions for generated groups. Endpoint descriptions still come from the
/// contract, while this small vocabulary explains how related endpoints fit together.
fn group_about(path: &[String]) -> &'static str {
    let words: Vec<_> = path.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["assets"] => "List assets supported by an organization",
        ["bills"] => "Inspect billing statements and summaries",
        ["checkout"] => "Manage hosted checkout sessions, settings, and event streams",
        ["checkout", "events"] => "Watch hosted checkout events",
        ["checkout", "sessions"] => "Create and inspect hosted checkout sessions",
        ["checkout", "settings"] => "Inspect and update hosted checkout settings",
        ["checkout", "streams"] => "Create credentials for checkout event streams",
        ["credit-lines"] => "Inspect and manage lines of credit",
        ["payments"] => "Create, inspect, and summarize payments",
        ["quotes"] => "Create and inspect currency conversion quotes",
        ["treasury"] => "Move funds between treasury accounts",
        ["treasury", "movements"] => "Create treasury movements",
        ["wallets"] => "Create, inspect, and manage wallets",
        ["wallets", "ledger"] => "Inspect wallet ledger entries",
        ["wallets", "payments"] => "Summarize payments for a wallet",
        ["wallets", "policies"] => "Inspect and update wallet policies",
        ["webhooks"] => "Create, inspect, and manage webhooks",
        ["webhooks", "deliveries"] => "Inspect and retry webhook deliveries",
        ["webhooks", "keys"] => "Rotate webhook signing keys",
        _ => "Browse related Voltage API operations",
    }
}

/// Required friendly-flag combinations that clap cannot express as simple required options.
fn friendly_requirements(id: OperationId, command_name: &str) -> Option<&'static str> {
    match (id, command_name) {
        (OperationId::CreatePayment, "send") => Some(
            "Pass --wallet and exactly one of --invoice or --address; on-chain sends also need --amount. Without --currency, the wallet's currency is read. --amount is in the currency's base unit (msats or cents) unless --unit is set, and a USD wallet read this way needs --unit. --max-fee is in msats unless --fee-unit is set.",
        ),
        (OperationId::CreatePayment, "receive") => Some(
            "Pass --wallet and --kind. Without --currency, the wallet's currency is read, and --unit must be in it. --amount is in the currency's base unit (msats or cents) unless --unit is set, and a USD wallet read this way needs --unit.",
        ),
        (OperationId::CreateWallet, "create") => Some(
            "Select one environment with --env or --profile, then pass --name, --credit-line, --network, and --limit.",
        ),
        _ => None,
    }
}

/// Examples for every command that changes something, plus the reads whose flags are least
/// obvious. Each `# comment` line says what the command under it does.
fn endpoint_examples(id: OperationId, command_name: &str) -> Option<&'static str> {
    Some(match (id, command_name) {
        (OperationId::CreatePayment, "send") => {
            "  # Pay a Lightning invoice, paying at most 10 sats in fees
  voltage payments send --profile prod --wallet WALLET_ID --currency btc --invoice BOLT11_INVOICE --max-fee 10 --fee-unit sats --execute
  # Send 1,000 sats on-chain
  voltage payments send --profile prod --wallet WALLET_ID --currency btc --address BITCOIN_ADDRESS --amount 1000 --unit sats --execute"
        }
        (OperationId::CreatePayment, "receive") => {
            "  # Create a 1,000 sat Lightning invoice and print it when it is ready
  voltage payments receive --profile prod --wallet WALLET_ID --currency btc --kind bolt11 --amount 1000 --unit sats --wait ready --execute
  # Show the invoice as a QR code and wait until it is paid
  voltage payments receive --profile prod --wallet WALLET_ID --currency btc --kind bolt11 --amount 1000 --unit sats --qr --wait completed --execute"
        }
        (OperationId::CreatePayment, _) => {
            "  # Send a complete payment request body from a file
  voltage payments create --profile prod --data @payment.json --execute"
        }
        (OperationId::GetPayment, _) => {
            "  # Wait up to two minutes for a payment to settle
  voltage payments get PAYMENT_ID --profile prod --wait completed --timeout 120"
        }
        (OperationId::CheckPayment, _) => {
            "  # Check whether a payment would be accepted, without creating a payment
  voltage payments check --profile prod --data @payment.json --execute"
        }
        (OperationId::CreateTreasuryMovement, _) => {
            "  # Move funds between treasury accounts
  voltage treasury movements create --profile prod --data @movement.json --execute"
        }
        (OperationId::CreateWallet, _) => {
            "  # Create a wallet on a line of credit with a limit of 100,000 in its base units
  voltage wallets create --profile prod --name treasury --credit-line CREDIT_LINE_ID --network mutinynet --limit 100000 --execute"
        }
        (OperationId::UpdateWallet, _) => {
            "  # Rename a wallet
  voltage wallets update WALLET_ID --profile prod --name operations --execute"
        }
        (OperationId::DeleteWallet, _) => {
            "  # Show the request without --execute, then delete the wallet
  voltage wallets delete WALLET_ID --profile prod
  voltage wallets delete WALLET_ID --profile prod --execute"
        }
        (OperationId::UpdateWalletPolicies, _) => {
            "  # Save only the current policies, not the output envelope, edit them, and send them back
  voltage wallets policies get WALLET_ID --profile prod --json | jq .data > policies.json
  voltage wallets policies update WALLET_ID --profile prod --data @policies.json --execute"
        }
        (OperationId::UpdateSandboxLineOfCredit, _) => {
            "  # Change a sandbox line of credit
  voltage credit-lines update-sandbox CREDIT_LINE_ID --profile prod --data @sandbox.json --execute"
        }
        (OperationId::CreateWebhook, _) => {
            "  # Send completed receives to your URL; the one-time secret is saved to a private file
  voltage webhooks create --profile prod --name orders --url https://example.com/voltage --event receive.completed --output-file webhook.json --execute"
        }
        (OperationId::UpdateWebhook, _) => {
            "  # Replace the event selection; list every event you still want
  voltage webhooks update WEBHOOK_ID --profile prod --event receive.completed --event send.failed --execute"
        }
        (OperationId::DeleteWebhook, _) => {
            "  # Show the request without --execute, then delete the webhook
  voltage webhooks delete WEBHOOK_ID --profile prod
  voltage webhooks delete WEBHOOK_ID --profile prod --execute"
        }
        (OperationId::GenerateWebhookKey, _) => {
            "  # Rotate the shared secret and save the new one to a private file
  voltage webhooks keys rotate WEBHOOK_ID --profile prod --output-file webhook-key.json --execute"
        }
        (OperationId::TestWebhook, _) => {
            "  # Send a test delivery with a delivery_id and a sample payload
  voltage webhooks test WEBHOOK_ID --profile prod --data @test-delivery.json --execute"
        }
        (OperationId::StartWebhookAsUser, _) => {
            "  # Resume deliveries
  voltage webhooks start WEBHOOK_ID --profile prod --execute"
        }
        (OperationId::StopWebhookAsUser, _) => {
            "  # Pause deliveries
  voltage webhooks stop WEBHOOK_ID --profile prod --execute"
        }
        (OperationId::RetryWebhookDelivery, _) => {
            "  # Deliver a failed event again
  voltage webhooks deliveries retry DELIVERY_ID --profile prod --webhook WEBHOOK_ID --execute"
        }
        (OperationId::AbandonWebhookDelivery, _) => {
            "  # Stop retrying a delivery
  voltage webhooks deliveries abandon DELIVERY_ID --profile prod --webhook WEBHOOK_ID --execute"
        }
        (OperationId::RequestAQuote, _) => {
            "  # Quote $10 in BTC for a USD payment on a line of credit
  voltage quotes create --profile prod --credit-line CREDIT_LINE_ID --network mutinynet --amount 10 --unit usd --to btc --execute"
        }
        (OperationId::CreateSession, _) => {
            "  # Create a hosted checkout session; its one-time token is saved to a private file
  voltage checkout sessions create --profile prod --data @session-request.json --output-file session.json --execute"
        }
        (OperationId::CreateEventStreamToken, _) => {
            "  # Create a checkout event stream token and save it to a private file
  voltage checkout streams create --profile prod --data @stream.json --output-file stream-token.json --execute"
        }
        (OperationId::UpdateSettings, _) => {
            "  # Replace the environment's checkout settings
  voltage checkout settings update --profile prod --data @settings.json --execute"
        }
        (OperationId::GetEvents, _) => {
            "  # Follow checkout events with a stream token read from stdin
  voltage checkout events watch --token-file - --json"
        }
        _ => return None,
    })
}

/// Where each part of the CLI is explained, under `docs/`.
fn guide(path: &[&str]) -> &'static str {
    match path {
        [
            "login" | "logout" | "auth" | "organizations" | "environments" | "profiles" | "context",
            ..,
        ] => "guides/login-and-profiles.md",
        ["wallets", ..] => "guides/wallets.md",
        ["webhooks", ..] => "guides/webhooks.md",
        ["checkout", ..] => "guides/checkout.md",
        [
            "payments" | "quotes" | "credit-lines" | "treasury" | "price" | "convert",
            ..,
        ] => "guides/payments.md",
        ["completions", ..] => "guides/install.md",
        _ => "README.md",
    }
}

/// The guide for `path` in the docs that match this binary; see [`DOCS_REF`].
fn learn_more(path: &[&str]) -> String {
    format!(
        "Learn more: https://github.com/voltagecloud/voltage-cli/blob/{DOCS_REF}/docs/{}",
        guide(path)
    )
}

/// Examples for commands the CLI defines itself rather than the API contract.
fn local_examples(path: &[&str]) -> Option<&'static str> {
    Some(match path {
        ["login"] => {
            "  # Sign in through your browser and save the login as `work`
  voltage login --account work"
        }
        ["auth", "import-key"] => {
            "  # Save an environment API key read from stdin
  voltage auth import-key --account ci --stdin < key.txt"
        }
        ["profiles", "create"] => {
            "  # Save an organization, environment, and credential as `prod`
  voltage profiles create prod --org ORG_ID --env ENV_ID --account work"
        }
        ["context"] => {
            "  # Show what a command with this profile would use, and where each value came from
  voltage context --profile prod"
        }
        ["convert"] => {
            "  # What $10 buys in bitcoin now, and 21,000 sats in dollars on a past date
  voltage convert 10 usd --to btc
  voltage convert 21000 sats --to usd --at 2026-09-01T00:00:00Z"
        }
        _ => return None,
    })
}

/// Add `Examples:` and `Learn more:` to the long help of the CLI's own commands, which
/// clap derives without them.
fn describe_local(mut cmd: ClapCommand, path: &mut Vec<String>) -> ClapCommand {
    let words: Vec<&str> = path.iter().map(String::as_str).collect();
    let mut notes = Vec::new();
    if let Some(examples) = local_examples(&words) {
        notes.push(format!("Examples:\n{examples}"));
    }
    notes.push(learn_more(&words));
    cmd = cmd.after_long_help(notes.join("\n\n"));
    for name in cmd
        .get_subcommands()
        .map(|sub| sub.get_name().to_owned())
        .collect::<Vec<_>>()
    {
        path.push(name.clone());
        cmd = cmd.mut_subcommand(&name, |sub| describe_local(sub, path));
        path.pop();
    }
    cmd
}

/// The generated flag for one documented filter, typed by the contract's schema.
fn filter_arg(parameter: &Parameter) -> Arg {
    let flag = parameter.flag();
    let arg = Arg::new(flag.clone())
        .long(flag)
        .help(parameter.description.clone())
        .action(ArgAction::Append);
    if parameter.is_boolean() {
        arg.num_args(0..=1)
            .default_missing_value("true")
            .value_parser(clap::value_parser!(bool))
    } else if parameter.values.is_empty() {
        arg
    } else {
        let canonical = parameter.values.clone();
        arg.ignore_case(true)
            .value_parser(PossibleValuesParser::new(parameter.values.clone()).map(
                move |input: String| {
                    canonical
                        .iter()
                        .find(|value| value.eq_ignore_ascii_case(&input))
                        .cloned()
                        .unwrap_or(input)
                },
            ))
    }
}

/// The innermost subcommand and the words that reach it.
fn leaf(matches: &ArgMatches) -> (Vec<String>, &ArgMatches) {
    let mut path = Vec::new();
    let mut leaf = matches;
    while let Some((name, sub)) = leaf.subcommand() {
        path.push(name.into());
        leaf = sub;
    }
    (path, leaf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CredentialProblem as Problem;
    use crate::error::{CredentialRejection, ErrorKind as Kind};
    use std::path::Path;

    const ENV: &str = "22222222-2222-4222-8222-222222222222";
    const WALLET: &str = "33333333-3333-4333-8333-333333333333";

    mod examples {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/examples.rs"
        ));
    }
    use examples::{doc_examples, user_docs};

    fn api(args: &[&str]) -> ApiInvocation {
        let mut full = vec!["voltage"];
        full.extend_from_slice(args);
        match Invocation::try_parse_from(full).unwrap().command {
            Command::Api(api) => *api,
            Command::Local(local) => panic!("expected an API command, got {local:?}"),
        }
    }

    #[test]
    fn command_tree_is_valid() {
        command().debug_assert();
    }

    #[test]
    fn help_describes_commands_not_their_option_groups() {
        let root = command();
        assert_eq!(
            root.get_about().map(ToString::to_string).as_deref(),
            Some("Manage wallets, payments, and more with the Voltage API")
        );
        let about = |words: &[&str]| {
            let mut cmd = &root;
            for word in words {
                cmd = cmd.find_subcommand(word).unwrap();
            }
            cmd.get_about().map(ToString::to_string)
        };
        assert_eq!(
            about(&["payments"]).as_deref(),
            Some("Create, inspect, and summarize payments")
        );
        assert_eq!(
            about(&["checkout", "sessions"]).as_deref(),
            Some("Create and inspect hosted checkout sessions")
        );
        assert_eq!(
            about(&["payments", "receive"]).as_deref(),
            Some("Create a receive payment using friendly flags")
        );
        assert_eq!(
            about(&["wallets", "get"]).as_deref(),
            Some(operation(OperationId::GetWallet).description.as_str())
        );
        assert_eq!(
            about(&["login"]).as_deref(),
            Some("Sign in through your browser")
        );
        assert_eq!(
            about(&["auth", "import-key"]).as_deref(),
            Some("Save an API key from hidden input or stdin")
        );
    }

    #[test]
    fn representative_leaf_help_groups_options_and_explains_body_choices() {
        let help = command()
            .try_get_matches_from(["voltage", "payments", "send", "--help"])
            .unwrap_err()
            .to_string();
        for heading in [
            "Payment:",
            "Scope:",
            "Output:",
            "Safety:",
            "Request body:",
            "Examples:",
        ] {
            assert!(help.contains(heading), "missing {heading} in:\n{help}");
        }
        assert!(help.contains("exactly one of --invoice or --address"));
        assert!(help.contains("they cannot be combined"));
        assert!(help.contains("voltage payments send"));

        let help = command()
            .try_get_matches_from(["voltage", "wallets", "create", "--help"])
            .unwrap_err()
            .to_string();
        assert!(help.contains("Select one environment with --env or --profile"));
        assert!(help.contains("voltage wallets create"));
    }

    #[test]
    fn json_is_only_detected_as_an_option_before_the_argument_terminator() {
        assert!(json_errors_requested(["voltage", "--json", "paymnts"]));
        assert!(json_errors_requested(["voltage", "paymnts", "--json"]));
        assert!(!json_errors_requested(["voltage", "paymnts"]));
        assert!(!json_errors_requested(["voltage", "--", "--json"]));
        assert!(!json_errors_requested([
            "voltage",
            "--query=label=--json",
            "payments"
        ]));
    }

    #[test]
    fn raw_body_conflicts_with_friendly_fields() {
        assert!(
            Invocation::try_parse_from([
                "voltage", "wallets", "create", "--data", "@a.json", "--name", "hello"
            ])
            .is_err()
        );
    }

    #[test]
    fn all_is_accepted_by_every_list_and_pages_only_a_paginated_one() {
        // `wallets list` returns every wallet at once, so `--all` changes nothing.
        assert!(!api(&["wallets", "list", "--all"]).all);
        assert!(!api(&["webhooks", "list", "--all"]).all);
        assert!(api(&["payments", "list", "--all"]).all);
        // A command that is not a list still rejects it.
        let get =
            Invocation::try_parse_from(["voltage", "wallets", "get", WALLET, "--all"]).unwrap_err();
        assert!(
            get.message.contains("unexpected argument '--all'"),
            "{}",
            get.message
        );
    }

    #[test]
    fn wallet_flag_names_the_wallet_a_wallet_command_acts_on() {
        const OTHER: &str = "44444444-4444-4444-8444-444444444444";
        let wallet = Some(WALLET.parse().unwrap());
        assert_eq!(
            api(&["wallets", "get", "--wallet", WALLET]).resource_id,
            wallet
        );
        assert_eq!(
            api(&["wallets", "get", WALLET, "--wallet", WALLET]).resource_id,
            wallet
        );
        assert_eq!(
            api(&["wallets", "delete", "--wallet", WALLET]).resource_id,
            wallet
        );

        let differ =
            Invocation::try_parse_from(["voltage", "wallets", "get", WALLET, "--wallet", OTHER])
                .unwrap_err();
        assert_eq!(differ.kind, Kind::Usage);
        assert_eq!(
            differ.message,
            format!("WALLET_ID {WALLET} and --wallet {OTHER} name different wallets")
        );
        assert_eq!(
            differ.hint.as_deref(),
            Some("Pass the wallet once, as WALLET_ID or --wallet.")
        );

        let missing = Invocation::try_parse_from(["voltage", "wallets", "get"]).unwrap_err();
        assert_eq!(missing.kind, Kind::Usage);
        assert_eq!(missing.message, "wallets get needs a wallet ID");
        assert!(
            missing
                .hint
                .unwrap()
                .contains("voltage wallets get WALLET_ID")
        );

        // Commands that act on something else still require their own ID.
        let payment =
            Invocation::try_parse_from(["voltage", "payments", "get", "--wallet", WALLET])
                .unwrap_err();
        assert!(
            payment.message.contains("<PAYMENT_ID|--id <ID>>"),
            "{}",
            payment.message
        );
    }

    #[test]
    fn short_o_selects_the_output_format_before_or_after_the_subcommand() {
        for args in [
            ["voltage", "-o", "json", "wallets", "list"],
            ["voltage", "wallets", "list", "-o", "json"],
        ] {
            let invocation = Invocation::try_parse_from(args).unwrap();
            assert_eq!(invocation.global.output, Some(OutputFormat::Json));
        }
    }

    #[test]
    fn global_flags_work_after_subcommand() {
        let invocation = Invocation::try_parse_from([
            "voltage",
            "wallets",
            "list",
            "--profile",
            "staging",
            "--env",
            ENV,
            "--env",
            WALLET,
        ])
        .unwrap();
        assert_eq!(invocation.global.profile.as_deref(), Some("staging"));
        assert_eq!(invocation.global.env.len(), 2);
        assert_eq!(invocation.global.timeout, Duration::from_secs(60));
        let Command::Api(api) = invocation.command else {
            panic!("expected an API command");
        };
        assert_eq!(
            api.operation.id,
            OperationId::GetAllOrganizationsWalletsAsUser
        );
        assert!(api.body.is_none());
    }

    #[test]
    fn payment_aliases_reach_create_payment_with_their_direction() {
        let invocation = api(&[
            "payments",
            "receive",
            "--currency",
            "btc",
            "--kind",
            "bolt11",
            "--wait",
            "ready",
        ]);
        assert_eq!(invocation.operation.id, OperationId::CreatePayment);
        assert_eq!(invocation.alias, Some(PaymentDirection::Receive));
        assert_eq!(invocation.wait, Some(WaitTarget::Ready));
        let Some(BodySource::Friendly(FriendlyFlags::Payment(flags))) = invocation.body else {
            panic!("expected payment flags");
        };
        assert_eq!(flags.kind, Some(ReceiveKind::Bolt11));
        assert_eq!(flags.currency, Some(Currency::Btc));
        assert_eq!(api(&["payments", "create", "--data", "-"]).alias, None);
        assert_eq!(
            api(&["payments", "send", "--execute"]).requested,
            Requested::Execute
        );
        assert_eq!(api(&["payments", "send"]).requested, Requested::Unspecified);
        // A change without --execute is already the preview, so there is no --dry-run.
        assert!(Invocation::try_parse_from(["voltage", "payments", "send", "--dry-run"]).is_err());
        assert!(Invocation::try_parse_from(["voltage", "payments", "send", "-n"]).is_err());
        assert!(Invocation::try_parse_from(["voltage", "payments", "list", "--execute"]).is_err());
        assert_eq!(api(&["payments", "send"]).command_name(), "payments send");
        assert_eq!(api(&["payments", "list"]).command_name(), "payments list");
        assert_eq!(api(&["payments", "list"]).requested, Requested::Unspecified);
    }

    #[test]
    fn documented_filters_are_validated_and_canonicalized() {
        let invocation = api(&[
            "payments",
            "list",
            "--statuses",
            "completed",
            "--statuses",
            "failed",
            "--sort-order",
            "asc",
            "--query",
            "limit=5",
            "--all",
        ]);
        assert!(invocation.all);
        let filter = |name: &str| {
            invocation
                .filters
                .iter()
                .find(|filter| filter.parameter.name == name)
                .unwrap()
        };
        assert_eq!(filter("statuses").values, ["completed", "failed"]);
        assert_eq!(filter("sort_order").values, ["ASC"]);
        assert_eq!(
            invocation.query,
            [QueryOverride {
                name: "limit".into(),
                value: "5".into()
            }]
        );
        assert!(
            Invocation::try_parse_from(["voltage", "payments", "list", "--statuses", "bogus"])
                .is_err()
        );
        assert!(
            Invocation::try_parse_from(["voltage", "payments", "list", "--query", "limit"])
                .is_err()
        );
        let bills = api(&["bills", "list", "--exclude-zero-amount-bills"]);
        assert_eq!(
            filter_values_of(&bills, "exclude_zero_amount_bills"),
            ["true"]
        );
    }

    fn filter_values_of<'a>(invocation: &'a ApiInvocation, name: &str) -> &'a [String] {
        &invocation
            .filters
            .iter()
            .find(|filter| filter.parameter.name == name)
            .unwrap()
            .values
    }

    #[test]
    fn operation_options_exist_only_where_supported() {
        let payment = api(&["payments", "get", WALLET, "--wait", "completed", "--qr"]);
        assert_eq!(payment.resource_id, Some(WALLET.parse().unwrap()));
        assert_eq!(payment.wait, Some(WaitTarget::Completed));
        assert!(payment.qr && !payment.copy);
        assert!(
            Invocation::try_parse_from(["voltage", "wallets", "get", WALLET, "--wait", "ready"])
                .is_err()
        );
        assert!(Invocation::try_parse_from(["voltage", "wallets", "get", "not-a-uuid"]).is_err());
        let session = api(&[
            "checkout",
            "sessions",
            "get",
            WALLET,
            "--token-file",
            "-",
            "--origin",
            "https://shop.example.test",
        ]);
        assert_eq!(session.token_file, Some(InputSource::Stdin));
        assert_eq!(
            session.origin.as_ref().map(Origin::as_str),
            Some("https://shop.example.test")
        );
        assert!(
            Invocation::try_parse_from([
                "voltage",
                "checkout",
                "sessions",
                "get",
                WALLET,
                "--origin",
                "https://shop.example.test/path",
            ])
            .is_err()
        );
        assert!(
            Invocation::try_parse_from(["voltage", "wallets", "list", "--timeout", "0"]).is_err()
        );
    }

    #[test]
    fn local_commands_parse_into_their_variants() {
        let login = Invocation::try_parse_from([
            "voltage",
            "login",
            "--no-browser",
            "--credential-store",
            "file",
        ])
        .unwrap();
        let Command::Local(LocalCommand::Login(flags)) = login.command else {
            panic!("expected login");
        };
        assert!(flags.no_browser);
        assert_eq!(flags.credential_store, Some(CredentialStore::File));
        let profile =
            Invocation::try_parse_from(["voltage", "profiles", "delete", "stage"]).unwrap();
        assert!(matches!(
            profile.command,
            Command::Local(LocalCommand::Profiles { command: ProfileCommand::Delete { name } }) if name == "stage"
        ));
        let convert = Invocation::try_parse_from([
            "voltage",
            "convert",
            "10",
            "usd",
            "--to",
            "btc",
            "--at",
            "2026-09-18T17:30:00Z",
        ])
        .unwrap();
        let Command::Local(LocalCommand::Convert(flags)) = convert.command else {
            panic!("expected convert");
        };
        assert_eq!(flags.unit, AmountUnit::Usd);
        assert_eq!(flags.to, Currency::Btc);
        assert_eq!(flags.at.as_deref(), Some("2026-09-18T17:30:00Z"));
        assert!(Invocation::try_parse_from(["voltage", "convert", "10", "usd"]).is_err());
        assert!(matches!(
            Invocation::try_parse_from(["voltage", "context"])
                .unwrap()
                .command,
            Command::Local(LocalCommand::Context)
        ));
        assert!(matches!(
            Invocation::try_parse_from(["voltage", "price"])
                .unwrap()
                .command,
            Command::Local(LocalCommand::Price(PriceFlags { at: None }))
        ));
        let status = Invocation::try_parse_from(["voltage", "auth", "status"]).unwrap();
        assert!(matches!(
            status.command,
            Command::Local(LocalCommand::Auth {
                command: AuthCommand::Status
            })
        ));
        let data = api(&["payments", "create", "--data", "@request.json"]);
        assert!(
            matches!(data.body, Some(BodySource::Raw(InputSource::File(path))) if path == std::path::Path::new("request.json"))
        );
        assert!(
            Invocation::try_parse_from(["voltage", "payments", "create", "--data", "x.json"])
                .is_err()
        );
        assert!(Invocation::try_parse_from(["voltage", "bogus"]).is_err());
    }

    /// GitHub's anchors for a document's headings: lowercase, punctuation dropped, spaces
    /// as hyphens, and `-1`, `-2`, ... after repeats. Lines inside code fences are skipped.
    fn anchors(text: &str) -> Vec<String> {
        let mut anchors: Vec<String> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let mut fenced = false;
        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
            }
            if fenced || !line.starts_with('#') {
                continue;
            }
            let base: String = line
                .trim_start_matches('#')
                .trim()
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
                .map(|c| if c == ' ' { '-' } else { c })
                .collect();
            let repeats = seen.iter().filter(|heading| **heading == base).count();
            anchors.push(match repeats {
                0 => base.clone(),
                n => format!("{base}-{n}"),
            });
            seen.push(base);
        }
        anchors
    }

    #[test]
    fn documentation_links_reach_existing_files_and_headings() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        for (path, text) in user_docs() {
            // Markdown links, and the README's HTML image paths.
            let markdown = text
                .split("](")
                .skip(1)
                .filter_map(|rest| rest.split(')').next());
            let html = ["src=\"", "srcset=\""]
                .into_iter()
                .flat_map(|attribute| text.split(attribute).skip(1))
                .filter_map(|rest| rest.split('"').next());
            for link in markdown.chain(html) {
                if link.contains("://") {
                    continue;
                }
                let (file, fragment) = link.split_once('#').unwrap_or((link, ""));
                let target = if file.is_empty() {
                    root.join(&path)
                } else {
                    root.join(&path).parent().unwrap().join(file)
                };
                let contents = std::fs::read_to_string(&target)
                    .unwrap_or_else(|error| panic!("{}: {link}: {error}", path.display()));
                if !fragment.is_empty() {
                    assert!(
                        anchors(&contents).iter().any(|anchor| anchor == fragment),
                        "{}: no heading for {link}",
                        path.display()
                    );
                }
            }
        }
    }

    /// Every example line in the long help of every command.
    fn help_examples(cmd: &mut ClapCommand, examples: &mut Vec<String>) {
        let help = cmd.render_long_help().to_string();
        if let Some((_, section)) = help.split_once("Examples:") {
            examples.extend(
                section
                    .lines()
                    .skip(1)
                    .map(str::trim)
                    .take_while(|line| !line.is_empty())
                    .filter(|line| !line.starts_with('#'))
                    // Keep the command, not a redirection of its input or output.
                    .map(|line| {
                        line.split(['<', '>', '|'])
                            .next()
                            .unwrap_or_default()
                            .trim()
                    })
                    .map(str::to_owned),
            );
        }
        for sub in cmd.get_subcommands_mut() {
            help_examples(sub, examples);
        }
    }

    /// `docs/reference/global-flags.md`, rendered from the flags every command accepts.
    fn global_flags_reference() -> String {
        let flags = GlobalFlags::augment_args(ClapCommand::new("voltage"));
        // Rows by heading, in the order each heading first appears.
        let mut sections: Vec<(&str, Vec<String>)> = Vec::new();
        for arg in flags.get_arguments() {
            let Some(long) = arg.get_long() else { continue };
            let mut flag = match arg.get_short() {
                Some(short) => format!("`-{short}`, `--{long}"),
                None => format!("`--{long}"),
            };
            let mut meaning = arg.get_help().map(ToString::to_string).unwrap_or_default();
            if arg.get_action().takes_values() {
                let names = arg.get_value_names().unwrap_or_default();
                let name = names.first().map_or("VALUE", |name| name.as_str());
                flag.push_str(&format!(" {name}"));
                let values: Vec<String> = arg
                    .get_possible_values()
                    .iter()
                    .filter(|value| !value.is_hide_set())
                    .map(|value| format!("`{}`", value.get_name()))
                    .collect();
                if !values.is_empty() {
                    meaning.push_str(&format!(" ({})", values.join(", ")));
                }
                if let Some(default) = arg.get_default_values().first() {
                    meaning.push_str(&format!("; default `{}`", default.to_string_lossy()));
                }
            }
            flag.push('`');
            let row = format!("| {flag} | {} |", meaning.replace('|', "\\|"));
            let heading = arg.get_help_heading().unwrap_or("Other");
            match sections.iter_mut().find(|(name, _)| *name == heading) {
                Some((_, rows)) => rows.push(row),
                None => sections.push((heading, vec![row])),
            }
        }
        let mut text = String::from(
            "# Global flags\n\nGenerated from the command definitions by `VOLTAGE_BLESS=1 cargo test`; do not edit by hand. Every command accepts these flags, before or after its name.\n\n`--execute` is not global: each command that changes something takes it. See [changes need `--execute`](../concepts/safety-model.md#changes-need---execute).\n",
        );
        for (heading, rows) in sections {
            text.push_str(&format!(
                "\n## {heading}\n\n| Flag | Meaning |\n|---|---|\n{}\n",
                rows.join("\n")
            ));
        }
        text.push_str("\nRelated: [environment variables](environment.md), [configuration](configuration.md), [output](output.md).\n");
        text
    }

    #[test]
    fn global_flags_reference_matches_the_command_definitions() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/reference/global-flags.md");
        let expected = global_flags_reference();
        if std::env::var_os("VOLTAGE_BLESS").is_some() {
            std::fs::write(&path, &expected).unwrap();
        }
        // Git may check the file out with CRLF line endings, as on Windows.
        let actual = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert!(
            actual == expected,
            "docs/reference/global-flags.md is stale; run VOLTAGE_BLESS=1 cargo test"
        );
    }

    /// Every error kind. A new kind fails to compile here until it is listed.
    fn error_kinds() -> [Kind; 7] {
        let _exhaustive = |kind: Kind| match kind {
            Kind::Api
            | Kind::Usage
            | Kind::Auth
            | Kind::Transport
            | Kind::Timeout
            | Kind::NotExecuted
            | Kind::Interrupted => (),
        };
        [
            Kind::Api,
            Kind::Usage,
            Kind::Auth,
            Kind::Transport,
            Kind::Timeout,
            Kind::NotExecuted,
            Kind::Interrupted,
        ]
    }

    /// Every `.rs` file under `dir`, in subdirectories too.
    fn rust_sources(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(rust_sources(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
        files
    }

    #[test]
    fn rust_sources_include_subdirectories() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let files = rust_sources(&root.join("src"));
        assert!(files.contains(&root.join("src/cli.rs")));
        assert!(files.contains(&root.join("src/output/human.rs")));
        assert!(files.iter().all(|file| file.extension().unwrap() == "rs"));
    }

    #[test]
    fn every_variable_and_exit_code_is_documented() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let read = |path: &str| std::fs::read_to_string(root.join(path)).unwrap();
        let environment = read("docs/reference/environment.md");
        let mut variables = std::collections::BTreeSet::new();
        for path in rust_sources(&root.join("src")) {
            let source = std::fs::read_to_string(path).unwrap();
            for (start, _) in source.match_indices("\"VOLTAGE_") {
                let name: String = source[start + 1..]
                    .chars()
                    .take_while(|c| c.is_ascii_uppercase() || *c == '_')
                    .collect();
                // A bare `VOLTAGE_` is a prefix check, not a variable.
                if !name.ends_with('_') {
                    variables.insert(name);
                }
            }
        }
        // Set by build.rs at compile time, and the contributors' switch for this test file;
        // none is read from a user's environment.
        variables.remove("VOLTAGE_VERSION");
        variables.remove("VOLTAGE_DOCS_REF");
        variables.remove("VOLTAGE_BLESS");
        assert!(variables.len() >= 8, "{variables:?}");
        for name in variables
            .iter()
            .map(String::as_str)
            .chain(["COLUMNS", "XDG_CONFIG_HOME"])
        {
            assert!(
                environment.contains(&format!("`{name}`")),
                "{name} is undocumented"
            );
        }
        let exit_codes = read("docs/reference/exit-codes.md");
        for kind in error_kinds() {
            let code = kind.exit_code();
            assert!(
                exit_codes.contains(&format!("| {code} |")),
                "exit code {code} is undocumented"
            );
        }
    }

    #[test]
    fn troubleshooting_covers_every_exit_code_and_credential_problem() {
        let text = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/guides/troubleshooting.md"),
        )
        .unwrap()
        // Git may check the file out with CRLF line endings, as on Windows.
        .replace("\r\n", "\n");
        for code in error_kinds().map(Kind::exit_code) {
            assert!(
                text.contains(&format!("\n### Exit code {code}\n")),
                "exit code {code}"
            );
        }
        // A new problem fails to compile here until it has a name, but its entry is checked
        // only once it is also added to the list below.
        let name = |problem: Problem| match problem {
            Problem::EmptyApiKey => "empty_api_key",
            Problem::NoCredential => "no_credential",
            Problem::MultipleCredentials => "multiple_credentials",
            Problem::UnknownCredential => "unknown_credential",
            Problem::BoundElsewhere => "bound_elsewhere",
            Problem::AuthUrlMismatch => "auth_url_mismatch",
        };
        for problem in [
            Problem::EmptyApiKey,
            Problem::NoCredential,
            Problem::MultipleCredentials,
            Problem::UnknownCredential,
            Problem::BoundElsewhere,
            Problem::AuthUrlMismatch,
        ] {
            let name = name(problem);
            assert_eq!(serde_json::to_value(problem).unwrap(), name);
            assert!(text.contains(&format!("\n### `{name}`\n")), "{name}");
        }
        let rejected = |rejection: CredentialRejection| match rejection {
            CredentialRejection::LoginExpired => "login_expired",
        };
        let name = rejected(CredentialRejection::LoginExpired);
        assert_eq!(
            serde_json::to_value(CredentialRejection::LoginExpired).unwrap(),
            name
        );
        assert!(text.contains(&format!("\n### `{name}`\n")), "{name}");
    }

    /// Every command below `cmd`, by path, with its rendered long help.
    fn long_helps(cmd: &mut ClapCommand, path: &mut Vec<String>, out: &mut Vec<(String, String)>) {
        out.push((path.join(" "), cmd.render_long_help().to_string()));
        for sub in cmd.get_subcommands_mut() {
            path.push(sub.get_name().to_owned());
            long_helps(sub, path, out);
            path.pop();
        }
    }

    #[test]
    fn every_change_has_an_example_and_every_command_links_its_guide() {
        let mut root = command();
        root.build();
        let mut helps = Vec::new();
        long_helps(&mut root, &mut vec!["voltage".to_owned()], &mut helps);
        let help = |path: &str| {
            helps
                .iter()
                .find(|(name, _)| name == path)
                .map(|(_, help)| help.as_str())
                .unwrap_or_else(|| panic!("no command {path}"))
        };
        let changes = OPERATIONS
            .iter()
            .filter(|operation| operation.method != Method::Get)
            .map(|operation| format!("voltage {}", operation.command.join(" ")))
            .chain(
                PAYMENT_ALIASES
                    .iter()
                    .map(|(name, _)| format!("voltage payments {name}")),
            );
        for path in changes {
            assert!(
                help(&path).contains("Examples:\n  #"),
                "{path} has no example"
            );
        }
        let root_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // One docs ref for every link: the release tag of a release build, else master.
        assert!(
            DOCS_REF == "master" || DOCS_REF == format!("v{}", env!("CARGO_PKG_VERSION")),
            "{DOCS_REF}"
        );
        assert!(help("voltage").contains(&format!(
            "Documentation: https://github.com/voltagecloud/voltage-cli/tree/{DOCS_REF}/docs"
        )));
        let prefix = format!(
            "Learn more: https://github.com/voltagecloud/voltage-cli/blob/{DOCS_REF}/docs/"
        );
        for (path, help) in helps.iter().filter(|(path, _)| path != "voltage") {
            // Clap's generated `help` subcommands print other commands' help.
            if path.split(' ').any(|word| word == "help") {
                continue;
            }
            let page = help
                .lines()
                .find_map(|line| line.strip_prefix(&prefix))
                .unwrap_or_else(|| panic!("{path} has no Learn more link"));
            assert!(root_dir.join("docs").join(page).is_file(), "{path}: {page}");
        }
        // `-h` stays short: the notes are only in `--help`.
        let short = command()
            .try_get_matches_from(["voltage", "payments", "send", "-h"])
            .unwrap_err()
            .to_string();
        assert!(!short.contains("Examples:") && !short.contains("Learn more:"));
    }

    /// A floor below the number of documented examples, so a scanner that stops finding
    /// them fails the test instead of checking nothing.
    const MIN_DOC_EXAMPLES: usize = 40;

    #[test]
    fn documented_examples_parse_and_changes_say_whether_to_send() {
        let mut examples = doc_examples();
        let from_docs = examples.len();
        let mut root = command();
        root.build();
        help_examples(&mut root, &mut examples);
        assert!(
            from_docs >= MIN_DOC_EXAMPLES && examples.len() > from_docs,
            "{examples:?}"
        );
        // A change shown without --execute is a deliberate preview only when the same
        // command is also documented with --execute; otherwise it forgot to send.
        let words = |example: &str| example.split_whitespace().collect::<Vec<_>>().join(" ");
        let sent: std::collections::HashSet<String> = examples
            .iter()
            .filter(|example| example.split_whitespace().any(|word| word == "--execute"))
            .map(|example| {
                example
                    .split_whitespace()
                    .filter(|word| *word != "--execute")
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        for example in &examples {
            // Placeholder IDs stand for real UUIDs; every other placeholder parses as typed.
            // Words split on whitespace with no shell quoting, so an example must not use a
            // quoted argument with spaces such as `--name "My wallet"`.
            let args = example.split_whitespace().map(|word| {
                if word.ends_with("_ID") && word.chars().all(|c| c.is_ascii_uppercase() || c == '_')
                {
                    WALLET
                } else {
                    word
                }
            });
            let matches = match command().try_get_matches_from(args) {
                Ok(matches) => matches,
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                    ) =>
                {
                    continue;
                }
                Err(error) => panic!("{example}: {error}"),
            };
            let invocation = Invocation::from_matches(&matches)
                .unwrap_or_else(|error| panic!("{example}: {error:?}"));
            if let Command::Api(api) = invocation.command {
                assert!(
                    api.operation.method == Method::Get
                        || api.requested != Requested::Unspecified
                        || sent.contains(&words(example)),
                    "a change without --execute only describes itself; add --execute, or also \
                     document the same command with --execute: {example}"
                );
            }
        }
    }
}
