use anyhow::{anyhow, ensure};
use api::{
    get_last_deployed_slot, get_remote_job, get_remote_status, send_job_with_uploader_to_remote,
};
use base64::{prelude::BASE64_STANDARD, Engine};
use bincode::serialize;
use cargo_lock::Lockfile;
use cargo_toml::{Manifest, Value};
use clap::{App, AppSettings, Arg, ArgMatches, SubCommand};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::Signals,
};
use solana_address::Address;
use solana_cli_config::{Config, CONFIG_FILE};
use solana_loader_v3_interface::{get_program_data_address, state::UpgradeableLoaderState};
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable};
use solana_transaction_status_client_types::UiTransactionEncoding;
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::sleep,
    time::Duration,
};
use uuid::Uuid;
pub mod api;
#[rustfmt::skip]
pub mod image_config;
pub mod solana_program;
use image_config::IMAGE_MAP;

#[cfg(test)]
mod test;

use crate::solana_program::{
    compose_transaction, find_build_params_pda, get_all_pdas_available, get_program_pda,
    process_close, resolve_rpc_url, upload_program_verification_data, validate_config_and_keypair,
    InputParams, OtterBuildParams, OtterVerifyInstructions,
};

const MAINNET_GENESIS_HASH: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
const MAX_RETRIES: u32 = 3;
const INITIAL_RETRY_DELAY_MS: u64 = 500;

pub fn get_network(network_str: &str) -> &str {
    match network_str {
        "testnet" | "test" | "t" => "https://api.testnet.solana.com",
        "devnet" | "dev" | "d" => "https://api.devnet.solana.com",
        "mainnet" | "main" | "m" | "mainnet-beta" => "https://api.mainnet-beta.solana.com",
        "localnet" | "localhost" | "l" | "local" => "http://localhost:8899",
        _ => network_str,
    }
}

