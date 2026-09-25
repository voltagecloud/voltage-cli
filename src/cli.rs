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
    registry::{OPERATIONS, Operation, OperationId, Parameter, operation},
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
    #[arg(long, global = true, value_enum, help_heading = "Output")]
    pub output: Option<OutputFormat>,
    /// Save the complete response to a new owner-only file
    #[arg(long, global = true, value_name = "PATH", help_heading = "Output")]
    pub output_file: Option<PathBuf>,
    /// Explicitly allow secrets in result output
    #[arg(long, global = true, help_heading = "Output")]
    pub show_secrets: bool,
    /// Approve consequential actions without prompting (never supplies a credential)
    #[arg(short = 'y', long, global = true, help_heading = "Safety")]
    pub yes: bool,
    /// Never prompt for approval or a secret; supply --yes or --stdin as needed
    #[arg(long, global = true, help_heading = "Safety")]
    pub no_input: bool,
    /// Hide optional progress and notices, not errors, results, or recovery IDs
    #[arg(short = 'q', long, global = true, help_heading = "Output")]
    pub quiet: bool,
    /// HTTP or wait deadline in seconds
    #[arg(long, global = true, value_name = "SECONDS", default_value = "60", value_parser = parse_timeout, help_heading = "Request")]
    pub timeout: Duration,
    /// Configuration directory (default: ~/.config/voltage)
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
    /// Where to save credentials
    #[arg(long, value_enum, default_value_t = CredentialStore::Keychain)]
    pub credential_store: CredentialStore,
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

/// Positional resource identifier for operations with a target.
#[derive(Debug, Args)]
struct TargetFlags {
    #[arg(help_heading = "Arguments")]
    resource_id: Uuid,
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
    /// Wallet currency for sends, receive currency for any-amount receives
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
    /// Exact decimal amount; pair with --unit
    #[arg(long)]
    pub amount: Option<String>,
    /// Unit of --amount
    #[arg(long, value_enum)]
    pub unit: Option<AmountUnit>,
    /// Maximum network/provider fee; processing fees are additional
    #[arg(long)]
    pub max_fee: Option<String>,
    /// Unit of --max-fee: msats, sats, or btc
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
    /// Exact decimal amount
    #[arg(long)]
    pub amount: Option<String>,
    /// Unit of --amount
    #[arg(long, value_enum)]
    pub unit: Option<AmountUnit>,
    /// Target currency
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
    pub token_file: Option<InputSource>,
    pub origin: Option<Origin>,
    pub wait: Option<WaitTarget>,
    pub qr: bool,
    pub copy: bool,
    pub body: Option<BodySource>,
}

impl ApiInvocation {
    /// `--qr` or `--copy` was requested.
    pub fn presents_invoice(&self) -> bool {
        self.qr || self.copy
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
    let pagination = parse_if::<PaginationFlags>(operation.has_parameter("limit"), m)?;
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
        resource_id: target.map(|target| target.resource_id),
        filters,
        query: query.query,
        all: pagination.is_some_and(|flags| flags.all),
        token_file: checkout.and_then(|flags| flags.token_file),
        origin: origin.and_then(|flags| flags.origin),
        wait: wait.and_then(|flags| flags.wait),
        qr: invoice.as_ref().is_some_and(|flags| flags.qr),
        copy: invoice.as_ref().is_some_and(|flags| flags.copy),
        body,
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

/// The complete command tree, for parsing, help, and completions.
pub fn command() -> ClapCommand {
    let root = ClapCommand::new("voltage")
        .version(env!("CARGO_PKG_VERSION"))
        .subcommand_required(true)
        .arg_required_else_help(true);
    let root = GlobalFlags::augment_args(root);
    let root = LocalCommand::augment_subcommands(root);
    // Derived types apply their own doc comments as `about`, so the command's text goes last.
    add_children(root, &[]).about("Manage wallets, payments, and more with the Voltage API")
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
            .about(group_about(&path)),
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
            "Scope:\n  Wallets have organization scope. --env validates the wallet on reads and mutations; it does not change this endpoint's scope.".to_owned()
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
    if !notes.is_empty() {
        cmd = cmd.after_help(notes.join("\n\n"));
    }
    if let Some(target) = operation.target {
        cmd = TargetFlags::augment_args(cmd)
            .mut_arg("resource_id", |arg| arg.value_name(target.value_name()));
    }
    for parameter in operation
        .query_filters()
        .filter(|parameter| parameter.scope().is_none())
    {
        cmd = cmd.arg(filter_arg(parameter));
    }
    cmd = QueryFlags::augment_args(cmd);
    if operation.has_parameter("limit") {
        cmd = PaginationFlags::augment_args(cmd);
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
            "Pass --wallet and --currency, plus exactly one of --invoice or --address. Pair --amount with --unit and --max-fee with --fee-unit; on-chain sends require amount and unit.",
        ),
        (OperationId::CreatePayment, "receive") => Some(
            "Pass --wallet, --currency, and --kind. If an amount is set, pass both --amount and --unit.",
        ),
        (OperationId::CreateWallet, "create") => Some(
            "Select one environment with --env or --profile, then pass --name, --credit-line, --network, and --limit.",
        ),
        _ => None,
    }
}

/// Focused examples for the commands where friendly-body combinations are least obvious.
fn endpoint_examples(id: OperationId, command_name: &str) -> Option<&'static str> {
    match (id, command_name) {
        (OperationId::CreatePayment, "send") => Some(
            "  voltage payments send --profile prod --wallet WALLET_ID --currency btc --invoice BOLT11_INVOICE --max-fee 10 --fee-unit sats --yes\n  voltage payments send --profile prod --wallet WALLET_ID --currency btc --address BITCOIN_ADDRESS --amount 1000 --unit sats --yes",
        ),
        (OperationId::CreatePayment, "receive") => Some(
            "  voltage payments receive --profile prod --wallet WALLET_ID --currency btc --kind bolt11 --amount 1000 --unit sats --wait ready",
        ),
        (OperationId::CreateWallet, "create") => Some(
            "  voltage wallets create --profile prod --name treasury --credit-line CREDIT_LINE_ID --network mutinynet --limit 100000",
        ),
        _ => None,
    }
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

    const ENV: &str = "22222222-2222-4222-8222-222222222222";
    const WALLET: &str = "33333333-3333-4333-8333-333333333333";

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
        assert_eq!(flags.credential_store, CredentialStore::File);
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
}
