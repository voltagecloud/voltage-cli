//! Process composition: parse the command line, run one command, report, and exit.

use crate::{
    Error, Result,
    api::{self, Execution, SubmissionState},
    auth::{self, Discovery},
    cli::{
        self, AuthCommand, Command, GlobalFlags, Invocation, LocalCommand, ParseFailure,
        ProfileCommand,
    },
    config::{self, Profile, Scope, Settings},
    context,
    output::{Envelope, Output, OutputFormat, report_error, write_stdout},
    price::{PRICE_URL, PriceService},
    terminal::Terminal,
};
use serde::Serialize;
use std::process::ExitCode;

const BINARY_NAME: &str = "voltage";

pub async fn run() -> ExitCode {
    let json_errors = cli::json_errors_requested(std::env::args_os());
    let invocation = match Invocation::from_env() {
        Ok(invocation) => invocation,
        Err(failure) => return fail_parse(failure, json_errors),
    };
    let format = invocation.global.output_format();
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
        Err(error) => fail(error, format),
    }
}

fn fail_parse(failure: ParseFailure, json_errors: bool) -> ExitCode {
    match failure {
        ParseFailure::Validation(error) => fail(error, OutputFormat::select(json_errors, None)),
        ParseFailure::Clap(error) if error.use_stderr() && json_errors => {
            fail(Error::usage(error.to_string()), OutputFormat::Json)
        }
        ParseFailure::Clap(error) => {
            let writes_stdout = !error.use_stderr();
            let exit_code = error.exit_code();
            match error.print() {
                Ok(()) => process_exit(exit_code),
                Err(io) if writes_stdout && io.kind() == std::io::ErrorKind::BrokenPipe => {
                    ExitCode::SUCCESS
                }
                Err(io) => fail(io.into(), OutputFormat::select(false, None)),
            }
        }
    }
}

fn fail(error: Error, format: OutputFormat) -> ExitCode {
    report_error(&error, format);
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
    let terminal = global.terminal();
    // `reserve` is false for a description, which goes to stdout and never reserves the file.
    let output = |reserve: bool| {
        Output::new(
            global.output_format(),
            global.show_secrets,
            global.output_file.as_deref().filter(|_| reserve),
        )
    };
    // Price commands need neither configuration nor credentials.
    if let Command::Local(LocalCommand::Price(_) | LocalCommand::Convert(_)) = &command {
        let mut out = output(true)?;
        let service = PriceService::new(
            global.price_url.as_deref().unwrap_or(PRICE_URL),
            global.timeout,
        )?;
        const WAITING: &str = "Waiting for price service...";
        let envelope = match command {
            Command::Local(LocalCommand::Price(flags)) => Envelope::local(
                terminal
                    .during(WAITING, service.report(flags.at.as_deref()))
                    .await?,
            )?,
            Command::Local(LocalCommand::Convert(flags)) => Envelope::local(
                terminal
                    .during(
                        WAITING,
                        service.convert(&flags.request(), flags.at.as_deref()),
                    )
                    .await?,
            )?,
            _ => unreachable!("matched above"),
        };
        return out.write(envelope, &[]);
    }
    let mut settings = Settings::open(config::directory(global.config_dir.clone())?)?;
    let scope = settings.scope(global.scope_selection())?;
    match command {
        Command::Api(api) => {
            let execution = Execution::resolve(&api, config::execute_variable()?);
            let mut out = output(!execution.describes())?;
            api::execute(
                &api, execution, &global, &settings, &scope, &mut out, submission,
            )
            .await
        }
        Command::Local(local) => {
            let mut out = output(true)?;
            let envelope = local_command(local, &global, &mut settings, &scope, submission).await?;
            out.write(envelope, &[])
        }
    }
}

async fn local_command(
    command: LocalCommand,
    global: &GlobalFlags,
    settings: &mut Settings,
    scope: &Scope,
    submission: &SubmissionState,
) -> Result<Envelope> {
    match command {
        LocalCommand::Login(flags) => {
            Envelope::local(auth::login(settings, &flags, global, submission).await?)
        }
        LocalCommand::Logout(flags) => Envelope::local(
            auth::logout(settings, scope, flags.local, global.terminal(), submission).await?,
        ),
        LocalCommand::Auth {
            command: AuthCommand::Status,
        } => Envelope::local(auth::status(settings, scope, global)?),
        LocalCommand::Auth {
            command: AuthCommand::ImportKey(flags),
        } => Envelope::local(auth::import_key(settings, scope, &flags, global).await?),
        LocalCommand::Organizations { .. } => Envelope::local(
            auth::discover(
                settings,
                scope,
                global,
                Discovery::Organizations,
                submission,
            )
            .await?,
        ),
        LocalCommand::Environments { .. } => Envelope::local(
            auth::discover(settings, scope, global, Discovery::Environments, submission).await?,
        ),
        LocalCommand::Context => Envelope::settings(context::inspect(settings, global)?),
        LocalCommand::Profiles { command } => {
            profiles(command, settings, scope, global.terminal()).await
        }
        LocalCommand::Completions { .. } | LocalCommand::Price(_) | LocalCommand::Convert(_) => {
            Err(Error::usage("This command needs no configuration"))
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

/// Profile edits take the credential lock and re-read the configuration first, so two
/// concurrent commands never overwrite each other's changes.
async fn profiles(
    command: ProfileCommand,
    settings: &mut Settings,
    scope: &Scope,
    terminal: Terminal,
) -> Result<Envelope> {
    let _lock = settings.lock(terminal).await?;
    settings.reload()?;
    let unknown = || Error::usage("Unknown profile");
    match command {
        ProfileCommand::List => Envelope::local(&settings.config.profiles),
        ProfileCommand::Get { name } => {
            Envelope::local(settings.config.profiles.get(&name).ok_or_else(unknown)?)
        }
        ProfileCommand::Create { name } => {
            if settings.config.profiles.contains_key(&name) {
                return Err(Error::usage("Profile already exists"));
            }
            let profile = Profile {
                account: settings.account_name(scope)?,
                organization_id: scope.require_org()?,
                environment_id: scope.single_env()?,
            };
            settings.config.profiles.insert(name.clone(), profile);
            settings.save()?;
            Envelope::local(ProfileCreated {
                profile: name,
                created: true,
            })
        }
        ProfileCommand::Delete { name } => {
            if settings.config.profiles.remove(&name).is_none() {
                return Err(unknown());
            }
            settings.save()?;
            Envelope::local(ProfileDeleted {
                profile: name,
                deleted: true,
            })
        }
    }
}
