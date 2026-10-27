//! Process composition: parse the command line, run one command, report, and exit.

use crate::{
    Error, Result, all_orgs,
    api::{self, Execution, SubmissionState},
    auth::{self, Discovery, Resolver},
    cli::{
        self, AuthCommand, Command, ConvertFlags, FeedbackFlags, GlobalFlags, Invocation,
        LocalCommand, ParseFailure, PriceFlags, ProfileCommand,
    },
    config::{self, Profile, Scope, Settings},
    context,
    error::{CredentialSources, ErrorKind},
    feedback::{self, Reporter},
    greeting,
    output::{Envelope, NextStep, Output, OutputFormat, report_error, write_stdout},
    pick,
    price::{PRICE_URL, PriceService},
    style::ColorChoice,
    wallet_currency, wallet_shortlist,
};
use serde::Serialize;
use std::process::ExitCode;

const BINARY_NAME: &str = "voltage";

pub async fn run() -> ExitCode {
    let json_errors = cli::json_errors_requested(std::env::args_os());
    feedback::report_panics(Reporter::detect(feedback::interactive(json_errors)));
    let color = ColorChoice::requested(std::env::args_os())
        .with_environment(|name| std::env::var(name).ok());
    if let Some(global) = greeting::wanted(&std::env::args_os().collect::<Vec<_>>()) {
        return match greeting::show(&global) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error, OutputFormat::Table, color),
        };
    }
    let invocation = match Invocation::from_env(color) {
        Ok(invocation) => invocation,
        Err(failure) => return fail_parse(failure, json_errors, color),
    };
    let format = invocation.global.output_format();
    let color = invocation.global.color();
    let quiet = invocation.global.quiet;
    let submission = SubmissionState::default();
    // Biased, with the interrupt first: tokio installs the SIGINT handler when `ctrl_c` is
    // first polled. Polled second, a command that reaches a prompt without awaiting would leave
    // an early Ctrl-C to the default handler, which kills the process without a report.
    let result = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            // The next interrupt must not wait for synchronous cleanup or stderr I/O. The
            // listener is detached on purpose: runtime shutdown after the report ends it.
            // process::exit skips destructors; by then the first interrupt has already
            // dropped `execute`, which restores terminal echo after a hidden prompt.
            tokio::spawn(async {
                if tokio::signal::ctrl_c().await.is_ok() {
                    std::process::exit(130);
                }
            });
            Err(submission.interrupted())
        },
        result = execute(invocation, &submission) => result,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.is_stdout_closed() => ExitCode::SUCCESS,
        // Ctrl-C is how a watch ends; it keeps the interrupt's exit code without an error.
        Err(error) if error.kind == ErrorKind::Interrupted && submission.is_watching() => {
            if !quiet {
                eprintln!("Stopped watching payments.");
            }
            process_exit(error.kind.exit_code())
        }
        Err(error) => fail(error, format, color),
    }
}

fn fail_parse(failure: ParseFailure, json_errors: bool, color: ColorChoice) -> ExitCode {
    match failure {
        ParseFailure::Validation(error) => {
            fail(error, OutputFormat::select(json_errors, None), color)
        }
        ParseFailure::Clap(error) if error.use_stderr() && json_errors => {
            fail(Error::usage(error.to_string()), OutputFormat::Json, color)
        }
        ParseFailure::Clap(error) => {
            let writes_stdout = !error.use_stderr();
            let exit_code = error.exit_code();
            match error.print() {
                Ok(()) => process_exit(exit_code),
                Err(io) if writes_stdout && io.kind() == std::io::ErrorKind::BrokenPipe => {
                    ExitCode::SUCCESS
                }
                Err(io) => fail(io.into(), OutputFormat::select(false, None), color),
            }
        }
    }
}

fn fail(error: Error, format: OutputFormat, color: ColorChoice) -> ExitCode {
    report_error(&error, format, color);
    process_exit(error.kind.exit_code())
}

fn process_exit(code: i32) -> ExitCode {
    u8::try_from(code)
        .map(ExitCode::from)
        .unwrap_or(ExitCode::FAILURE)
}

