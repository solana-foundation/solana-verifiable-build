use anyhow::anyhow;
use crossbeam_channel::{unbounded, Receiver};
use indicatif::{HumanDuration, ProgressBar, ProgressStyle};
use reqwest::{Client, Response};
use serde_json::json;
use solana_address::Address;
use solana_rpc_client::rpc_client::RpcClient;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crate::api::models::{
    ErrorResponse, JobResponse, JobStatus, JobVerificationResponse, RemoteStatusResponseWrapper,
    VerifyResponse,
};
use crate::solana_program::get_program_pda;
use crate::SIGNAL_RECEIVED;
use crate::{get_genesis_hash, DEVNET_GENESIS_HASH, MAINNET_GENESIS_HASH};

/// Remote verification server for mainnet programs.
pub const MAINNET_REMOTE_SERVER_URL: &str = "https://verify.osec.io";
/// Remote verification server for devnet programs.
pub const DEVNET_REMOTE_SERVER_URL: &str = "https://verify-devnet.osec.io";

/// Returns the remote verification server for the cluster with `genesis_hash`, or `None` for a
/// cluster other than mainnet or devnet.
fn remote_server_url_for_genesis(genesis_hash: &str) -> Option<&'static str> {
    match genesis_hash {
        MAINNET_GENESIS_HASH => Some(MAINNET_REMOTE_SERVER_URL),
        DEVNET_GENESIS_HASH => Some(DEVNET_REMOTE_SERVER_URL),
        _ => None,
    }
}

/// Returns `override_url` without a trailing slash if set, else the remote verification server for
/// the cluster that `connection` points at. Only queries the cluster when `override_url` is `None`,
/// and fails for a cluster other than mainnet or devnet.
pub fn resolve_remote_server_url(
    connection: &RpcClient,
    override_url: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(url) = override_url {
        return Ok(url.trim_end_matches('/').to_string());
    }
    let genesis_hash = get_genesis_hash(connection)?;
    remote_server_url_for_genesis(&genesis_hash)
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!(
                "No remote verification server is known for the cluster with genesis hash {genesis_hash}. Pass --remote-url to choose one."
            )
        })
}