// At the top level, make the signal handler accessible throughout the program
lazy_static::lazy_static! {
    static ref SIGNAL_RECEIVED: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Handle SIGTERM and SIGINT gracefully by stopping the docker container
    let mut signals = Signals::new([SIGTERM, SIGINT])?;
    let mut container_id: Option<String> = None;
    let mut temp_dir: Option<String> = None;

    let handle = signals.handle();
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            SIGNAL_RECEIVED.store(true, Ordering::Relaxed);
        }
    });

    // Add a function to check if we should abort
    let check_signal = |container_id: &mut Option<String>, temp_dir: &mut Option<String>| {
        if SIGNAL_RECEIVED.load(Ordering::Relaxed) {
            println!("\nReceived interrupt signal, cleaning up...");

            if let Some(container_id) = container_id.take() {
                if std::process::Command::new("docker")
                    .args(["kill", &container_id])
                    .output()
                    .is_err()
                {
                    println!("Failed to close docker container");
                } else {
                    println!("Stopped container {container_id}")
                }
            }

            if let Some(temp_dir) = temp_dir.take() {
                if std::process::Command::new("rm")
                    .args(["-rf", &temp_dir])
                    .output()
                    .is_err()
                {
                    println!("Failed to remove temporary directory");
                } else {
                    println!("Removed temporary directory {temp_dir}");
                }
            }

            std::process::exit(130);
        }
    };

    let matches = App::new("solana-verify")
        .author("Ellipsis Labs <maintainers@ellipsislabs.xyz>")
        .version(env!("CARGO_PKG_VERSION"))
        .about("A CLI tool for building verifiable Solana programs")
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .arg(Arg::with_name("url")
            .short("u")
            .long("url")
            .global(true)
            .takes_value(true)
            .help("Optionally include your RPC endpoint. Defaults to Solana CLI config file"))
        .arg(Arg::with_name("compute-unit-price")
            .long("compute-unit-price")
            .global(true)
            .takes_value(true)
            .default_value("100000")
            .help("Priority fee in micro-lamports per compute unit"))
        .arg(Arg::with_name("config")
            .short("c")
            .long("config")
            .global(true)
            .takes_value(true)
            .help("Specify a custom configuration file path"))
        .subcommand(SubCommand::with_name("build")
            .about("Deterministically build the program in a Docker container")
            .arg(Arg::with_name("mount-directory")
                .help("Path to mount to the docker image")
                .takes_value(true))
            .arg(Arg::with_name("workspace-path")
                .long("workspace-path")
                .takes_value(true)
                .help("Path to the workspace root (for monorepos). Defaults to mount path. Use when the program is in a separate workspace that references other crates in the repo."))
            .arg(Arg::with_name("library-name")
                .long("library-name")
                .takes_value(true)
                .help("Which binary file to build"))
            .arg(Arg::with_name("base-image")
                .short("b")
                .long("base-image")
                .takes_value(true)
                .help("Optionally specify a custom base docker image to use for building"))
            .arg(Arg::with_name("bpf")
                .long("bpf")
                .help("If the program requires cargo build-bpf (instead of cargo build-sbf), set this flag"))
            .arg(Arg::with_name("arch")
                .long("arch")
                .takes_value(true)
                .possible_values(&["v0", "v1", "v2", "v3"])
                .help("Build for the given target architecture [default: v0]"))
            .arg(Arg::with_name("cargo-build-sbf-args")
                .long("cargo-build-sbf-args")
                .takes_value(true)
                .require_equals(true)
                .value_name("ARGS")
                .help("Arguments to pass to the underlying `cargo build-sbf` command"))
            .arg(Arg::with_name("cargo-args")
                .multiple(true)
                .last(true)
                .help("Arguments to pass to the underlying `cargo` command")))
        .subcommand(SubCommand::with_name("verify-from-image")
            .about("Verifies a cached build from a docker image")
            .arg(Arg::with_name("executable-path-in-image")
                .short("e")
                .long("executable-path-in-image")
                .takes_value(true)
                .required(true)
                .help("Path to the executable solana program within the source code repository in the docker image"))
            .arg(Arg::with_name("image")
                .short("i")
                .long("image")
                .takes_value(true)
                .required(true)
                .help("Image that contains the source code to be verified"))
            .arg(Arg::with_name("program-id")
                .short("p")
                .long("program-id")
                .takes_value(true)
                .required(true)
                .help("The Program ID of the program to verify"))
            .arg(Arg::with_name("current-dir")
                .long("current-dir")
                .help("Verify in current directory")))
        .subcommand(SubCommand::with_name("get-executable-hash")
            .about("Get the hash of a program binary from an executable file")
            .arg(Arg::with_name("filepath")
                .required(true)
                .help("Path to the executable solana program")))
        .subcommand(SubCommand::with_name("get-program-hash")
            .about("Get the hash of a program binary from the deployed on-chain program")
            .arg(Arg::with_name("program-id")
                .required(true)
                .help("The Program ID of the program to verify")))
        .subcommand(SubCommand::with_name("get-buffer-hash")
            .about("Get the hash of a program binary from the deployed buffer address")
            .arg(Arg::with_name("buffer-address")
                .required(true)
                .help("Address of the buffer account containing the deployed program data")))
        .subcommand(SubCommand::with_name("verify-from-repo")
            .about("Builds and verifies a program from a given repository URL and a program ID")
            .arg(Arg::with_name("remote")
                .long("remote")
                .help("Send the verify command to a remote machine")
                .default_value("false")
                .takes_value(false))
            .arg(Arg::with_name("mount-path")
                .long("mount-path")
                .takes_value(true)
                .default_value("")
                .help("Relative path to the root directory or the source code repository from which to build the program"))
            .arg(Arg::with_name("workspace-path")
                .long("workspace-path")
                .takes_value(true)
                .default_value("")
                .help("Relative path to the program workspace (for monorepos). Use when the program is in a separate workspace that references other crates. Defaults to mount-path."))
            .arg(Arg::with_name("repo-url")
                .required(true)
                .help("The HTTPS URL of the repo to clone"))
            .arg(Arg::with_name("commit-hash")
                .long("commit-hash")
                .takes_value(true)
                .help("Commit hash to checkout. Required to know the correct program snapshot. Will fallback to HEAD if not provided"))
            .arg(Arg::with_name("program-id")
                .long("program-id")
                .required(true)
                .takes_value(true)
                .help("The Program ID of the program to verify"))
            .arg(Arg::with_name("base-image")
                .short("b")
                .long("base-image")
                .takes_value(true)
                .help("Optionally specify a custom base docker image to use for building"))
            .arg(Arg::with_name("library-name")
                .long("library-name")
                .takes_value(true)
                .help("Specify the name of the library to build and verify"))
            .arg(Arg::with_name("bpf")
                .long("bpf")
                .help("If the program requires cargo build-bpf (instead of cargo build-sbf), set this flag"))
            .arg(Arg::with_name("arch")
                .long("arch")
                .takes_value(true)
                .possible_values(&["v0", "v1", "v2", "v3"])
                .help("Build for the given target architecture [default: v0]"))
            .arg(Arg::with_name("current-dir")
                .long("current-dir")
                .help("Verify in current directory"))
            .arg(Arg::with_name("skip-prompt")
                .short("y")
                .long("skip-prompt")
                .help("Skip the prompt to write verify data on chain without user confirmation"))
            .arg(Arg::with_name("keypair")
                .short("k")
                .long("keypair")
                .takes_value(true)
                .help("Optionally specify a keypair to use for uploading the program verification args"))
            .arg(Arg::with_name("cargo-build-sbf-args")
                .long("cargo-build-sbf-args")
                .takes_value(true)
                .require_equals(true)
                .value_name("ARGS")
                .help("Arguments to pass to the underlying `cargo build-sbf` command"))
            .arg(Arg::with_name("cargo-args")
                .multiple(true)
                .last(true)
                .help("Arguments to pass to the underlying `cargo build-sbf` command"))
            .arg(Arg::with_name("skip-build")
                .long("skip-build")
                .help("Skip building and verification, only upload the PDA")
                .takes_value(false)))
        .subcommand(SubCommand::with_name("export-pda-tx")
            .about("Export the transaction as base58 for use with Squads")
            .arg(Arg::with_name("uploader")
                .long("uploader")
                .takes_value(true)
                .required(true)
                .help("Specifies an address to use for uploading the program verification args (should be the program authority)"))
            .arg(Arg::with_name("encoding")
                .long("encoding")
                .takes_value(true)
                .default_value("base58")
                .possible_values(&["base58", "base64"])
                .help("The encoding to use for the transaction"))   
            .arg(Arg::with_name("mount-path")
                .long("mount-path")
                .takes_value(true)
                .default_value("")
                .help("Relative path to the root directory or the source code repository from which to build the program"))
            .arg(Arg::with_name("workspace-path")
                .long("workspace-path")
                .takes_value(true)
                .default_value("")
                .help("Relative path to the program workspace (for monorepos). Use when the program is in a separate workspace that references other crates. Defaults to mount-path."))
            .arg(Arg::with_name("repo-url")
                .required(true)
                .help("The HTTPS URL of the repo to clone"))
            .arg(Arg::with_name("commit-hash")
                .long("commit-hash")
                .takes_value(true)
                .help("Commit hash to checkout. Required to know the correct program snapshot. Will fallback to HEAD if not provided"))
            .arg(Arg::with_name("program-id")
                .long("program-id")
                .required(true)
                .takes_value(true)
                .help("The Program ID of the program to verify"))
            .arg(Arg::with_name("base-image")
                .short("b")
                .long("base-image")
                .takes_value(true)
                .help("Optionally specify a custom base docker image to use for building"))
            .arg(Arg::with_name("library-name")
                .long("library-name")
                .takes_value(true)
                .help("Specify the name of the library to build and verify"))
            .arg(Arg::with_name("bpf")
                .long("bpf")
                .help("If the program requires cargo build-bpf (instead of cargo build-sbf), set this flag"))
            .arg(Arg::with_name("arch")
                .long("arch")
                .takes_value(true)
                .possible_values(&["v0", "v1", "v2", "v3"])
                .help("Build for the given target architecture [default: v0]"))
            .arg(Arg::with_name("cargo-build-sbf-args")
                .long("cargo-build-sbf-args")
                .takes_value(true)
                .require_equals(true)
                .value_name("ARGS")
                .help("Arguments to pass to the underlying `cargo build-sbf` command"))
            .arg(Arg::with_name("cargo-args")
                .multiple(true)
                .last(true)
                .help("Arguments to pass to the underlying `cargo build-sbf` command")))
        .subcommand(SubCommand::with_name("close")
            .about("Close the otter-verify PDA account associated with the given program ID")
            .arg(Arg::with_name("program-id")
                .long("program-id")
                .required(true)
                .takes_value(true)
                .help("The address of the program to close the PDA")))
            .arg(Arg::with_name("export")
                .long("export")
                .required(false)
                .help("Print the transaction as base58 for use with Squads"))
        .subcommand(SubCommand::with_name("list-program-pdas")
            .about("List all the PDA information associated with a program ID. Requires custom RPC endpoint")
            .arg(Arg::with_name("program-id")
                .long("program-id")
                .required(true)
                .takes_value(true)))
        .subcommand(SubCommand::with_name("get-program-pda")
            .about("Get uploaded PDA information for a given program ID and signer")
            .arg(Arg::with_name("program-id")
                .long("program-id")
                .required(true)
                .takes_value(true)
            )
            .arg(Arg::with_name("signer")
                .short("s")
                .long("signer")
                .required(false)
                .takes_value(true)
                .help("Signer to get the PDA for")
            )
        )
        .subcommand(SubCommand::with_name("remote")
            .about("Send a command to a remote machine")
        .setting(AppSettings::SubcommandRequiredElseHelp)
            .subcommand(SubCommand::with_name("get-status")
                .about("Get the verification status of a program")
                .arg(Arg::with_name("program-id")
                    .long("program-id")
                    .required(true)
                    .takes_value(true)
                    .help("The program address to fetch verification status for")))

            .subcommand(SubCommand::with_name("get-job")
                .about("Get the status of a verification job")
                .arg(Arg::with_name("job-id")
                    .long("job-id")
                    .required(true)
                    .takes_value(true)))
            .subcommand(SubCommand::with_name("submit-job")
                .about("Submit a verification job with with on-chain information")
                .arg(Arg::with_name("program-id")
                    .long("program-id")
                    .required(true)
                    .takes_value(true))
                .arg(Arg::with_name("uploader")
                    .long("uploader")
                    .required(true)
                    .takes_value(true)
                    .help("This is the address that uploaded verified build information for the program-id")))
        )
        .get_matches();

    // Validate configuration early if custom config is provided
    let config_path = matches.value_of("config").map(|s| s.to_string());
    if config_path.is_some() {
        // Check if verify-from-repo subcommand has a keypair parameter
        let keypair_path = if let ("verify-from-repo", Some(sub_m)) = matches.subcommand() {
            sub_m.value_of("keypair").map(|s| s.to_string())
        } else {
            None
        };
        validate_config_and_keypair(config_path.as_deref(), keypair_path.as_deref())?;
    }

    let connection = resolve_rpc_url(
        matches.value_of("url").map(|s| s.to_string()),
        config_path.clone(),
    )?;
    let res = match matches.subcommand() {
        ("build", Some(sub_m)) => {
            let mount_directory = sub_m.value_of("mount-directory").map(|s| s.to_string());
            let workspace_path = sub_m.value_of("workspace-path").map(|s| s.to_string());
            let library_name = sub_m.value_of("library-name").map(|s| s.to_string());
            let base_image = sub_m.value_of("base-image").map(|s| s.to_string());
            let bpf_flag = sub_m.is_present("bpf");
            let arch = sub_m.value_of("arch").map(|s| s.to_string());
            let cargo_build_sbf_args = sub_m
                .value_of("cargo-build-sbf-args")
                .map(|s| s.to_string());
            let cargo_args = sub_m
                .values_of("cargo-args")
                .unwrap_or_default()
                .map(|s| s.to_string())
                .collect();
            build(
                mount_directory,
                workspace_path,
                library_name,
                base_image,
                bpf_flag,
                arch,
                cargo_build_sbf_args,
                cargo_args,
                &mut container_id,
            )
        }
        ("verify-from-image", Some(sub_m)) => {
            let executable_path = sub_m.value_of("executable-path-in-image").unwrap();
            let image = sub_m.value_of("image").unwrap();
            let program_id = sub_m.value_of("program-id").unwrap();
            let current_dir = sub_m.is_present("current-dir");
            verify_from_image(
                executable_path.to_string(),
                image.to_string(),
                matches.value_of("url").map(|s| s.to_string()),
                config_path.clone(),
                Address::try_from(program_id)?,
                current_dir,
                &mut temp_dir,
                &mut container_id,
            )
        }
        ("get-executable-hash", Some(sub_m)) => {
            let filepath = sub_m.value_of("filepath").map(|s| s.to_string()).unwrap();
            let program_hash = get_file_hash(&filepath)?;
            println!("{program_hash}");
            Ok(())
        }
        ("get-buffer-hash", Some(sub_m)) => {
            let buffer_address = sub_m.value_of("buffer-address").unwrap();
            let buffer_hash = get_buffer_hash(
                matches.value_of("url").map(|s| s.to_string()),
                Address::try_from(buffer_address)?,
            )?;
            println!("{buffer_hash}");
            Ok(())
        }
        ("get-program-hash", Some(sub_m)) => {
            let program_id = sub_m.value_of("program-id").unwrap();
            let program_hash = get_program_hash(&connection, Address::try_from(program_id)?)?;
            println!("{program_hash}");
            Ok(())
        }
        ("verify-from-repo", Some(sub_m)) => {
            let skip_build = sub_m.is_present("skip-build");
            let remote = sub_m.is_present("remote");
            let mount_path = sub_m.value_of("mount-path").map(|s| s.to_string()).unwrap();
            let workspace_path = sub_m
                .value_of("workspace-path")
                .map(|s| s.to_string())
                .unwrap_or_default();
            let repo_url = sub_m.value_of("repo-url").map(|s| s.to_string()).unwrap();
            let program_id = sub_m.value_of("program-id").unwrap();
            if remote {
                return Err(anyhow!(
                    "The --remote flag has been deprecated. Upload your verify PDA with programs upgrade authority, then queue the remote worker with `solana-verify remote submit-job --program-id {program_id} --uploader <UPLOADER>`. See https://solana.com/docs/programs/verified-builds for the full workflow."
                ));
            }
            let base_image = sub_m.value_of("base-image").map(|s| s.to_string());
            let library_name = sub_m.value_of("library-name").map(|s| s.to_string());
            let bpf_flag = sub_m.is_present("bpf");
            let arch = sub_m.value_of("arch").map(|s| s.to_string());
            let current_dir = sub_m.is_present("current-dir");
            let skip_prompt = sub_m.is_present("skip-prompt");
            let path_to_keypair = sub_m.value_of("keypair").map(|s| s.to_string());
            let compute_unit_price = matches
                .value_of("compute-unit-price")
                .unwrap()
                .parse::<u64>()
                .unwrap_or(100000);
            let cargo_build_sbf_args = sub_m
                .value_of("cargo-build-sbf-args")
                .map(|s| s.to_string());
            let cargo_args: Vec<String> = sub_m
                .values_of("cargo-args")
                .unwrap_or_default()
                .map(|s| s.to_string())
                .collect();

            let commit_hash = get_commit_hash(sub_m, &repo_url)?;

            println!("Skipping prompt: {skip_prompt}");
            verify_from_repo(
                mount_path,
                workspace_path,
                &connection,
                repo_url,
                Some(commit_hash),
                Address::try_from(program_id)?,
                base_image,
                library_name,
                bpf_flag,
                arch,
                cargo_build_sbf_args,
                cargo_args,
                current_dir,
                skip_prompt,
                path_to_keypair,
                compute_unit_price,
                skip_build,
                &mut container_id,
                &mut temp_dir,
                &check_signal,
                config_path.clone(),
            )
            .await
        }
        ("close", Some(sub_m)) => {
            let program_id = sub_m.value_of("program-id").unwrap();
            let compute_unit_price = matches
                .value_of("compute-unit-price")
                .unwrap()
                .parse::<u64>()
                .unwrap_or(100000);
            process_close(
                Address::try_from(program_id)?,
                &connection,
                compute_unit_price,
                config_path.clone(),
            )
            .await
        }
        ("export-pda-tx", Some(sub_m)) => {
            let uploader = sub_m.value_of("uploader").unwrap();
            let mount_path = sub_m.value_of("mount-path").map(|s| s.to_string()).unwrap();
            let workspace_path = sub_m
                .value_of("workspace-path")
                .map(|s| s.to_string())
                .unwrap_or_default();
            let repo_url = sub_m.value_of("repo-url").map(|s| s.to_string()).unwrap();
            let program_id = sub_m.value_of("program-id").unwrap();
            let base_image = sub_m.value_of("base-image").map(|s| s.to_string());
            let library_name = sub_m.value_of("library-name").map(|s| s.to_string());
            let bpf_flag = sub_m.is_present("bpf");
            let arch = sub_m.value_of("arch").map(|s| s.to_string());
            let encoding = sub_m.value_of("encoding").unwrap();

            let encoding: UiTransactionEncoding = match encoding {
                "base58" => UiTransactionEncoding::Base58,
                "base64" => UiTransactionEncoding::Base64,
                _ => {
                    return Err(anyhow!("Unsupported encoding: {}", encoding));
                }
            };

            let compute_unit_price = matches
                .value_of("compute-unit-price")
                .unwrap()
                .parse::<u64>()
                .unwrap_or(100000);

            let commit_hash = get_commit_hash(sub_m, &repo_url)?;
            let cargo_build_sbf_args = sub_m
                .value_of("cargo-build-sbf-args")
                .map(|s| s.to_string());
            let cargo_args: Vec<String> = sub_m
                .values_of("cargo-args")
                .unwrap_or_default()
                .map(|s| s.to_string())
                .collect();

            let connection = resolve_rpc_url(
                matches.value_of("url").map(|s| s.to_string()),
                config_path.clone(),
            )?;
            println!("Using connection url: {}", connection.url());

            export_pda_tx(
                &connection,
                Address::try_from(program_id)?,
                Address::try_from(uploader)?,
                repo_url,
                commit_hash,
                mount_path,
                workspace_path,
                library_name,
                base_image,
                bpf_flag,
                arch,
                &mut temp_dir,
                encoding,
                cargo_build_sbf_args,
                cargo_args,
                compute_unit_price,
            )
            .await
        }
        ("list-program-pdas", Some(sub_m)) => {
            let program_id = sub_m.value_of("program-id").unwrap();
            list_program_pdas(Address::try_from(program_id)?, &connection).await
        }
        ("get-program-pda", Some(sub_m)) => {
            let program_id = sub_m.value_of("program-id").unwrap();
            let signer = sub_m.value_of("signer").map(|s| s.to_string());
            print_program_pda(
                Address::try_from(program_id)?,
                signer,
                &connection,
                config_path.clone(),
            )
            .await
        }
        ("remote", Some(sub_m)) => match sub_m.subcommand() {
            ("get-status", Some(sub_m)) => {
                let program_id = sub_m.value_of("program-id").unwrap();
                get_remote_status(Address::try_from(program_id)?).await
            }
            ("get-job", Some(sub_m)) => {
                let job_id = sub_m.value_of("job-id").unwrap();
                get_remote_job(job_id).await
            }
            ("submit-job", Some(sub_m)) => {
                let program_id = sub_m.value_of("program-id").unwrap();
                let uploader = sub_m.value_of("uploader").unwrap();

                send_job_with_uploader_to_remote(
                    &connection,
                    &Address::try_from(program_id)?,
                    &Address::try_from(uploader)?,
                )
                .await
            }
            _ => unreachable!(),
        },
        // Handle other subcommands in a similar manner, for now let's panic
        _ => panic!(
            "Unknown subcommand: {:?}\nUse '--help' to see available commands",
            matches.subcommand().0
        ),
    };

    handle.close();
    res
}

