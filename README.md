# Solana Verified Builds

This repository demonstrates how to implement verified builds for Solana programs. Verified builds ensure that your deployed program matches exactly with your public source code, promoting transparency and security in the Solana ecosystem.

## What are Verified Builds?

Verified builds allow developers and users to verify that a deployed Solana program matches its source code. This verification:

- Ensures program authenticity
- Promotes transparency
- Builds user trust
- Makes source code discoverable

## Quick Start

1. Install prerequisites:

   - Docker
   - Cargo
   - Solana Verify CLI (`cargo install solana-verify --locked`)

For production use, prefer installing from a tagged release.

2. Build your program:

```bash
solana-verify build
```

For programs that don't depend on `solana-program` (e.g. SDK v3 or pinocchio), add the Solana CLI version in your root `Cargo.toml` so the tool can pick the right build image:

```toml
[workspace.metadata.cli]
solana = "3.0.0"
```

The tool checks this first, then falls back to `Cargo.lock` (solana-program, solana-program-error, or solana-account-info).

3. Deploy and verify:

```bash
# Deploy
solana program deploy -u $NETWORK_URL target/deploy/$PROGRAM_LIB_NAME.so --program-id $PROGRAM_ID

# Verify against repository -> upload your build data on chain
solana-verify verify-from-repo -u $NETWORK_URL --program-id $PROGRAM_ID https://github.com/$REPO_PATH

# Trigger a remote job
solana-verify remote submit-job --program-id $PROGRAM_ID --uploader $THE_PUBKEY_THAT_UPLOADED_YOUR_BUILD_DATA
```

> The legacy `--remote` flag on `verify-from-repo` has been deprecated. Upload your PDA with programs upgrade authority, then run the `remote submit-job` command to queue OtterSec's worker. For a full walkthrough of the PDA workflow, see the [Solana verified builds guide](https://solana.com/docs/programs/verified-builds).

## Documentation

For detailed instructions and best practices, please refer to the [official Solana documentation on verified builds](https://solana.com/docs/programs/verified-builds).

## IDL Verification

`solana-verify` can verify program IDLs published through the
`program-metadata` `"idl"` seed and publish provenance metadata through the
matching `"idl-verification"` seed.

IDL verification metadata uses this JSON shape:

```json
{
  "kind": "idl-verification",
  "version": 1,
  "repo_url": "https://github.com/example/program",
  "commit": "7e5f4b7e7d13c37f2b8470b5f0f9f0f10c5c7a1d",
  "path": "target/idl/my_program.json"
}
```

The verifier hashes the IDL artifact bytes directly, so formatting and key ordering are part of the verified artifact.

Verify an existing on-chain IDL verification record:

```bash
solana-verify idl verify --program-id $PROGRAM_ID

# Verify a non-canonical third-party IDL record
solana-verify idl verify --program-id $PROGRAM_ID --authority $AUTHORITY
```

Verify an IDL and upload `"idl-verification"` metadata after success:

```bash
solana-verify idl verify-and-upload \
  --program-id $PROGRAM_ID \
  --repo-url https://github.com/example/program \
  --commit-hash 7e5f4b7e7d13c37f2b8470b5f0f9f0f10c5c7a1d \
  --idl-path target/idl/my_program.json \
  -k ~/.config/solana/id.json
```

`verify-from-repo` can do executable and IDL verification in one flow:

```bash
solana-verify verify-from-repo \
  --program-id $PROGRAM_ID \
  --commit-hash $COMMIT \
  --idl-path target/idl/my_program.json \
  https://github.com/example/program
```

If the signer is the program upgrade authority, the CLI writes the canonical
`[program_id, "idl-verification"]` metadata account. Otherwise it writes the
non-canonical `[program_id, signer, "idl-verification"]` account. The matching
`"idl"` account and executable verification PDA must use the same metadata
authority, repository URL, and commit.

## Security Considerations

While verified builds enhance transparency, they should not be considered a complete security solution. Always:

- Review the source code
- Use trusted build environments
- Consider using governance solutions for program upgrades

### Current Verification Scope

- Build images are selected by pinned digest.
- Solana/Agave installer scripts in generated Dockerfiles are checksum pinned.
- Post-install verification of installed toolchain/platform-tools is still follow-up work.

For responsible disclosure of bugs related to verified builds CLI, please email maintainers@ellipsislabs.xyz with a detailed description of the attack vector.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## Releasing

Use `.github/workflows/release.yml` (`Release`) as the canonical release path.
Use `.github/workflows/build.yml` only for manual artifact builds from a specific ref.

See [RELEASE.md](RELEASE.md) for release steps and recovery procedures.
