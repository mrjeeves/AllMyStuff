//! `allmystuff-agent` — the CEC technician's command-line tool.
//!
//! ```text
//! allmystuff-agent start   --email sam@cec       # email a sign-in code
//! allmystuff-agent verify  --email sam@cec --code 123456
//! allmystuff-agent whoami
//! allmystuff-agent online                        # available for requests
//! allmystuff-agent queue                         # waiting help sessions
//! allmystuff-agent watch   --accept              # loop: pick up the next one
//! allmystuff-agent accept  <help-id>
//! allmystuff-agent end     <help-id>
//! allmystuff-agent offline
//! allmystuff-agent logout
//! ```
//!
//! Point it at a local mock with `--backend http://127.0.0.1:8787` (the flag
//! is remembered).

use std::process::ExitCode;
use std::time::Duration;

use allmystuff_agent::{fmt_session, watch_once, Config};
use allmystuff_cec::{CecClient, Error, ReqwestTransport};

#[tokio::main]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = take_positional(&mut args) else {
        print_help();
        return ExitCode::SUCCESS;
    };
    if matches!(cmd.as_str(), "-h" | "--help" | "help") {
        print_help();
        return ExitCode::SUCCESS;
    }

    let path = match Config::default_path() {
        Some(p) => p,
        None => {
            eprintln!("can't locate a home directory (set MYOWNMESH_HOME or HOME)");
            return ExitCode::FAILURE;
        }
    };
    let mut config = Config::load(&path);

    // `--backend <url>` overrides and is remembered.
    if let Some(backend) = take_flag(&mut args, "--backend") {
        config.backend_url = backend.trim_end_matches('/').to_string();
        let _ = config.save(&path);
    }

    match run(&cmd, &mut args, &mut config, &path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {}", describe(&e));
            ExitCode::FAILURE
        }
    }
}