pub fn get_client(url: Option<String>, config_path: Option<String>) -> RpcClient {
    let config = match config_path {
        Some(config_file) => Config::load(&config_file).unwrap_or_else(|_| {
            println!("Failed to load config file: {config_file}");
            Config::default()
        }),
        None => match CONFIG_FILE.as_ref() {
            Some(config_file) => Config::load(config_file).unwrap_or_else(|_| {
                println!("Failed to load config file: {config_file}");
                Config::default()
            }),
            None => Config::default(),
        },
    };
    let url = &get_network(&url.unwrap_or(config.json_rpc_url)).to_string();
    RpcClient::new(url)
}

fn get_commit_hash_from_remote(repo_url: &str) -> anyhow::Result<String> {
    // Fetch the symbolic reference of the default branch
    let output = Command::new("git")
        .arg("ls-remote")
        .arg("--symref")
        .arg(repo_url)
        .output()
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to fetch repository information using git.\nError: {}",
                e
            )
        })?;

    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "Failed to fetch default branch information from repository '{}'.\nGit error: {}",
            repo_url,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Find out if the branch is called master or main
    let output_str = String::from_utf8(output.stdout)?;
    let default_branch = output_str
        .lines()
        .find_map(|line| {
            if line.starts_with("ref: refs/heads/") {
                Some(
                    line.trim_start_matches("ref: refs/heads/")
                        .split_whitespace()
                        .next()?
                        .to_string(),
                )
            } else {
                None
            }
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Unable to determine default branch from repository '{}'",
                repo_url
            )
        })?;

    println!("Default branch detected: {default_branch}");

    // Fetch the latest commit hash for the default branch
    let hash_output = Command::new("git")
        .arg("ls-remote")
        .arg(repo_url)
        .arg(&default_branch)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to fetch commit hash for default branch '{default_branch}' from repository '{repo_url}'.\nError: {e}"))?;

    if !hash_output.status.success() {
        let stderr = String::from_utf8_lossy(&hash_output.stderr);
        return Err(anyhow::anyhow!(
            "Failed to fetch commit hash for branch '{default_branch}' from repository '{repo_url}'.\nGit error: {stderr}"
        ));
    }

    // Parse and return the commit hash
    String::from_utf8(hash_output.stdout)?
        .split_whitespace()
        .next()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to parse commit hash from git ls-remote output"))
}

