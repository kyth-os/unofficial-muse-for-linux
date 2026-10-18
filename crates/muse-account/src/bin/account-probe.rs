//! Read-only, counts-only integration check; deliberately sending is opt-in.
use muse_account::{AccountClient, AccountError, BridgeConfig};
use std::{env, path::PathBuf, process::ExitCode};

fn probe(client: &AccountClient) -> Result<(), AccountError> {
    let snapshot = client.connect()?;
    let messages = client.history(None, 20)?;
    let tasks = client.tasks()?;
    println!(
        "connected=true threads={} history_messages={} schedules={} runs={}",
        snapshot.threads.len(),
        messages.len(),
        tasks.schedules.len(),
        tasks.runs.len()
    );
    // No inherited text variable alone can turn this read-only probe into a send.
    if env::var("MUSE_ACCOUNT_PROBE_ALLOW_SEND").as_deref() == Ok("1")
        && let Ok(text) = env::var("MUSE_ACCOUNT_PROBE_SEND_TEXT")
    {
        let thread = env::var("MUSE_ACCOUNT_PROBE_THREAD").ok();
        let receipt = client.send(thread.as_deref(), &text)?;
        println!(
            "send_attempts=1 sent={} confirmed={}",
            receipt.sent, receipt.confirmed
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() != 3 {
        eprintln!("Usage: account-probe /absolute/python /absolute/bridge.py /absolute/config-dir");
        return ExitCode::from(2);
    }
    let client = AccountClient::new(BridgeConfig {
        python: PathBuf::from(&args[0]),
        script: PathBuf::from(&args[1]),
        config_dir: PathBuf::from(&args[2]),
    });
    match probe(&client) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