async fn execute(invocation: Invocation, submission: &SubmissionState) -> Result<()> {
    let Invocation { global, command } = invocation;
    if let Command::Local(LocalCommand::Completions { shell }) = command {
        // The generator panics on a write error, so render to memory and use the normal
        // stdout error policy when writing the completed script.
        let mut script = Vec::new();
        clap_complete::generate(shell, &mut cli::command(), BINARY_NAME, &mut script);
        return write_stdout(&script);
    }
    // `reserve` is false for a description, which goes to stdout and never reserves the file.
    let output = |reserve: bool| {
        Output::new(
            global.output_format(),
            global.show_secrets,
            global.output_file.as_deref().filter(|_| reserve),
            global.color(),
            global.quiet,
        )
    };
    // Feedback needs neither configuration nor credentials.
    if let Command::Local(LocalCommand::Feedback(flags)) = &command {
        return file_feedback(flags, &global, output(true)?);
    }
    // Price commands need neither configuration nor credentials.
    let command = match PriceQuery::try_from(command) {
        Ok(query) => {
            let mut out = output(true)?;
            let envelope = query.run(&global).await?;
            return out.write(envelope, &[]);
        }
        Err(command) => command,
    };
    let mut session = Session::open(&command, &global, submission).await?;
    match command {
        Command::Api(api) if api.all_orgs => {
            let mut out = output(true)?;
            all_orgs::wallets(&api, &global, session.resolver(), &session.scope, &mut out).await
        }
        Command::Api(mut api) => {
            // `VOLTAGE_EXECUTE` and every local check run before the wallet is read.
            let execution = Execution::resolve(&api, config::execute_variable()?);
            let resolver = session.resolver();
            wallet_shortlist::require(&api, &global, resolver, &session.scope).await?;
            wallet_currency::fill(&mut api, &global, resolver, &session.scope).await?;
            let mut out = output(!execution.describes())?;
            api::execute(&api, execution, &global, resolver, &session.scope, &mut out).await
        }
        Command::Local(local) => {
            let mut out = output(true)?;
            let envelope = session.local(local).await?;
            out.write(envelope, &[])
        }
    }
}

/// `voltage feedback`: a person at a terminal reads two links; anything else gets the report.
fn file_feedback(flags: &FeedbackFlags, global: &GlobalFlags, mut out: Output) -> Result<()> {
    let terminal = global.terminal();
    let filed = feedback::run(flags, out.on_terminal() && terminal.can_prompt());
    if out.on_terminal() {
        return out.preface(&filed.lines(terminal.glyphs().step));
    }
    out.write(Envelope::local(filed)?, &[])
}

/// One command once its configuration is read and its scope is complete: what the flags
/// asked for, the saved settings, the scope, and what the command may have changed.
/// Commands run from here; what they call receives only the parts it uses.
struct Session<'a> {
    global: &'a GlobalFlags,
    settings: Settings,
    scope: Scope,
    submission: &'a SubmissionState,
    credential_sources: Option<CredentialSources>,
}

impl<'a> Session<'a> {
    /// Read the configuration and select the scope, then ask for any scope the command needs
    /// and lacks. The scope does not change after this.
    async fn open(
        command: &Command,
        global: &'a GlobalFlags,
        submission: &'a SubmissionState,
    ) -> Result<Self> {
        let settings = Settings::open(config::directory(global.config_dir.clone())?)?;
        let (mut scope, sources) = settings.sourced_scope(global.scope_selection())?;
        let credential_sources = auth::environment_key_sources(global, &sources);
        let resolver = Resolver {
            settings: &settings,
            flags: global,
            submission,
            credential_sources,
        };
        pick::fill(command, global, resolver, &mut scope).await?;
        Ok(Self {
            global,
            settings,
            scope,
            submission,
            credential_sources,
        })
    }

    fn resolver(&self) -> Resolver<'_> {
        Resolver {
            settings: &self.settings,
            flags: self.global,
            submission: self.submission,
            credential_sources: self.credential_sources,
        }
    }

    async fn local(&mut self, command: LocalCommand) -> Result<Envelope> {
        let (global, scope, submission) = (self.global, &self.scope, self.submission);
        match command {
            LocalCommand::Login(flags) => {
                let mut outcome =
                    auth::login(&mut self.settings, &flags, global, submission).await?;
                outcome.profile =
                    pick::offer_profile(&mut self.settings, &outcome.account, global, submission)
                        .await?;
                Envelope::local(outcome)
            }
            LocalCommand::Logout(flags) => Envelope::local(
                auth::logout(
                    &mut self.settings,
                    scope,
                    flags.local,
                    global.terminal(),
                    submission,
                )
                .await?,
            ),
            LocalCommand::Auth {
                command: AuthCommand::Status,
            } => Envelope::local(auth::status(&self.settings, scope, global)?)
                .map(|status| status.titled("voltage auth status")),
            LocalCommand::Auth {
                command: AuthCommand::ImportKey(flags),
            } => {
                Envelope::local(auth::import_key(&mut self.settings, scope, &flags, global).await?)
            }
            LocalCommand::Organizations { .. } => Envelope::local(
                self.resolver()
                    .discover(scope, Discovery::Organizations)
                    .await?,
            ),
            LocalCommand::Environments { .. } => Envelope::local(
                self.resolver()
                    .discover(scope, Discovery::Environments)
                    .await?,
            ),
            LocalCommand::Context => Envelope::settings(context::inspect(&self.settings, global)?)
                .map(|context| context.titled("voltage context")),
            LocalCommand::Profiles { command } => {
                profiles(command, &mut self.settings, scope, global).await
            }
            LocalCommand::Completions { .. }
            | LocalCommand::Feedback(_)
            | LocalCommand::Price(_)
            | LocalCommand::Convert(_) => Err(Error::usage("This command needs no configuration")),
        }
    }
}