pub fn get_binary_hash(program_data: Vec<u8>) -> String {
    let buffer = program_data
        .into_iter()
        .rev()
        .skip_while(|&x| x == 0)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    sha256::digest(&buffer[..])
}

pub fn get_file_hash(filepath: &str) -> Result<String, std::io::Error> {
    let mut f = std::fs::File::open(filepath)?;
    let metadata = std::fs::metadata(filepath)?;
    let mut buffer = vec![0; metadata.len() as usize];
    f.read_exact(&mut buffer)?;
    Ok(get_binary_hash(buffer))
}

pub fn get_buffer_hash(url: Option<String>, buffer_address: Address) -> anyhow::Result<String> {
    let client = get_client(url, None);
    let offset = UpgradeableLoaderState::size_of_buffer_metadata();
    let account_data =
        retry_rpc_call(|| Ok(client.get_account_data(&buffer_address)?[offset..].to_vec()))?;
    Ok(get_binary_hash(account_data))
}

pub fn get_program_hash(client: &RpcClient, program_id: Address) -> anyhow::Result<String> {
    let account = retry_rpc_call(|| {
        client
            .get_account(&program_id)
            .map_err(|e| anyhow!("Program {} is not deployed: {}", program_id, e))
    })?;

    let owner = account.owner;

    match owner {
        // Check if the program is owned by the upgradeable loader (Loader-v3)
        // If so, the program data is in a separate program data account
        owner_id if owner_id == bpf_loader_upgradeable::id() => {
            let program_buffer = get_program_data_address(&program_id);

            let data = retry_rpc_call(|| {
                client.get_account_data(&program_buffer).map_err(|e| {
                    anyhow!(
                        "Could not find program data for {}: {}. This could mean:\n\
                     1. The program is not deployed\n\
                     2. The program is not upgradeable\n\
                     3. The program was deployed with a different loader",
                        program_id,
                        e
                    )
                })
            })?;

            let offset = UpgradeableLoaderState::size_of_programdata_metadata();

            let account_data = data
                .get(offset..)
                .ok_or_else(|| {
                    anyhow!(
                        "Program data account appears corrupted or incomplete. Expected at least {} bytes for metadata.",
                        offset
                    )
                })?
                .to_vec();

            Ok(get_binary_hash(account_data))
        }

        // Check if the program is owned by the legacy BPF loaders (v1/v2)
        // If so, the program data is stored in the program account's data
        owner_id if owner_id == bpf_loader_deprecated::id() || owner_id == bpf_loader::id() => {
            let program_data = account.data;

            if program_data.is_empty() {
                return Err(anyhow!(
                    "Program {} has no data (legacy loader account empty)",
                    program_id
                ));
            }

            Ok(get_binary_hash(program_data))
        }

        // Unsupported loader
        _ => Err(anyhow!(
            "Unknown or unsupported program loader. \
             Program {} is owned by {}. Supported loaders: BPF Loader v1, v2, or Upgradeable (loader-v3).",
            program_id,
            owner
        )),
    }
}

pub fn get_genesis_hash(client: &RpcClient) -> anyhow::Result<String> {
    retry_rpc_call(|| {
        let genesis_hash = client.get_genesis_hash()?;
        Ok(genesis_hash.to_string())
    })
}

pub fn get_docker_resource_limits() -> Option<(String, String)> {
    let memory = std::env::var("SVB_DOCKER_MEMORY_LIMIT").ok();
    let cpus = std::env::var("SVB_DOCKER_CPU_LIMIT").ok();
    if memory.is_some() || cpus.is_some() {
        println!("Using docker resource limits: memory: {memory:?}, cpus: {cpus:?}");
    } else {
        // Print message to user that they can set these environment variables to limit docker resources
        println!("No Docker resource limits are set.");
        println!(
            "You can set the SVB_DOCKER_MEMORY_LIMIT and SVB_DOCKER_CPU_LIMIT environment variables to limit Docker resources."
        );
        println!("For example: SVB_DOCKER_MEMORY_LIMIT=2g SVB_DOCKER_CPU_LIMIT=2.");
    }
    memory.zip(cpus)
}

