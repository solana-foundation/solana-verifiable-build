mod tests {
    use anyhow::Context;
    use regex::Regex;
    use std::io::Write;
    use std::process::Stdio;
    use std::sync::OnceLock;

    fn binary_path() -> anyhow::Result<&'static str> {
        static BINARY_PATH: OnceLock<Result<String, String>> = OnceLock::new();

        let path = BINARY_PATH.get_or_init(|| {
            let binary_path = "./target/debug/solana-verify";
            let output = std::process::Command::new("cargo")
                .args(["build", "--bin", "solana-verify", "--locked"])
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .output()
                .map_err(|err| format!("Failed to build solana-verify binary for tests: {err}"))?;

            if !output.status.success() {
                return Err("cargo build --bin solana-verify failed".to_string());
            }

            if !std::path::Path::new(binary_path).exists() {
                return Err(format!(
                    "Binary not found at {binary_path} after cargo build"
                ));
            }

            Ok(binary_path.to_string())
        });

        match path {
            Ok(path) => Ok(path.as_str()),
            Err(err) => Err(anyhow::anyhow!(err.clone())),
        }
    }

    fn extract_hash_from_output(output: &str) -> anyhow::Result<String> {
        let re = Regex::new(r"([a-f0-9]{64})").context("Failed to compile hash regex")?;
        re.captures_iter(output)
            .last()
            .and_then(|captures| captures.get(1))
            .map(|matched| matched.as_str().to_string())
            .context("Could not find hash in command output")
    }

    fn test_verify_program_hash_helper(expected_hash: &str, args: &[&str]) -> anyhow::Result<()> {
        let binary_path = binary_path()?;

        let mut child = std::process::Command::new(binary_path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to execute solana-verify command")?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(b"n")?;
        }

        let output = child
            .wait_with_output()
            .context("Failed to wait for solana-verify command")?;

        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Command failed: {}", error);
        }

        // Print the last 10 lines of the output
        let output_str = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = output_str.split('\n').collect();
        let last_10_lines: Vec<String> =
            lines.iter().rev().take(10).map(|s| s.to_string()).collect();
        println!("Last 10 lines of output:\n{}", last_10_lines.join("\n"));

        let re = Regex::new(r"Executable Program Hash from repo: ([a-f0-9]{64})")
            .context("Failed to compile regex")?;

        let program_hash = re
            .captures(&output_str)
            .context("Could not find program hash in output")?
            .get(1)
            .context("Invalid capture group")?
            .as_str();

        assert_eq!(
            program_hash, expected_hash,
            "Program hash {program_hash} does not match expected value {expected_hash}"
        );

        Ok(())
    }

    fn test_local_build_hash_helper(
        build_path: &str,
        executable_path: &str,
        expected_hash: &str,
    ) -> anyhow::Result<()> {
        test_local_build_hash_helper_with_args(
            &["build", build_path],
            executable_path,
            expected_hash,
        )
    }

    fn test_local_build_hash_helper_with_args(
        build_args: &[&str],
        executable_path: &str,
        expected_hash: &str,
    ) -> anyhow::Result<()> {
        let binary_path = binary_path()?;
        let child = std::process::Command::new(binary_path)
            .args(build_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to execute solana-verify build command")?;

        let output = child
            .wait_with_output()
            .context("Failed to wait for solana-verify build command")?;

        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Command failed: {}", error);
        }

        let hash_args = ["get-executable-hash", executable_path];
        let child = std::process::Command::new(binary_path)
            .args(hash_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to execute solana-verify get-executable-hash command")?;

        let output = child
            .wait_with_output()
            .context("Failed to wait for solana-verify get-executable-hash command")?;

        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!("Command failed: {}", error);
        }

        let hash = extract_hash_from_output(&String::from_utf8_lossy(&output.stdout))?;
        assert_eq!(
            hash, expected_hash,
            "Program hash {} does not match expected value {}",
            hash, expected_hash
        );

        Ok(())
    }

    #[test]
    fn test_phoenix_v1() -> anyhow::Result<()> {
        const EXPECTED_HASH: &str =
            "6877a5b732b3494b828a324ec846d526d962223959534dbaf4209e0da3b2d6a9";
        let args: Vec<&str> =  "verify-from-repo -um --program-id PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY https://github.com/Ellipsis-Labs/phoenix-v1".split(" ").collect();
        test_verify_program_hash_helper(EXPECTED_HASH, &args)?;
        Ok(())
    }

    #[test]
    fn test_squads_v3() -> anyhow::Result<()> {
        const EXPECTED_HASH: &str =
            "72da599d9ee14b2a03a23ccfa6f06d53eea4a00825ad2191929cbd78fb69205c";
        let args: Vec<&str> = "verify-from-repo https://github.com/Squads-Protocol/squads-mpl --commit-hash c95b7673d616c377a349ca424261872dfcf8b19d --program-id SMPLecH534NA9acpos4G6x7uf3LWbCAwZQE9e8ZekMu -um --library-name squads_mpl --bpf".split(" ").collect();
        test_verify_program_hash_helper(EXPECTED_HASH, &args)?;
        Ok(())
    }

    #[test]
    fn test_drift_v2() -> anyhow::Result<()> {
        const EXPECTED_HASH: &str =
            "febe4feb7310395adcda7166e92d41a13a383567457a6068613964b72353fa4e";
        let args: Vec<&str> = "verify-from-repo -um --program-id dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH https://github.com/drift-labs/protocol-v2 --commit-hash 0d35029d780b5faf8b80fce84053e862802badc1 --library-name drift".split(" ").collect();
        test_verify_program_hash_helper(EXPECTED_HASH, &args)?;
        Ok(())
    }

    #[test]
    fn test_marginfi_v2() -> anyhow::Result<()> {
        const EXPECTED_HASH: &str =
            "890d68f48f96991016222b1fcbc2cc81b8ef2dcbf280c44fe378c523c108fad5";
        let args: Vec<&str> = "verify-from-repo -um --program-id MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA https://github.com/mrgnlabs/marginfi-v2 --commit-hash d33e649e415c354cc2a1e3c49131725552d69ba0 --library-name marginfi".split(" ").collect();
        test_verify_program_hash_helper(EXPECTED_HASH, &args)?;
        Ok(())
    }

    #[test]
    fn test_games_preset() -> anyhow::Result<()> {
        const EXPECTED_HASH: &str =
            "668ff275819d9276362c6a2636d2a392afe224296e815481b94474785f490025";
        let args: Vec<&str> = "verify-from-repo -um --program-id MkabCfyUD6rBTaYHpgKBBpBo5qzWA2pK2hrGGKMurJt https://github.com/solana-developers/solana-game-preset --commit-hash eaf772fd1f21fe03a9974587f5680635e970be38 --mount-path program".split(" ").collect();
        test_verify_program_hash_helper(EXPECTED_HASH, &args)?;
        Ok(())
    }

    #[test]
    fn test_agave_2_1() -> anyhow::Result<()> {
        const EXPECTED_HASH: &str =
            "29e7713aa3c48e242e2847bc031fe2a03eb61aae5ecaec8728131e16934de465";
        let args: Vec<&str> = "verify-from-repo https://github.com/Woody4618/verify-2-1 --program-id kGYz2q2WUYCXhKpgUF4AMR3seDA9eg8sbirP5dhbyhy --commit-hash e0f138fb58b669791c823f44f878cb3547a92a26".split(" ").collect();
        test_verify_program_hash_helper(EXPECTED_HASH, &args)?;
        Ok(())
    }

    #[test]
    fn test_local_example() -> anyhow::Result<()> {
        const EXPECTED_HASH: &str =
            "08d91368d349c2b56c712422f6d274a1e8f1946ff2ecd1dc3efc3ebace52a760";
        test_local_build_hash_helper(
            "./examples/hello_world",
            "./examples/hello_world/target/deploy/hello_world.so",
            EXPECTED_HASH,
        )
    }

    #[test]
    fn test_local_pinocchio_example() -> anyhow::Result<()> {
        const EXPECTED_HASH: &str =
            "8d2e821c04f8c3826abd81b79035e38fc1284862023a18e608086674e2938512";
        test_local_build_hash_helper(
            "./examples/hello_world_pinocchio",
            "./examples/hello_world_pinocchio/target/deploy/hello_world.so",
            EXPECTED_HASH,
        )
    }

    #[test]
    fn test_local_pinocchio_example_with_cargo_build_sbf_args() -> anyhow::Result<()> {
        // This test ensures that the --cargo-build-sbf-args flag is correctly passed to
        // the build command and it does affect the resulting hash.
        //
        // The first build uses platform-tools v1.47 while the second build uses
        // platform-tools v1.54, which should produce different hashes for the same source
        // code.

        const EXPECTED_HASH_1_47: &str =
            "1df485441105f79ad2d2e795da85be191aa4abc02148c196fb3334b456266a6c";
        test_local_build_hash_helper_with_args(
            &[
                "build",
                "--cargo-build-sbf-args=--tools-version v1.47",
                "./examples/hello_world_pinocchio",
                "--",
                "--target-dir",
                "target/v1.47",
            ],
            "./examples/hello_world_pinocchio/target/v1.47/deploy/hello_world.so",
            EXPECTED_HASH_1_47,
        )?;

        const EXPECTED_HASH_1_54: &str =
            "c0b2e69079b0c3e04caae20a4e771fa16031bdf7258a26688e1d3b65a1ac8167";
        test_local_build_hash_helper_with_args(
            &[
                "build",
                "--cargo-build-sbf-args=--tools-version v1.54",
                "./examples/hello_world_pinocchio",
                "--",
                "--target-dir",
                "target/v1.54",
            ],
            "./examples/hello_world_pinocchio/target/v1.54/deploy/hello_world.so",
            EXPECTED_HASH_1_54,
        )
    }

    #[test]
    fn test_verify_from_image() -> anyhow::Result<()> {
        let binary_path = binary_path()?;
        let args: Vec<&str> = "verify-from-image -e examples/hello_world/target/deploy/hello_world.so -i ellipsislabs/hello_world_verifiable_build:latest -p 2ZrriTQSVekoj414Ynysd48jyn4AX6ZF4TTJRqHfbJfn".split(" ").collect();
        let child = std::process::Command::new(binary_path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to execute solana-verify command")?;

        let output = child
            .wait_with_output()
            .context("Failed to wait for solana-verify command")?;

        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Command failed: {}", error);
        }
        Ok(())
    }

    #[test]
    fn test_get_program_hash_legacy_loader() -> anyhow::Result<()> {
        let binary_path = binary_path()?;
        const SPL_TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
        const EXPECTED_HASH: &str =
            "d5a1793250f0f22efc7174bd8399570636e667655179642b2e90b0fb80e09106";
        let args = ["get-program-hash", SPL_TOKEN_PROGRAM_ID];
        let child = std::process::Command::new(binary_path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to execute solana-verify command")?;

        let output = child
            .wait_with_output()
            .context("Failed to wait for solana-verify command")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "get-program-hash for legacy loader program failed: {}",
                stderr
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let hash = extract_hash_from_output(&stdout)?;
        assert_eq!(
            hash, EXPECTED_HASH,
            "Program hash {} does not match expected value {}",
            hash, EXPECTED_HASH
        );
        Ok(())
    }
}