async fn run(
    cmd: &str,
    args: &mut Vec<String>,
    config: &mut Config,
    path: &std::path::Path,
) -> Result<(), Error> {
    let transport = ReqwestTransport::new(&config.backend_url)?;
    let mut client = CecClient::with_token(transport, config.token.clone());

    match cmd {
        "start" => {
            let email = require_flag(args, "--email")?;
            let resp = client.start_sign_in(&email).await?;
            config.email = Some(email.clone());
            let _ = config.save(path);
            let masked = resp.masked_email.unwrap_or(email);
            println!("Sign-in code sent to {masked}.");
            println!("Then run:  allmystuff-agent verify --email {masked} --code <code>");
            Ok(())
        }
        "verify" => {
            let email = require_flag(args, "--email")?;
            let code = require_flag(args, "--code")?;
            let session = client.verify_sign_in(&email, &code, None, None).await?;
            config.token = Some(session.token);
            config.email = Some(session.account.email.clone());
            config.save(path).map_err(io)?;
            let role = if session.account.is_agent() {
                "agent"
            } else {
                "customer only (this account can't take help sessions yet)"
            };
            println!("Signed in as {} — {role}.", session.account.email);
            Ok(())
        }
        "whoami" => {
            let me = client.me().await?;
            println!("{} ({})", me.account.display_name, me.account.email);
            println!(
                "roles: {}",
                me.account
                    .roles
                    .iter()
                    .map(|r| format!("{r:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            Ok(())
        }
        "online" => {
            let p = client.set_presence(true).await?;
            println!(
                "You're online. {}",
                if p.online { "Available." } else { "" }
            );
            Ok(())
        }
        "offline" => {
            client.set_presence(false).await?;
            println!("You're offline.");
            Ok(())
        }
        "queue" => {
            let q = client.agent_queue().await?;
            if q.is_empty() {
                println!("No one's waiting.");
            } else {
                println!("{} waiting:", q.len());
                for s in &q {
                    println!("{}", fmt_session(s));
                }
            }
            Ok(())
        }
        "accept" => {
            let id = require_positional(args, "<help-id>")?;
            let a = client.accept_help(&id).await?;
            println!(
                "Accepted {} for {}. Joining as the CEC Service node on {}.",
                a.session.id, a.session.customer_label, a.session.network_id
            );
            println!("  room:  {}", a.session.room_id);
            println!("  venue: {} signaling server(s)", a.venue.signaling.len());
            Ok(())
        }
        "decline" => {
            let id = require_positional(args, "<help-id>")?;
            client.decline_help(&id).await?;
            println!("Declined {id}; it stays in the queue for another agent.");
            Ok(())
        }
        "end" => {
            let id = require_positional(args, "<help-id>")?;
            client.end_help(&id).await?;
            println!("Ended {id}.");
            Ok(())
        }
        "watch" => {
            let accept = has_flag(args, "--accept");
            let interval = take_flag(args, "--interval")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5)
                .max(1);
            client.set_presence(true).await?;
            println!(
                "Online and watching the queue every {interval}s{}. Ctrl-C to stop.",
                if accept { ", auto-accepting" } else { "" }
            );
            loop {
                match watch_once(&client, accept).await {
                    Ok(report) => {
                        if let Some(a) = report.accepted {
                            println!(
                                "→ picked up {} for {} (room {})",
                                a.session.id, a.session.customer_label, a.session.room_id
                            );
                        } else if report.queued.is_empty() {
                            // quiet
                        } else {
                            println!("{} waiting:", report.queued.len());
                            for s in &report.queued {
                                println!("{}", fmt_session(s));
                            }
                        }
                    }
                    Err(e) => eprintln!("  (queue error: {})", describe(&e)),
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        }
        "logout" => {
            let _ = client.sign_out().await;
            config.token = None;
            config.save(path).map_err(io)?;
            println!("Signed out.");
            Ok(())
        }
        "config" => {
            println!("backend: {}", config.backend_url);
            println!(
                "signed in: {}",
                if config.is_signed_in() {
                    config.email.clone().unwrap_or_else(|| "yes".into())
                } else {
                    "no".into()
                }
            );
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}\n");
            print_help();
            Err(Error::Transport("unknown command".into()))
        }
    }
}

// --- tiny arg helpers (no clap dependency) ---------------------------------

fn take_positional(args: &mut Vec<String>) -> Option<String> {
    let idx = args.iter().position(|a| !a.starts_with('-'))?;
    Some(args.remove(idx))
}

fn require_positional(args: &mut Vec<String>, name: &str) -> Result<String, Error> {
    take_positional(args).ok_or_else(|| Error::Transport(format!("missing {name}")))
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == flag)?;
    args.remove(idx); // the flag
    if idx < args.len() {
        Some(args.remove(idx)) // its value
    } else {
        None
    }
}

fn require_flag(args: &mut Vec<String>, flag: &str) -> Result<String, Error> {
    take_flag(args, flag).ok_or_else(|| Error::Transport(format!("missing {flag} <value>")))
}

fn has_flag(args: &mut Vec<String>, flag: &str) -> bool {
    if let Some(idx) = args.iter().position(|a| a == flag) {
        args.remove(idx);
        true
    } else {
        false
    }
}

fn io(e: std::io::Error) -> Error {
    Error::Transport(format!("local config: {e}"))
}

fn describe(e: &Error) -> String {
    match e {
        Error::Api {
            code: Some(c),
            message,
            ..
        } => format!("{message} ({c})"),
        _ => e.to_string(),
    }
}

fn print_help() {
    eprintln!(
        "allmystuff-agent — the CEC technician's tool\n\n\
         USAGE:\n  allmystuff-agent <command> [--backend <url>] [args]\n\n\
         COMMANDS:\n  \
         start   --email <e>              email a one-time sign-in code\n  \
         verify  --email <e> --code <c>   complete sign-in\n  \
         whoami                           show the signed-in account\n  \
         online | offline                 set availability\n  \
         queue                            list waiting help sessions\n  \
         watch   [--accept] [--interval <s>]  loop the queue (optionally take the next)\n  \
         accept  <help-id>                take a session\n  \
         decline <help-id>                pass on a session\n  \
         end     <help-id>                end a session you're handling\n  \
         logout                           forget the session\n  \
         config                           show current backend + sign-in state\n"
    );
}