fn setup_offline_build(mount_path: &str) -> anyhow::Result<()> {
    // Run cargo vendor
    let output = std::process::Command::new("cargo")
        .args(["vendor"])
        .current_dir(mount_path)
        .stderr(Stdio::inherit())
        .stdout(Stdio::inherit())
        .output()?;
    ensure!(output.status.success(), "Failed to run cargo vendor");

    // Create .cargo directory if it doesn't exist
    let cargo_dir = format!("{mount_path}/.cargo");
    std::fs::create_dir_all(&cargo_dir)?;

    // Create config.toml with vendored sources configuration
    let config_content = "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n[source.vendored-sources]\ndirectory = \"vendor\"";
    std::fs::write(format!("{cargo_dir}/config.toml"), config_content)?;

    println!("Successfully set up offline build configuration");
    Ok(())
}
#[allow(clippy::too_many_arguments)]
pub fn build(
    mount_directory: Option<String>,
    workspace_root: Option<String>,
    library_name: Option<String>,
    base_image: Option<String>,
    bpf_flag: bool,
    arch: Option<String>,
    cargo_build_sbf_args: Option<String>,
    cargo_args: Vec<String>,
    container_id_opt: &mut Option<String>,
) -> anyhow::Result<()> {
    let mut mount_path = mount_directory.unwrap_or(
        std::env::current_dir()?
            .as_os_str()
            .to_str()
            .ok_or_else(|| anyhow::Error::msg("Invalid path string"))?
            .to_string(),
    );
    mount_path = mount_path.trim_end_matches('/').to_string();
    println!("Mounting path: {mount_path}");

    let workspace_path = workspace_root
        .unwrap_or_else(|| mount_path.clone())
        .trim_end_matches('/')
        .to_string();
    println!("Workspace path: {}", workspace_path);

    let lockfile = format!("{}/Cargo.lock", workspace_path);
    if !std::path::Path::new(&lockfile).exists() {
        println!("Mount directory must contain a Cargo.lock file");
        return Err(anyhow!("Missing Cargo.lock file at '{lockfile}'"));
    }

    // Check if --offline flag is present in cargo_args
    if cargo_args.contains(&"--offline".to_string()) {
        setup_offline_build(&workspace_path)?;
    }

    let build_command = if bpf_flag { "build-bpf" } else { "build-sbf" };

    let mut solana_version: Option<String> = None;
    let (mut major, mut minor, mut patch) = (0, 0, 0);
    let image: String = match base_image {
        Some(base_image) => base_image,
        None => {
            // Resolve Solana version: [workspace.metadata.cli] first, then Cargo.lock fallback
            (major, minor, patch) = get_solana_version_from_workspace_metadata(&mount_path)
                .or_else(|| get_solana_version_from_lockfile(&lockfile).ok())
                .ok_or_else(|| {
                    anyhow!(
                        "Failed to determine Solana version: not found in [workspace.metadata.cli] in Cargo.toml nor in Cargo.lock"
                    )
                })?;
            if bpf_flag {
                // Use this for backwards compatibility with anchor verified builds
                solana_version = Some("v1.13.5".to_string());
                "projectserum/build@sha256:75b75eab447ebcca1f471c98583d9b5d82c4be122c470852a022afcf9c98bead".to_string()
            } else if let Some(digest) = IMAGE_MAP.get(&(major, minor, patch)) {
                println!("Found docker image for Solana version {major}.{minor}.{patch}");
                solana_version = Some(format!("v{major}.{minor}.{patch}"));
                format!("solanafoundation/solana-verifiable-build@{digest}")
            } else {
                return Err(anyhow!(
                    "No compatible Docker image found for Solana version {major}.{minor}.{patch} \nPlease use --base-image flag to specify a compatible Docker image manually"
                ));
            }
        }
    };

    let mut manifest_path = None;

    let relative_build_path = match library_name.as_deref() {
        Some(library_name) => {
            let (manifest_path_for_library_name, build_path) =
                find_relative_manifest_path_and_build_path(&mount_path, library_name)?;
            manifest_path = Some(manifest_path_for_library_name);
            build_path
        }
        None => "".into(),
    };

    let workdir = std::process::Command::new("docker")
        .args(["run", "--rm", &image, "pwd"])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| anyhow::format_err!("Failed to get working directory : {}", e))
        .and_then(parse_output)?;

    println!("Workdir: {workdir}");

    let build_path = format!("{workdir}/{relative_build_path}");
    println!("Building program at {build_path}");

    let manifest_path_filter = manifest_path
        .clone()
        .map(|m| vec!["--manifest-path".to_string(), format!("{workdir}/{m}")])
        .unwrap_or_else(Vec::new);

    if let Some(manifest) = manifest_path.as_ref() {
        println!("Building manifest path: {workdir}/{manifest}");
    }

    // change directory to program/build dir
    let mount_params = format!("{mount_path}:{workdir}");
    let container_id = {
        let mut cmd = std::process::Command::new("docker");
        cmd.args(["run", "--rm", "-v", &mount_params, "-dit"]);
        cmd.stderr(Stdio::inherit());

        if let Some((memory_limit, cpu_limit)) = get_docker_resource_limits() {
            cmd.arg("--memory")
                .arg(memory_limit)
                .arg("--cpus")
                .arg(cpu_limit);
        }

        let output = cmd
            .args([&image, "bash"])
            .output()
            .map_err(|e| anyhow!("Failed to start Docker container : {}", e))?;

        parse_output(output)?
    };

    // Set the container id so we can kill it later if the process is interrupted
    container_id_opt.replace(container_id.clone());

    let active_toolchain = get_container_active_toolchain(&container_id)?;
    println!("Using container Rust toolchain: {active_toolchain}");

    // Solana v1.17 uses Rust 1.73, which defaults to the sparse registry, making
    // this fetch unnecessary, but requires us to omit the "frozen" argument
    let locked_args = if major == 1 && minor < 17 {
        // First, we resolve the dependencies and cache them in the Docker container
        // ARM processors running Linux have a bug where the build fails if the dependencies are not preloaded.
        // Running the build without the pre-fetch will cause the container to run out of memory.
        // This is a workaround for that issue.
        // Set RUSTUP_TOOLCHAIN to the active toolchain in the container
        // so that the dependencies are fetched with the correct toolchain
        let output = std::process::Command::new("docker")
            .args([
                "exec",
                "-e",
                &format!("RUSTUP_TOOLCHAIN={active_toolchain}"),
                &container_id,
            ])
            .args([
                "cargo",
                "--config",
                "net.git-fetch-with-cli=true",
                "fetch",
                "--locked",
            ])
            .stderr(Stdio::inherit())
            .stdout(Stdio::inherit())
            .output()?;
        ensure!(
            output.status.success(),
            "Failed to cargo fetch dependencies"
        );
        println!("Finished fetching build dependencies");

        ["--frozen", "--locked"].as_slice()
    } else {
        // To be totally safe, force the build to use the sparse registry
        [
            "--config",
            "registries.crates-io.protocol=\"sparse\"",
            "--locked",
        ]
        .as_slice()
    };

    let mut cmd = std::process::Command::new("docker");
    // Set RUSTUP_TOOLCHAIN to the active toolchain in the container
    // so that the build is performed with the correct toolchain
    cmd.args([
        "exec",
        "-e",
        &format!("RUSTUP_TOOLCHAIN={active_toolchain}"),
        "-w",
        &build_path,
        &container_id,
    ])
    .args(["cargo", build_command]);

    // Add arch flag if specified
    if let Some(arch_value) = &arch {
        cmd.args(["--arch", arch_value]);
    }

    // Add cargo-build-sbf arguments if present
    if let Some(cargo_build_sbf_args) = &cargo_build_sbf_args {
        cmd.args(cargo_build_sbf_args.split_whitespace());
    }

    let output = cmd
        .args(["--"])
        .args(locked_args)
        .args(manifest_path_filter)
        .args(cargo_args)
        .stderr(Stdio::inherit())
        .stdout(Stdio::inherit())
        .output()?;
    ensure!(output.status.success(), "Failed to cargo build");

    println!("Finished building program");
    println!("Program Solana version: v{major}.{minor}.{patch}");

    if let Some(solana_version) = solana_version {
        println!("Docker image Solana version: {solana_version}");
    }

    if let Some(program_name) = library_name {
        let executable_path = std::process::Command::new("find")
            .args([
                &format!("{}/target/deploy", workspace_path),
                "-name",
                &format!("{program_name}.so"),
            ])
            .output()
            .map_err(|e| anyhow!("Failed to find program: {}", e))
            .and_then(parse_output)?;
        let executable_hash = get_file_hash(&executable_path)?;
        println!("{executable_hash}");
    }
    let output = std::process::Command::new("docker")
        .args(["kill", &container_id])
        .output()?;
    ensure!(output.status.success(), "Failed to find the program binary");

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn verify_from_image(
    executable_path: String,
    image: String,
    network: Option<String>,
    config_path: Option<String>,
    program_id: Address,
    current_dir: bool,
    temp_dir: &mut Option<String>,
    container_id_opt: &mut Option<String>,
) -> anyhow::Result<()> {
    println!("Verifying image: {image:?}, on network {network:?} against program ID {program_id}");
    println!("Executable path in container: {executable_path:?}");
    println!(" ");

    let workdir = std::process::Command::new("docker")
        .args(["run", "--rm", &image, "pwd"])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| anyhow::format_err!("Failed to get working directory : {}", e))
        .and_then(parse_output)?;

    println!("Workdir: {workdir}");

    let container_id = {
        let mut cmd = std::process::Command::new("docker");
        cmd.args(["run", "--rm", "-dit"]);
        cmd.stderr(Stdio::inherit());

        if let Some((memory_limit, cpu_limit)) = get_docker_resource_limits() {
            cmd.arg("--memory")
                .arg(memory_limit)
                .arg("--cpus")
                .arg(cpu_limit);
        }

        let output = cmd
            .args([&image])
            .output()
            .map_err(|e| anyhow!("Failed to start Docker container : {}", e))?;
        parse_output(output)?
    };

    container_id_opt.replace(container_id.clone());

    let uuid = Uuid::new_v4().to_string();

    // Create a temporary directory to clone the repo into
    let verify_dir = if current_dir {
        format!(
            "{}/.{}",
            std::env::current_dir()?
                .as_os_str()
                .to_str()
                .ok_or_else(|| anyhow::Error::msg("Invalid path string"))?,
            uuid.clone()
        )
    } else {
        "/tmp".to_string()
    };

    temp_dir.replace(verify_dir.clone());

    let program_filepath = format!("{verify_dir}/program.so");
    let output = std::process::Command::new("docker")
        .args([
            "cp",
            format!("{container_id}:{workdir}/{executable_path}").as_str(),
            program_filepath.as_str(),
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| anyhow::format_err!("Failed to copy executable file {}", e))?;
    ensure!(output.status.success(), "Failed to copy executable file");

    let executable_hash: String = get_file_hash(program_filepath.as_str())?;
    let client = get_client(network, config_path);
    let program_hash = get_program_hash(&client, program_id)?;
    println!("Executable hash: {}", executable_hash);
    println!("Program hash: {}", program_hash);

    // Cleanup docker and rm file
    std::process::Command::new("docker")
        .args(["kill", container_id.as_str()])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| anyhow::format_err!("Docker kill failed: {}", e))?;

    std::process::Command::new("rm")
        .args([program_filepath])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| anyhow::format_err!("Failed to remove temp program file: {}", e))?;

    if program_hash != executable_hash {
        println!("Executable hash mismatch");
        return Err(anyhow::Error::msg("Executable hash mismatch"));
    } else {
        println!("Executable matches on-chain program data ✅");
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn build_args(
    relative_mount_path: &str,
    relative_workspace_path: &str,
    library_name_opt: Option<String>,
    verify_tmp_root_path: &str,
    base_image: Option<String>,
    bpf_flag: bool,
    arch: Option<String>,
    cargo_build_sbf_args: Option<String>,
    cargo_args: Vec<String>,
) -> anyhow::Result<(Vec<String>, String, String, String)> {
    let mut args: Vec<String> = Vec::new();
    if !relative_mount_path.is_empty() {
        args.push("--mount-path".to_string());
        args.push(relative_mount_path.to_string());
    }
    if !relative_workspace_path.is_empty() {
        args.push("--workspace-path".to_string());
        args.push(relative_workspace_path.to_string());
    }
    // Get the absolute build path to the solana program directory to build inside docker
    let mount_path = PathBuf::from(verify_tmp_root_path).join(relative_mount_path);
    let workspace_path = if relative_workspace_path.is_empty() {
        mount_path.to_str().unwrap().to_string()
    } else {
        PathBuf::from(verify_tmp_root_path)
            .join(relative_workspace_path)
            .to_str()
            .unwrap()
            .to_string()
    };

    args.push("--library-name".to_string());
    let library_name = match library_name_opt.clone() {
        Some(p) => p,
        None => {
            std::process::Command::new("find")
                .args([mount_path.to_str().unwrap(), "-name", "Cargo.toml"])
                .output()
                .map_err(|e| {
                    anyhow::format_err!(
                        "Failed to find Cargo.toml files in root directory: {}",
                        e
                    )
                })
                .and_then(|output| {
                    ensure!(output.status.success(), "Failed to search for Cargo.toml in root directory");
                    let mut options = vec![];
                    for path in String::from_utf8(output.stdout)?.split("\n") {
                        match get_lib_name_from_cargo_toml(path) {
                            Ok(name) => {
                                options.push(name);
                            }
                            Err(_) => {
                                continue;
                            }
                        }
                    }
                    if options.len() != 1 {
                        println!(
                            "Found multiple possible targets in root directory: {options:?}"
                        );
                        println!(
                            "Please explicitly specify the target with the --library-name <name> option"
                        );
                        Err(anyhow::format_err!(
                            "Multiple library targets found: {options:?}"
                        ))
                    } else {
                        Ok(options[0].clone())
                    }
                })?
        }
    };
    args.push(library_name.clone());

    if let Some(base_image) = &base_image {
        args.push("--base-image".to_string());
        args.push(base_image.clone());
    }

    if bpf_flag {
        args.push("--bpf".to_string());
    }

    if let Some(arch_value) = &arch {
        args.push("--arch".to_string());
        args.push(arch_value.clone());
    }

    if let Some(cargo_build_sbf_args) = &cargo_build_sbf_args {
        args.push(format!(
            "{}=\"{}\"",
            "--cargo-build-sbf-args",
            cargo_build_sbf_args.clone()
        ));
    }

    if !cargo_args.is_empty() {
        args.push("--".to_string());
        for arg in &cargo_args {
            args.push(arg.clone());
        }
    }

    Ok((
        args,
        mount_path.to_str().unwrap().to_string(),
        workspace_path,
        library_name,
    ))
}

fn clone_repo_and_checkout(
    repo_url: &str,
    current_dir: bool,
    base_name: &str,
    commit_hash: Option<String>,
    temp_dir_opt: &mut Option<String>,
) -> anyhow::Result<(String, String)> {
    let uuid = Uuid::new_v4().to_string();

    // Create a temporary directory to clone the repo into
    let verify_dir = if current_dir {
        format!(
            "{}/.{}",
            std::env::current_dir()?
                .as_os_str()
                .to_str()
                .ok_or_else(|| anyhow::Error::msg("Invalid path string"))?,
            uuid.clone()
        )
    } else {
        format!("/tmp/solana-verify/{uuid}")
    };

    temp_dir_opt.replace(verify_dir.clone());

    let verify_tmp_root_path = format!("{verify_dir}/{base_name}");
    println!("Cloning repo into: {verify_tmp_root_path}");

    let output = std::process::Command::new("git")
        .args(["clone", repo_url, &verify_tmp_root_path])
        .stdout(Stdio::inherit())
        .output()?;
    ensure!(
        output.status.success(),
        "Failed to clone repository '{}'",
        repo_url
    );

    if let Some(commit_hash) = commit_hash.as_ref() {
        let output = std::process::Command::new("git")
            .args(["-C", &verify_tmp_root_path])
            .args(["checkout", commit_hash])
            .output()
            .map_err(|e| anyhow!("Failed to checkout commit hash '{commit_hash}' : {e:?}"))?;
        if output.status.success() {
            println!("Checked out commit hash: {commit_hash}");
        } else {
            let output = std::process::Command::new("rm")
                .args(["-rf", verify_dir.as_str()])
                .output()?;
            ensure!(
                output.status.success(),
                "Failed to clean up temporary directory"
            );

            Err(anyhow!(
                "Git checkout failed for commit hash '{}'",
                commit_hash
            ))?;
        }
    }

    Ok((verify_tmp_root_path, verify_dir))
}

fn get_basename(repo_url: &str) -> anyhow::Result<String> {
    let base_name = std::process::Command::new("basename")
        .arg(repo_url)
        .output()
        .map_err(|e| {
            anyhow!(
                "Failed to extract repository name from URL '{}' : {:?}",
                repo_url,
                e
            )
        })
        .and_then(parse_output)?;
    Ok(base_name)
}

#[allow(clippy::too_many_arguments)]
pub async fn verify_from_repo(
    relative_mount_path: String,
    relative_workspace_path: String,
    connection: &RpcClient,
    repo_url: String,
    commit_hash: Option<String>,
    program_id: Address,
    base_image: Option<String>,
    library_name_opt: Option<String>,
    bpf_flag: bool,
    arch: Option<String>,
    cargo_build_sbf_args: Option<String>,
    cargo_args: Vec<String>,
    current_dir: bool,
    skip_prompt: bool,
    path_to_keypair: Option<String>,
    compute_unit_price: u64,
    skip_build: bool,
    container_id_opt: &mut Option<String>,
    temp_dir_opt: &mut Option<String>,
    check_signal: &dyn Fn(&mut Option<String>, &mut Option<String>),
    config_path: Option<String>,
) -> anyhow::Result<()> {
    // Get source code from repo_url
    let base_name = get_basename(&repo_url)?;

    check_signal(container_id_opt, temp_dir_opt);

    let (verify_tmp_root_path, verify_dir) = clone_repo_and_checkout(
        &repo_url,
        current_dir,
        &base_name,
        commit_hash.clone(),
        temp_dir_opt,
    )?;

    check_signal(container_id_opt, temp_dir_opt);

    let (args, mount_path, workspace_path, library_name) = build_args(
        &relative_mount_path,
        &relative_workspace_path,
        library_name_opt.clone(),
        &verify_tmp_root_path,
        base_image.clone(),
        bpf_flag,
        arch.clone(),
        cargo_build_sbf_args.clone(),
        cargo_args.clone(),
    )?;
    println!("Build path: {mount_path:?}");
    println!("Verifying program: {library_name}");
    println!("Workspace path: {:?}", workspace_path);

    run_preflight_checks(&mount_path, &library_name)?;

    check_signal(container_id_opt, temp_dir_opt);

    let result: Result<(String, String), anyhow::Error> = if !skip_build {
        build_and_verify_repo(
            mount_path,
            workspace_path,
            base_image.clone(),
            bpf_flag,
            arch.clone(),
            library_name.clone(),
            connection,
            program_id,
            cargo_build_sbf_args.clone(),
            cargo_args.clone(),
            container_id_opt,
        )
    } else {
        Ok(("skipped".to_string(), "skipped".to_string()))
    };

    // Cleanup no matter the result
    std::process::Command::new("rm")
        .args(["-rf", &verify_dir])
        .output()?;

    // Handle the result
    match result {
        Ok((build_hash, program_hash)) => {
            if !skip_build {
                println!("Executable Program Hash from repo: {build_hash}");
                println!("On-chain Program Hash: {program_hash}");
            }

            if skip_build || build_hash == program_hash {
                if skip_build {
                    println!("Skipping local build and writing verify data on chain");
                } else {
                    println!("Program hash matches ✅");
                }

                upload_program_verification_data(
                    repo_url.clone(),
                    &commit_hash.clone(),
                    args.iter().map(|s| s.to_string()).collect(),
                    program_id,
                    connection,
                    skip_prompt,
                    path_to_keypair.clone(),
                    compute_unit_price,
                    config_path.clone(),
                )
                .await?;

                Ok(())
            } else {
                println!("Program hashes do not match ❌");
                println!("Executable Program Hash from repo: {build_hash}");
                println!("On-chain Program Hash: {program_hash}");
                Ok(())
            }
        }
        Err(e) => Err(anyhow!("Error verifying program: {:?}", e)),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_and_verify_repo(
    mount_path: String,
    workspace_path: String,
    base_image: Option<String>,
    bpf_flag: bool,
    arch: Option<String>,
    library_name: String,
    connection: &RpcClient,
    program_id: Address,
    cargo_build_sbf_args: Option<String>,
    cargo_args: Vec<String>,
    container_id_opt: &mut Option<String>,
) -> anyhow::Result<(String, String)> {
    // Build the code using the docker container
    let executable_filename = format!("{library_name}.so");
    build(
        Some(mount_path.clone()),
        Some(workspace_path.clone()),
        Some(library_name.clone()),
        base_image,
        bpf_flag,
        arch,
        cargo_build_sbf_args,
        cargo_args,
        container_id_opt,
    )?;

    // Get the hash of the build
    let executable_path = std::process::Command::new("find")
        .args([
            &format!("{}/target/deploy", workspace_path),
            "-name",
            executable_filename.as_str(),
        ])
        .output()
        .map_err(|e| anyhow::format_err!("Failed to find executable file {}", e))
        .and_then(parse_output)?;
    println!("Executable file found at path: {executable_path:?}");
    let build_hash = get_file_hash(&executable_path)?;

    // Get the hash of the deployed program
    println!("Fetching on-chain program data for program ID: {program_id}");
    let program_hash = get_program_hash(connection, program_id)?;

    Ok((build_hash, program_hash))
}

pub fn parse_output(output: Output) -> anyhow::Result<String> {
    let string_result = String::from_utf8(output.stdout);
    // If not a success the output is meaningless
    ensure!(
        output.status.success(),
        "Status: {}, {:?}",
        output.status,
        string_result
    );
    let output = string_result?;

    let parsed_output = output
        .strip_suffix("\n")
        .ok_or_else(|| anyhow!("Failed to parse output: {output}"))?
        .to_string();
    Ok(parsed_output)
}

fn get_container_active_toolchain(container_id: &str) -> anyhow::Result<String> {
    // get the active toolchain from the container
    let output = std::process::Command::new("docker")
        .args([
            "exec",
            "-w",
            "/",
            container_id,
            "rustup",
            "show",
            "active-toolchain",
        ])
        .output()
        .map_err(|e| anyhow!("Failed to query container toolchain: {e}"))?;
    let active = parse_output(output)?;
    let toolchain = active
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("Failed to parse active rust toolchain from: {active}"))?;
    Ok(toolchain.to_string())
}

/// Reads Solana version from `[workspace.metadata.cli]` solana = "x.y.z" in the root Cargo.toml
pub fn get_solana_version_from_workspace_metadata(workspace_root: &str) -> Option<(u32, u32, u32)> {
    let path = format!("{}/Cargo.toml", workspace_root.trim_end_matches('/'));
    let manifest = Manifest::from_path(&path).ok()?;
    if let Some(Value::String(version)) = manifest
        .workspace
        .as_ref()
        .and_then(|w| w.metadata.as_ref())
        .and_then(|m| m.get("cli"))
        .and_then(|cli| cli.get("solana"))
    {
        let parts: Vec<&str> = version.split('.').collect();
        if parts.len() == 3 {
            let major = parts[0].parse::<u32>().ok()?;
            let minor = parts[1].parse::<u32>().ok()?;
            let patch = parts[2].parse::<u32>().ok()?;
            return Some((major, minor, patch));
        }
    }
    None
}

/// Tries solana-program, then solana-program-error, then solana-account-info in Cargo.lock
pub fn get_solana_version_from_lockfile(lockfile: &str) -> anyhow::Result<(u32, u32, u32)> {
    get_pkg_version_from_cargo_lock("solana-program", lockfile)
        .or_else(|_| get_pkg_version_from_cargo_lock("solana-program-error", lockfile))
        .or_else(|_| get_pkg_version_from_cargo_lock("solana-account-info", lockfile))
        .map_err(|_| {
            anyhow!(
                "Failed to determine Solana version from Cargo.lock (tried solana-program, solana-program-error, solana-account-info)"
            )
        })
}

pub fn get_pkg_version_from_cargo_lock(
    package_name: &str,
    cargo_lock_file: &str,
) -> anyhow::Result<(u32, u32, u32)> {
    let lockfile = Lockfile::load(cargo_lock_file)?;
    let res = lockfile
        .packages
        .iter()
        .filter(|pkg| pkg.name.to_string() == *package_name)
        .filter_map(|pkg| {
            let version = pkg.version.clone().to_string();
            let version_parts: Vec<&str> = version.split(".").collect();
            if version_parts.len() == 3 {
                let major = version_parts[0].parse::<u32>().unwrap_or(0);
                let minor = version_parts[1].parse::<u32>().unwrap_or(0);
                let patch = version_parts[2].parse::<u32>().unwrap_or(0);
                return Some((major, minor, patch));
            }
            None
        })
        .next()
        .ok_or_else(|| anyhow!("Failed to parse {} version from Cargo.lock", package_name))?;
    Ok(res)
}

pub fn get_lib_name_from_cargo_toml(cargo_toml_file: &str) -> anyhow::Result<String> {
    let manifest = Manifest::from_path(cargo_toml_file)?;
    let lib = manifest
        .lib
        .ok_or_else(|| anyhow!("Failed to parse lib from Cargo.toml"))?;
    lib.name
        .ok_or_else(|| anyhow!("Failed to parse lib name from Cargo.toml"))
}

pub fn get_pkg_name_from_cargo_toml(cargo_toml_file: &str) -> Option<String> {
    let manifest = Manifest::from_path(cargo_toml_file).ok()?;
    let pkg = manifest.package?;
    Some(pkg.name)
}

pub fn print_build_params(address: &Address, build_params: &OtterBuildParams) {
    println!("----------------------------------------------------------------");
    println!("Address: {address:?}");
    println!("----------------------------------------------------------------");
    println!("{build_params}");
}

pub async fn list_program_pdas(program_id: Address, client: &RpcClient) -> anyhow::Result<()> {
    let pdas = get_all_pdas_available(client, &program_id).await?;

    if pdas.is_empty() {
        println!("No verification PDAs found for program: {program_id}");
    } else {
        println!(
            "Found {} verification PDA(s) for program {program_id}:\n",
            pdas.len()
        );
        for (pda, build_params) in pdas {
            print_build_params(&pda, &build_params);
        }
    }

    Ok(())
}

pub async fn print_program_pda(
    program_id: Address,
    signer: Option<String>,
    client: &RpcClient,
    config_path: Option<String>,
) -> anyhow::Result<()> {
    let (pda, build_params) = get_program_pda(client, &program_id, signer, config_path).await?;
    print_build_params(&pda, &build_params);
    Ok(())
}

pub fn get_commit_hash(sub_m: &ArgMatches, repo_url: &str) -> anyhow::Result<String> {
    let commit_hash = sub_m
        .value_of("commit-hash")
        .map(String::from)
        .or_else(|| {
            get_commit_hash_from_remote(repo_url).ok() // Dynamically determine commit hash from remote
        })
        .ok_or_else(|| {
            anyhow::anyhow!("Commit hash must be provided or inferred from the remote repository")
        })?;

    println!("Commit hash from remote: {commit_hash}");
    Ok(commit_hash)
}

#[allow(clippy::too_many_arguments)]
async fn export_pda_tx(
    connection: &RpcClient,
    program_id: Address,
    uploader: Address,
    repo_url: String,
    commit_hash: String,
    mount_path: String,
    workspace_path: String,
    library_name: Option<String>,
    base_image: Option<String>,
    bpf_flag: bool,
    arch: Option<String>,
    temp_dir: &mut Option<String>,
    encoding: UiTransactionEncoding,
    cargo_build_sbf_args: Option<String>,
    cargo_args: Vec<String>,
    compute_unit_price: u64,
) -> anyhow::Result<()> {
    let last_deployed_slot = get_last_deployed_slot(connection, &program_id.to_string())
        .await
        .map_err(|err| anyhow!("Unable to get last deployed slot: {}", err))?;

    let (temp_root_path, verify_dir) = clone_repo_and_checkout(
        &repo_url,
        true,
        &get_basename(&repo_url)?,
        Some(commit_hash.clone()),
        temp_dir,
    )?;

    let input_params = InputParams {
        version: env!("CARGO_PKG_VERSION").to_string(),
        git_url: repo_url,
        commit: commit_hash.clone(),
        args: build_args(
            &mount_path,
            &workspace_path,
            library_name.clone(),
            &temp_root_path,
            base_image.clone(),
            bpf_flag,
            arch,
            cargo_build_sbf_args,
            cargo_args,
        )?
        .0,
        deployed_slot: last_deployed_slot,
    };

    let output = std::process::Command::new("rm")
        .args(["-rf", &verify_dir])
        .output()?;
    ensure!(
        output.status.success(),
        "Failed to delete the verifiable build directory"
    );

    let (pda, _) = find_build_params_pda(&program_id, &uploader);

    // check if account already exists
    let instruction = match connection.get_account(&pda) {
        Ok(account_info) => {
            if !account_info.data.is_empty() {
                println!("PDA already exists, creating update transaction");
                OtterVerifyInstructions::Update
            } else {
                println!("PDA does not exist, creating initialize transaction");
                OtterVerifyInstructions::Initialize
            }
        }
        Err(_) => OtterVerifyInstructions::Initialize,
    };

    let tx = compose_transaction(
        &input_params,
        uploader,
        pda,
        program_id,
        instruction,
        compute_unit_price,
    );

    // serialize the transaction to base58
    match encoding {
        UiTransactionEncoding::Base58 => {
            let encoded = bs58::encode(serialize(&tx)?).into_string();
            println!("{encoded}");
        }
        UiTransactionEncoding::Base64 => {
            let encoded = BASE64_STANDARD.encode(serialize(&tx)?);
            println!("{encoded}");
        }
        _ => unreachable!(),
    }

    Ok(())
}

fn find_relative_manifest_path_and_build_path(
    mount_path: &str,
    library_name: &str,
) -> anyhow::Result<(String, String)> {
    std::process::Command::new("find")
        .args([mount_path, "-name", "Cargo.toml"])
        .output()
        .map_err(|e| {
            anyhow::format_err!(
                "Failed to find Cargo.toml files in root directory: {}",
                e
            )
        })
        .and_then(|output| {
            ensure!(
                output.status.success(),
                "Failed to find Cargo.toml files in root directory"
            );
            for p in String::from_utf8(output.stdout)?.split("\n") {
                match get_lib_name_from_cargo_toml(p) {
                    Ok(name) => {
                        if name == library_name {
                            let manifest_path = p.to_string().replace(mount_path, "");
                            let build_path = p
                                .to_string()
                                .replace("Cargo.toml", "")
                                .replace(mount_path, "");

                            return Ok((manifest_path, build_path));
                        }
                    }
                    Err(_) => {
                        continue;
                    }
                }
            }
            Err(anyhow!(
                "No valid Cargo.toml file found in the directory for the library-name {library_name}"
            ))
        })
}

fn run_preflight_checks(mount_path: &str, library_name: &str) -> anyhow::Result<()> {
    println!("Running pre-flight validation...");

    // Check that mount path exists
    let mount_path_buf = Path::new(mount_path);
    ensure!(
        mount_path_buf.exists(),
        "Pre-flight check failed: mount path '{}' does not exist.",
        mount_path
    );

    // Validate that the library can be found using find_relative_manifest_path_and_build_path
    find_relative_manifest_path_and_build_path(mount_path, library_name)?;

    println!("Pre-flight checks passed ✅");
    Ok(())
}

fn retry_rpc_call<F, T>(mut rpc_call: F) -> anyhow::Result<T>
where
    F: FnMut() -> anyhow::Result<T>,
{
    let mut attempts = 0;
    let mut delay = INITIAL_RETRY_DELAY_MS;

    loop {
        match rpc_call() {
            Ok(result) => return Ok(result),
            Err(_err) if attempts < MAX_RETRIES => {
                attempts += 1;
                println!(
                    "RPC call failed (attempt {}/{}) - retrying in {} ms...",
                    attempts, MAX_RETRIES, delay
                );
                sleep(Duration::from_millis(delay));
                delay *= 2;
            }
            Err(err) => return Err(err),
        }
    }
}