/// A command that reads only the price service.
enum PriceQuery {
    Report(PriceFlags),
    Convert(ConvertFlags),
}

/// Every other command is handed back unchanged.
impl TryFrom<Command> for PriceQuery {
    type Error = Command;

    fn try_from(command: Command) -> std::result::Result<Self, Command> {
        match command {
            Command::Local(LocalCommand::Price(flags)) => Ok(Self::Report(flags)),
            Command::Local(LocalCommand::Convert(flags)) => Ok(Self::Convert(flags)),
            other => Err(other),
        }
    }
}

impl PriceQuery {
    /// Read the price service for `price` or `convert`.
    async fn run(self, global: &GlobalFlags) -> Result<Envelope> {
        let service = PriceService::new(
            global.price_url.as_deref().unwrap_or(PRICE_URL),
            global.timeout,
        )?;
        let terminal = global.terminal();
        const WAITING: &str = "Waiting for price service...";
        match self {
            Self::Report(flags) => Envelope::local(
                terminal
                    .during(WAITING, service.report(flags.at.as_deref()))
                    .await?,
            ),
            Self::Convert(flags) => Envelope::local(
                terminal
                    .during(
                        WAITING,
                        service.convert(&flags.request(), flags.at.as_deref()),
                    )
                    .await?,
            ),
        }
    }
}

#[derive(Serialize)]
struct ProfileCreated {
    profile: String,
    created: bool,
}

#[derive(Serialize)]
struct ProfileDeleted {
    profile: String,
    deleted: bool,
}

/// Profile commands take the credential lock and re-read the configuration first, so two
/// concurrent commands never overwrite each other's changes. `Settings::create_profile`
/// takes the lock itself, because a login's profile offer saves through it too.
async fn profiles(
    command: ProfileCommand,
    settings: &mut Settings,
    scope: &Scope,
    global: &GlobalFlags,
) -> Result<Envelope> {
    let terminal = global.terminal();
    if let ProfileCommand::Create { name } = command {
        settings
            .create_profile(&name, terminal, |settings| {
                Ok(Profile {
                    account: settings.account_name(scope)?,
                    organization_id: scope.require_org()?,
                    environment_id: scope.single_env()?,
                })
            })
            .await?;
        // A command that uses the new profile, with the same configuration directory.
        let config_dir = global
            .config_dir
            .as_ref()
            .map(|dir| {
                format!(
                    " --config-dir {}",
                    config::shell_word(&dir.to_string_lossy())
                )
            })
            .unwrap_or_default();
        let next = NextStep {
            what: "Use it",
            command: format!(
                "voltage wallets list --profile {}{config_dir}",
                config::shell_word(&name)
            ),
        };
        let created = Envelope::local(ProfileCreated {
            profile: name,
            created: true,
        })?;
        return Ok(Envelope {
            next: Some(next),
            ..created
        });
    }
    let _lock = settings.lock(terminal).await?;
    settings.reload()?;
    if let ProfileCommand::Delete { name } = command {
        if settings.config.profiles.remove(&name).is_none() {
            return Err(settings.unknown_profile(&name));
        }
        settings.save()?;
        return Envelope::local(ProfileDeleted {
            profile: name,
            deleted: true,
        });
    }
    match command {
        ProfileCommand::Get { name } => {
            let profile = settings.config.profiles.get(&name);
            Envelope::local(profile.ok_or_else(|| settings.unknown_profile(&name))?)
        }
        // List; Create and Delete returned above.
        _ => Envelope::local(&settings.config.profiles),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> Command {
        Invocation::try_parse_from(args).unwrap().command
    }

    #[test]
    fn only_price_and_convert_are_price_queries() {
        assert!(matches!(
            PriceQuery::try_from(command(&[
                "voltage",
                "price",
                "--at",
                "2026-09-18T17:30:00Z"
            ])),
            Ok(PriceQuery::Report(PriceFlags { at: Some(_) }))
        ));
        assert!(matches!(
            PriceQuery::try_from(command(&["voltage", "convert", "10", "usd", "--to", "btc"])),
            Ok(PriceQuery::Convert(_))
        ));
        // Any other command comes back unchanged, so `execute` can go on with it.
        assert!(matches!(
            PriceQuery::try_from(command(&["voltage", "context"])),
            Err(Command::Local(LocalCommand::Context))
        ));
        assert!(matches!(
            PriceQuery::try_from(command(&["voltage", "wallets", "list"])),
            Err(Command::Api(_))
        ));
    }
}