fn loading_animation(receiver: Receiver<bool>) {
    let started = Instant::now();
    let spinner_style =
        ProgressStyle::with_template("[{elapsed_precise}] {prefix:.bold.dim} {spinner} {wide_msg}")
            .unwrap()
            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");

    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style);
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_message("Request sent. Awaiting server response. This may take a moment... ⏳");

    loop {
        // Check if interrupt signal was received
        if SIGNAL_RECEIVED.load(Ordering::Relaxed) {
            pb.finish_with_message("❌ Operation interrupted by user.");
            break;
        }

        match receiver.try_recv() {
            Ok(result) => {
                if result {
                    pb.finish_with_message(format!(
                        "✅ Process completed. (Done in {})\n",
                        HumanDuration(started.elapsed())
                    ));
                } else {
                    pb.finish_with_message("❌ Request processing failed.");
                    println!("❌ Time elapsed : {}", HumanDuration(started.elapsed()));
                }
                break;
            }
            Err(_) => {
                if SIGNAL_RECEIVED.load(Ordering::Relaxed) {
                    pb.finish_with_message("❌ Operation interrupted by user.");
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    pb.abandon(); // Ensure the progress bar is cleaned up
}

fn print_verification_status(
    program_id: &str,
    status: bool,
    status_response: &JobVerificationResponse,
) {
    let status_message = if status {
        format!("Program {program_id} has been verified. ✅")
    } else {
        format!("Program {program_id} has not been verified. ❌")
    };
    let message = if status {
        "The provided GitHub build matches the on-chain hash."
    } else {
        "The provided GitHub build does not match the on-chain hash."
    };
    println!("{status_message}");
    println!("{message}");
    println!("On Chain Hash: {}", status_response.on_chain_hash.as_str());
    println!(
        "Executable Hash: {}",
        status_response.executable_hash.as_str()
    );
    println!("Repo URL: {}", status_response.repo_url.as_str());
}

pub async fn send_job_with_uploader_to_remote(
    connection: &RpcClient,
    program_id: &Address,
    uploader: &Address,
    remote_url: &str,
) -> anyhow::Result<()> {
    // Check that PDA exists before sending job
    get_program_pda(connection, program_id, Some(uploader.to_string()), None).await?;

    let client = Client::builder()
        .timeout(Duration::from_secs(18000))
        .build()?;

    println!("Using remote verification server: {remote_url}");
    // Send the POST request
    let response = client
        .post(format!("{remote_url}/verify-with-signer"))
        .json(&json!({
            "program_id": program_id.to_string(),
            "signer": uploader.to_string(),
            "repository": "",
            "commit_hash": "",
        }))
        .send()
        .await?;

    handle_submission_response(&client, response, program_id, remote_url).await
}

pub async fn handle_submission_response(
    client: &Client,
    response: Response,
    program_id: &Address,
    remote_url: &str,
) -> anyhow::Result<()> {
    if response.status().is_success() {
        // First get the raw text to preserve it in case of parsing failure
        let response_text = response.text().await?;
        let status_response =
            serde_json::from_str::<VerifyResponse>(&response_text).map_err(|e| {
                eprintln!("Failed to parse response as VerifyResponse: {e}");
                eprintln!("Raw response: {response_text}");
                anyhow!("Failed to parse server response")
            })?;
        let request_id = status_response.request_id;
        println!("Verification request sent with request id: {request_id}");
        println!("Verification in progress... ⏳");

        // Span new thread for polling the server for status
        // Create a channel for communication between threads
        let (sender, receiver) = unbounded();
        let handle = thread::spawn(move || loading_animation(receiver));

        loop {
            // Check for interrupt signal before polling
            if SIGNAL_RECEIVED.load(Ordering::Relaxed) {
                let _ = sender.send(false);
                handle.join().unwrap();
                break; // Exit the loop and continue with normal error handling
            }

            let status = check_job_status(client, remote_url, &request_id).await?;
            match status.status {
                JobStatus::InProgress => {
                    if SIGNAL_RECEIVED.load(Ordering::Relaxed) {
                        let _ = sender.send(false);
                        handle.join().unwrap();
                        break;
                    }
                    thread::sleep(Duration::from_secs(10));
                }
                JobStatus::Completed => {
                    let _ = sender.send(true);
                    handle.join().unwrap();
                    let status_response = status.respose.unwrap();

                    if status_response.executable_hash == status_response.on_chain_hash {
                        print_verification_status(
                            program_id.to_string().as_str(),
                            true,
                            &status_response,
                        );
                    } else {
                        print_verification_status(
                            program_id.to_string().as_str(),
                            false,
                            &status_response,
                        );
                    }
                    break;
                }
                JobStatus::Failed => {
                    let _ = sender.send(false);
                    handle.join().unwrap();
                    let status_response: JobVerificationResponse = status.respose.unwrap();
                    println!("Program {program_id} has not been verified. ❌");
                    eprintln!("Error message: {}", status_response.message.as_str());
                    println!(
                        "You can check the logs for more details here: {remote_url}/logs/{request_id}"
                    );
                    break;
                }
                JobStatus::Unknown => {
                    let _ = sender.send(false);
                    handle.join().unwrap();
                    println!("Program {program_id} has not been verified. ❌");
                    break;
                }
            }
        }
        let url = format!("{remote_url}/status/{program_id}");
        println!("Check the verification status at: {url}");
        println!("Job url: {remote_url}/job/{request_id}");

        Ok(())
    } else if response.status() == 409 {
        let response = response.json::<ErrorResponse>().await?;
        eprintln!("Error: {}", response.error.as_str());
        let url = format!("{remote_url}/status/{program_id}");
        println!("Check the status at: {url}");
        Ok(())
    } else {
        eprintln!("Encountered an error while attempting to send the job to remote");
        Err(anyhow!("{:?}", response.text().await?))?;
        let url = format!("{remote_url}/status/{program_id}");
        println!("Check the verification status at: {url}");
        Ok(())
    }
}

async fn check_job_status(
    client: &Client,
    remote_url: &str,
    request_id: &str,
) -> anyhow::Result<JobResponse> {
    // Get /job/:id
    let response = client
        .get(format!("{remote_url}/job/{request_id}"))
        .send()
        .await
        .unwrap();

    if response.status().is_success() {
        // Parse the response
        let response: JobVerificationResponse = response.json().await?;
        match response.status {
            JobStatus::InProgress => {
                thread::sleep(Duration::from_secs(5));
                Ok(JobResponse {
                    status: JobStatus::InProgress,
                    respose: None,
                })
            }
            JobStatus::Completed => Ok(JobResponse {
                status: JobStatus::Completed,
                respose: Some(response),
            }),
            JobStatus::Failed => Ok(JobResponse {
                status: JobStatus::Failed,
                respose: Some(response),
            }),
            JobStatus::Unknown => Ok(JobResponse {
                status: JobStatus::Unknown,
                respose: Some(response),
            }),
        }
    } else {
        Err(anyhow!(
            "Encountered an error while attempting to check job status : {:?}",
            response.text().await?
        ))?
    }
}

pub async fn get_remote_status(remote_url: &str, program_id: Address) -> anyhow::Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(18000))
        .build()?;

    let response = client
        .get(format!("{remote_url}/status-all/{program_id}"))
        .send()
        .await?;

    let status: RemoteStatusResponseWrapper = response.json().await?;
    println!("{status}");
    Ok(())
}

pub async fn get_remote_job(remote_url: &str, job_id: &str) -> anyhow::Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(18000))
        .build()?;

    let job = check_job_status(&client, remote_url, job_id).await?;
    println!("{job}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_server_url_follows_the_cluster() {
        assert_eq!(
            remote_server_url_for_genesis(MAINNET_GENESIS_HASH),
            Some(MAINNET_REMOTE_SERVER_URL)
        );
        assert_eq!(
            remote_server_url_for_genesis(DEVNET_GENESIS_HASH),
            Some(DEVNET_REMOTE_SERVER_URL)
        );
        assert_eq!(
            remote_server_url_for_genesis("4uhcVJyU9pJkvQyS88uRDiswHXSCkY3zQawwpjk2NsNY"),
            None
        );
    }

    #[test]
    fn remote_url_override_skips_the_cluster() {
        // Nothing listens on port 1, so a genesis hash query would fail.
        let connection = RpcClient::new("http://127.0.0.1:1".to_string());
        assert_eq!(
            resolve_remote_server_url(&connection, Some("http://localhost:3000/")).unwrap(),
            "http://localhost:3000"
        );
    }
}
