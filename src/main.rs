use clap::ArgMatches;
use serde_json::{Value, json};
use voltage_cli::{
    Error, Result, api, auth, cli,
    config::{Profile, Settings},
    output::{self, Output},
    registry::OPERATIONS,
};

#[tokio::main]
async fn main() {
    let matches = cli::command().get_matches();
    let json_output = cli::enabled(cli::leaf(&matches).1, "json")
        || cli::value(cli::leaf(&matches).1, "output").is_some_and(|v| v != "table")
        || !std::io::IsTerminal::is_terminal(&std::io::stdout());
    let result = tokio::select! {result=run(&matches)=>result,_=tokio::signal::ctrl_c()=>Err(Error::new(130,"Interrupted; any submitted payment continues independently. Use its original ID to check status."))};
    if let Err(error) = result {
        output::report_error(&error, json_output);
        std::process::exit(error.code);
    }
}
async fn run(matches: &ArgMatches) -> Result<()> {
    let (path, m) = cli::leaf(matches);
    if path == ["completions"] {
        clap_complete::generate(
            *m.get_one::<clap_complete::Shell>("shell").unwrap(),
            &mut cli::command(),
            "voltage",
            &mut std::io::stdout(),
        );
        return Ok(());
    }
    let mut settings = Settings::load(m)?;
    let scope = settings.scope(m)?;
    let mut out = Output::new(m)?;
    let result: Value = match path
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["login"] => auth::login(&mut settings, m).await?,
        ["logout"] => auth::logout(&mut settings, &scope, m).await?,
        ["auth", "import-key"] => auth::import_key(&mut settings, &scope, m).await?,
        ["auth", "status"] => {
            if cli::value(m, "profile").is_none()
                && cli::value(m, "account").is_none()
                && std::env::var_os("VOLTAGE_API_KEY").is_some()
            {
                json!({"source":"VOLTAGE_API_KEY","kind":"api_key","configured":std::env::var("VOLTAGE_API_KEY").is_ok_and(|v|!v.trim().is_empty())})
            } else {
                let name = settings.account_name(&scope)?;
                let credential = settings.read_credential(&name)?;
                json!({"account":name,"kind":settings.config.accounts[&name].kind,"email":credential.email,"expires_at":credential.expires_at,"expired":credential.expires_at.is_some_and(|v|v<=auth::now())})
            }
        }
        ["organizations", "list"] => auth::discover(&settings, &scope, m, true).await?,
        ["environments", "list"] => auth::discover(&settings, &scope, m, false).await?,
        ["profiles", action] => {
            let _lock = settings.lock().await?;
            settings.config = Settings::load(m)?.config;
            match *action {
                "list" => serde_json::to_value(&settings.config.profiles)?,
                "get" => serde_json::to_value(
                    settings
                        .config
                        .profiles
                        .get(&cli::value(m, "name").unwrap())
                        .ok_or_else(|| Error::usage("Unknown profile"))?,
                )?,
                "create" => {
                    let name = cli::value(m, "name").unwrap();
                    if settings.config.profiles.contains_key(&name) {
                        return Err(Error::usage("Profile already exists"));
                    }
                    let account = settings.account_name(&scope)?;
                    let org = scope
                        .org
                        .clone()
                        .ok_or_else(|| Error::usage("--org is required"))?;
                    let env = voltage_cli::input::single_env(&scope)?.to_owned();
                    settings.config.profiles.insert(
                        name.clone(),
                        Profile {
                            organization_id: org,
                            environment_id: env,
                            account,
                        },
                    );
                    settings.save()?;
                    json!({"profile":name,"created":true})
                }
                "delete" => {
                    let name = cli::value(m, "name").unwrap();
                    if settings.config.profiles.remove(&name).is_none() {
                        return Err(Error::usage("Unknown profile"));
                    }
                    settings.save()?;
                    json!({"profile":name,"deleted":true})
                }
                _ => return Err(Error::usage("Unknown profile command")),
            }
        }
        _ => {
            let op = if path == ["payments", "send"] || path == ["payments", "receive"] {
                voltage_cli::registry::operation("create_payment")
            } else {
                OPERATIONS
                    .iter()
                    .find(|o| o.command == path)
                    .ok_or_else(|| Error::usage("Unknown command"))?
            };
            return api::execute(op, &path, m, &settings, &scope, &mut out).await;
        }
    };
    out.write(output::envelope(None, result, None, "succeeded"), &[])
}
