use anyhow::{anyhow, ensure, Context};
use flate2::read::{GzDecoder, ZlibDecoder};
use serde::{Deserialize, Serialize};
use solana_address::Address;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::{Keypair, Signer};
use solana_loader_v3_interface::{get_program_data_address, state::UpgradeableLoaderState};
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable};
use solana_system_interface::{instruction as system_instruction, program as system_program};
use solana_transaction::{Message, Transaction};
use spl_program_metadata_client::{
    accounts::Metadata,
    types::{Compression, DataSource, Encoding, Format},
    ID as PROGRAM_METADATA_ID,
};
use std::{
    io::Read,
    path::{Component, Path, PathBuf},
    str::from_utf8,
};

use crate::solana_program::{
    get_keypair_from_path, get_user_config_with_path, prompt_user_input, OtterBuildParams,
};

pub const IDL_SEED: &str = "idl";
pub const IDL_VERIFICATION_SEED: &str = "idl-verification";
const METADATA_HEADER_LENGTH: usize = 96;
const METADATA_HEADER_PADDING: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdlVerificationMetadata {
    pub kind: String,
    pub version: u8,
    pub repo_url: String,
    pub commit: String,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct ReproducedIdl {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataTarget {
    pub authority: Address,
    pub canonical: bool,
    pub program_data: Option<Address>,
}

#[derive(Debug, Clone)]
pub struct FetchedMetadataAccount {
    pub address: Address,
    pub metadata: Metadata,
}

#[derive(Debug, Clone)]
pub struct IdlVerificationOutcome {
    pub idl_account: Address,
    pub idl_verification_account: Address,
    pub authority: Address,
    pub canonical: bool,
    pub reproduced_path: PathBuf,
    pub reproduced_hash: String,
    pub on_chain_hash: String,
    pub success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdlExecutableVerificationMatch {
    pub repo_url_matches: bool,
    pub commit_matches: bool,
}

impl IdlExecutableVerificationMatch {
    pub fn success(&self) -> bool {
        self.repo_url_matches && self.commit_matches
    }
}

impl IdlVerificationMetadata {
    pub fn new(repo_url: String, commit: String, path: String) -> anyhow::Result<Self> {
        let metadata = Self {
            kind: "idl-verification".to_string(),
            version: 1,
            repo_url,
            commit,
            path,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.kind == "idl-verification",
            "IDL verification metadata kind must be 'idl-verification'"
        );
        ensure!(
            self.version == 1,
            "Unsupported IDL verification metadata version '{}'",
            self.version
        );
        ensure!(
            !self.repo_url.trim().is_empty(),
            "IDL verification metadata requires non-empty 'repo_url'"
        );
        ensure!(
            !self.commit.trim().is_empty(),
            "IDL verification metadata requires non-empty 'commit'"
        );
        ensure!(
            !self.path.trim().is_empty(),
            "IDL verification metadata requires non-empty 'path'"
        );
        ensure_repo_relative(&self.path, "path")?;
        Ok(())
    }

    pub fn to_pretty_json_bytes(&self) -> anyhow::Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec_pretty(self)?)
    }
}

pub fn idl_metadata_from_cli(
    repo_url: &str,
    commit: &str,
    path: Option<&str>,
) -> anyhow::Result<Option<IdlVerificationMetadata>> {
    path.map(|path| {
        IdlVerificationMetadata::new(repo_url.to_string(), commit.to_string(), path.to_string())
    })
    .transpose()
}

pub fn parse_idl_verification_metadata(bytes: &[u8]) -> anyhow::Result<IdlVerificationMetadata> {
    let metadata: IdlVerificationMetadata = serde_json::from_slice(bytes)
        .map_err(|err| anyhow!("Failed to parse IDL verification metadata JSON: {}", err))?;
    metadata.validate()?;
    Ok(metadata)
}

pub fn seed_from_str(seed: &str) -> anyhow::Result<[u8; 16]> {
    let bytes = seed.as_bytes();
    ensure!(
        bytes.len() <= 16,
        "program-metadata seed '{}' is too long; maximum is 16 bytes",
        seed
    );
    let mut fixed = [0_u8; 16];
    fixed[..bytes.len()].copy_from_slice(bytes);
    Ok(fixed)
}

pub fn metadata_pda(program_id: &Address, authority: Option<&Address>, seed: &str) -> Address {
    let seed = seed_from_str(seed).expect("static metadata seed must fit");
    let seeds: Vec<&[u8]> = match authority {
        Some(authority) => vec![program_id.as_ref(), authority.as_ref(), seed.as_slice()],
        None => vec![program_id.as_ref(), seed.as_slice()],
    };
    Address::find_program_address(&seeds, &PROGRAM_METADATA_ID).0
}

impl MetadataTarget {
    pub fn pda_for_seed(&self, program_id: &Address, seed: &str) -> Address {
        metadata_pda(
            program_id,
            (!self.canonical).then_some(&self.authority),
            seed,
        )
    }
}

pub fn select_metadata_target(
    client: &RpcClient,
    program_id: &Address,
    authority: Option<Address>,
) -> anyhow::Result<MetadataTarget> {
    let program_authority = get_program_authority(client, program_id)?;

    match authority {
        Some(authority) if Some(authority) == program_authority.upgrade_authority => {
            Ok(MetadataTarget {
                authority,
                canonical: true,
                program_data: program_authority.program_data,
            })
        }
        Some(authority) => Ok(MetadataTarget {
            authority,
            canonical: false,
            program_data: None,
        }),
        None => {
            let authority = program_authority.upgrade_authority.ok_or_else(|| {
                anyhow!(
                    "Program {} does not have a current canonical upgrade authority; pass --authority to verify a non-canonical IDL metadata account",
                    program_id
                )
            })?;
            Ok(MetadataTarget {
                authority,
                canonical: true,
                program_data: program_authority.program_data,
            })
        }
    }
}

pub fn get_program_authority(
    client: &RpcClient,
    program_id: &Address,
) -> anyhow::Result<ProgramAuthority> {
    let program_account = client
        .get_account(program_id)
        .map_err(|err| anyhow!("Failed to fetch program account {}: {}", program_id, err))?;

    ensure!(
        program_account.executable,
        "Program account {} is not executable",
        program_id
    );

    if program_account.owner == bpf_loader_upgradeable::id() {
        let program_state: UpgradeableLoaderState = bincode::deserialize(
            program_account
                .data
                .get(..UpgradeableLoaderState::size_of_program())
                .ok_or_else(|| {
                    anyhow!("Upgradeable program account {} is too small", program_id)
                })?,
        )
        .context("Failed to decode upgradeable program account")?;

        let UpgradeableLoaderState::Program {
            programdata_address,
        } = program_state
        else {
            return Err(anyhow!(
                "Upgradeable program account {} has invalid loader state",
                program_id
            ));
        };

        let derived_program_data = get_program_data_address(program_id);
        ensure!(
            programdata_address == derived_program_data,
            "ProgramData account mismatch for program {}",
            program_id
        );

        let program_data_account = client.get_account(&programdata_address).map_err(|err| {
            anyhow!(
                "Failed to fetch ProgramData account {}: {}",
                programdata_address,
                err
            )
        })?;
        let program_data_state: UpgradeableLoaderState = bincode::deserialize(
            program_data_account
                .data
                .get(..UpgradeableLoaderState::size_of_programdata_metadata())
                .ok_or_else(|| {
                    anyhow!("ProgramData account {} is too small", programdata_address)
                })?,
        )
        .context("Failed to decode ProgramData account")?;

        let UpgradeableLoaderState::ProgramData {
            upgrade_authority_address,
            ..
        } = program_data_state
        else {
            return Err(anyhow!(
                "ProgramData account {} has invalid loader state",
                programdata_address
            ));
        };

        Ok(ProgramAuthority {
            upgrade_authority: upgrade_authority_address,
            program_data: Some(programdata_address),
        })
    } else if program_account.owner == bpf_loader::id()
        || program_account.owner == bpf_loader_deprecated::id()
    {
        Ok(ProgramAuthority {
            upgrade_authority: Some(*program_id),
            program_data: None,
        })
    } else {
        Err(anyhow!(
            "Unsupported program loader {} for program {}",
            program_account.owner,
            program_id
        ))
    }
}

#[derive(Debug, Clone)]
pub struct ProgramAuthority {
    pub upgrade_authority: Option<Address>,
    pub program_data: Option<Address>,
}

pub fn fetch_matching_metadata_account(
    client: &RpcClient,
    program_id: &Address,
    target: &MetadataTarget,
    seed_name: &str,
) -> anyhow::Result<FetchedMetadataAccount> {
    let seed = seed_from_str(seed_name)?;
    let address = target.pda_for_seed(program_id, seed_name);
    let account = client.get_account(&address).map_err(|err| {
        anyhow!(
            "Failed to fetch program-metadata account {}: {}",
            address,
            err
        )
    })?;

    ensure!(
        account.owner == PROGRAM_METADATA_ID,
        "Account {} is not owned by the program-metadata program",
        address
    );

    let metadata = Metadata::from_bytes(&account.data).map_err(|err| {
        anyhow!(
            "Failed to decode program-metadata account {}: {}",
            address,
            err
        )
    })?;

    ensure!(
        metadata.program == *program_id,
        "program-metadata account {} belongs to program {}, expected {}",
        address,
        metadata.program,
        program_id
    );
    ensure!(
        metadata.seed == seed,
        "program-metadata account {} has an unexpected seed",
        address
    );
    ensure!(
        metadata.canonical == target.canonical,
        "program-metadata account {} canonical flag does not match selected authority",
        address
    );

    if target.canonical {
        let zero = Address::new_from_array([0; 32]);
        ensure!(
            metadata.authority.to_string() == zero.to_string(),
            "canonical program-metadata account {} unexpectedly stores authority {}",
            address,
            metadata.authority
        );
    } else {
        ensure!(
            metadata.authority.to_string() == target.authority.to_string(),
            "program-metadata account {} authority {}, expected {}",
            address,
            metadata.authority,
            target.authority
        );
    }

    Ok(FetchedMetadataAccount { address, metadata })
}

pub fn fetch_idl_verification_metadata(
    client: &RpcClient,
    program_id: &Address,
    target: &MetadataTarget,
) -> anyhow::Result<(FetchedMetadataAccount, IdlVerificationMetadata)> {
    let account =
        fetch_matching_metadata_account(client, program_id, target, IDL_VERIFICATION_SEED)?;
    let content = resolve_metadata_content_bytes(client, &account.metadata)?;
    let metadata = parse_idl_verification_metadata(&content)?;
    Ok((account, metadata))
}

pub fn resolve_metadata_content_bytes(
    client: &RpcClient,
    metadata: &Metadata,
) -> anyhow::Result<Vec<u8>> {
    let packed = packed_metadata_data(metadata)?;
    match metadata.data_source {
        DataSource::Direct => decode_packed_bytes(packed, metadata.compression, metadata.encoding),
        DataSource::Url => {
            let url_bytes = decode_packed_bytes(packed, metadata.compression, metadata.encoding)?;
            let url = String::from_utf8(url_bytes)
                .map_err(|err| anyhow!("program-metadata URL is not valid UTF-8: {}", err))?;
            let response = reqwest::blocking::get(&url)
                .map_err(|err| anyhow!("Failed to fetch metadata URL '{}': {}", url, err))?;
            ensure!(
                response.status().is_success(),
                "Metadata URL '{}' returned HTTP status {}",
                url,
                response.status()
            );
            Ok(response.bytes()?.to_vec())
        }
        DataSource::External => {
            let external = unpack_external_data(&packed)?;
            let account = client.get_account(&external.address).map_err(|err| {
                anyhow!(
                    "Failed to fetch external metadata account {}: {}",
                    external.address,
                    err
                )
            })?;
            let offset = external.offset as usize;
            ensure!(
                offset <= account.data.len(),
                "External metadata offset {} exceeds account data length {}",
                offset,
                account.data.len()
            );
            let remaining = &account.data[offset..];
            let slice = if let Some(length) = external.length {
                let length = length as usize;
                ensure!(
                    length <= remaining.len(),
                    "External metadata length {} exceeds remaining account data length {}",
                    length,
                    remaining.len()
                );
                &remaining[..length]
            } else {
                remaining
            };
            decode_packed_bytes(slice.to_vec(), metadata.compression, metadata.encoding)
        }
    }
}

fn packed_metadata_data(metadata: &Metadata) -> anyhow::Result<Vec<u8>> {
    let data = metadata.data.to_vec();
    let expected_len = metadata.data_length as usize;
    let end = METADATA_HEADER_PADDING + expected_len;

    ensure!(
        end <= data.len(),
        "program-metadata data_length {} (with header padding) exceeds stored data length {}",
        expected_len,
        data.len()
    );
    Ok(data[METADATA_HEADER_PADDING..end].to_vec())
}

#[derive(Debug, Clone)]
struct ExternalMetadataData {
    address: Address,
    offset: u32,
    length: Option<u32>,
}

fn unpack_external_data(data: &[u8]) -> anyhow::Result<ExternalMetadataData> {
    ensure!(
        data.len() >= 40,
        "External program-metadata pointer must be at least 40 bytes"
    );
    let address = Address::new_from_array(
        data[..32]
            .try_into()
            .map_err(|_| anyhow!("Invalid external metadata address bytes"))?,
    );
    let offset = u32::from_le_bytes(
        data[32..36]
            .try_into()
            .map_err(|_| anyhow!("Invalid external metadata offset bytes"))?,
    );
    let raw_length = u32::from_le_bytes(
        data[36..40]
            .try_into()
            .map_err(|_| anyhow!("Invalid external metadata length bytes"))?,
    );
    Ok(ExternalMetadataData {
        address,
        offset,
        length: (raw_length != 0).then_some(raw_length),
    })
}

fn decode_packed_bytes(
    data: Vec<u8>,
    compression: Compression,
    encoding: Encoding,
) -> anyhow::Result<Vec<u8>> {
    let uncompressed = uncompress_data(&data, compression)?;
    decode_data(&uncompressed, encoding)
}

fn uncompress_data(data: &[u8], compression: Compression) -> anyhow::Result<Vec<u8>> {
    match compression {
        Compression::None => Ok(data.to_vec()),
        Compression::Gzip => {
            let mut decoder = GzDecoder::new(data);
            let mut output = Vec::new();
            decoder.read_to_end(&mut output)?;
            Ok(output)
        }
        Compression::Zlib => {
            let mut decoder = ZlibDecoder::new(data);
            let mut output = Vec::new();
            decoder.read_to_end(&mut output)?;
            Ok(output)
        }
    }
}

fn decode_data(data: &[u8], encoding: Encoding) -> anyhow::Result<Vec<u8>> {
    match encoding {
        Encoding::None | Encoding::Utf8 => Ok(data.to_vec()),
        Encoding::Base58 => {
            let encoded = from_utf8(data)
                .map_err(|err| anyhow!("Base58 metadata is not valid UTF-8: {}", err))?;
            bs58::decode(encoded)
                .into_vec()
                .map_err(|err| anyhow!("Failed to decode base58 metadata: {}", err))
        }
        Encoding::Base64 => {
            use base64::{prelude::BASE64_STANDARD, Engine};
            let encoded = from_utf8(data)
                .map_err(|err| anyhow!("Base64 metadata is not valid UTF-8: {}", err))?;
            BASE64_STANDARD
                .decode(encoded)
                .map_err(|err| anyhow!("Failed to decode base64 metadata: {}", err))
        }
    }
}

pub fn reproduce_idl(
    repo_root: &Path,
    metadata: &IdlVerificationMetadata,
) -> anyhow::Result<ReproducedIdl> {
    metadata.validate()?;

    let path = resolve_repo_path(repo_root, &metadata.path)?;
    let bytes = std::fs::read(&path).map_err(|err| {
        anyhow!(
            "Failed to read reproduced IDL '{}': {}",
            path.display(),
            err
        )
    })?;
    let sha256 = idl_hash_from_bytes(&bytes);
    Ok(ReproducedIdl {
        path,
        bytes,
        sha256,
    })
}

pub fn resolve_repo_path(repo_root: &Path, value: &str) -> anyhow::Result<PathBuf> {
    ensure_repo_relative(value, "path")?;
    Ok(repo_root.join(value))
}

fn ensure_repo_relative(value: &str, field: &str) -> anyhow::Result<()> {
    let path = Path::new(value);
    ensure!(
        !path.as_os_str().is_empty(),
        "{} must be a non-empty repo-relative path",
        field
    );
    ensure!(
        !path.is_absolute(),
        "{} must be repo-relative, got '{}'",
        field,
        value
    );
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(anyhow!(
                    "{} must not escape the repository with '..': '{}'",
                    field,
                    value
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!("{} must be repo-relative, got '{}'", field, value));
            }
        }
    }
    Ok(())
}

pub fn idl_hash_from_bytes(bytes: &[u8]) -> String {
    sha256::digest(bytes)
}

pub fn idl_hash_from_path(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)
        .map_err(|err| anyhow!("Failed to read IDL file '{}': {}", path.display(), err))?;
    Ok(idl_hash_from_bytes(&bytes))
}

pub fn verify_idl_against_on_chain(
    client: &RpcClient,
    program_id: &Address,
    target: &MetadataTarget,
    repo_root: &Path,
    metadata: &IdlVerificationMetadata,
    verification_account: Address,
) -> anyhow::Result<IdlVerificationOutcome> {
    let idl_account = fetch_matching_metadata_account(client, program_id, target, IDL_SEED)?;
    let on_chain_bytes = resolve_metadata_content_bytes(client, &idl_account.metadata)?;
    let on_chain_hash = idl_hash_from_bytes(&on_chain_bytes);
    let reproduced = reproduce_idl(repo_root, metadata)?;
    let reproduced_hash = reproduced.sha256.clone();
    let success = reproduced.bytes == on_chain_bytes;

    Ok(IdlVerificationOutcome {
        idl_account: idl_account.address,
        idl_verification_account: verification_account,
        authority: target.authority,
        canonical: target.canonical,
        reproduced_path: reproduced.path,
        reproduced_hash,
        on_chain_hash,
        success,
    })
}

pub fn print_idl_verification_outcome(outcome: &IdlVerificationOutcome) {
    println!("Verified IDL Path: {}", outcome.reproduced_path.display());
    println!("IDL Metadata Account: {}", outcome.idl_account);
    println!(
        "IDL Verification Metadata Account: {}",
        outcome.idl_verification_account
    );
    println!("IDL Metadata Authority: {}", outcome.authority);
    println!("IDL Hash from repo: {}", outcome.reproduced_hash);
    println!("On-chain IDL Hash: {}", outcome.on_chain_hash);
    if outcome.success {
        println!("IDL Verification: success ✅");
    } else {
        println!("IDL Verification: failed ❌");
    }
}

pub fn compare_executable_verification_pair(
    build_params: &OtterBuildParams,
    idl_metadata: &IdlVerificationMetadata,
) -> IdlExecutableVerificationMatch {
    IdlExecutableVerificationMatch {
        repo_url_matches: build_params.git_url == idl_metadata.repo_url,
        commit_matches: build_params.commit == idl_metadata.commit,
    }
}

pub fn print_executable_verification_pair_match(pair: &IdlExecutableVerificationMatch) {
    if pair.success() {
        println!("IDL Verification matches Program Verification repo and commit: success ✅");
    } else {
        println!("IDL Verification matches Program Verification repo and commit: failed ❌");
    }
}

pub fn validate_executable_verification_identity(
    build_params: &OtterBuildParams,
    program_id: &Address,
    metadata_authority: &Address,
) -> anyhow::Result<()> {
    ensure!(
        build_params.address == *program_id,
        "Executable verification PDA program {} does not match requested program {}",
        build_params.address,
        program_id
    );
    ensure!(
        build_params.signer == *metadata_authority,
        "Executable verification PDA signer {} does not match IDL metadata authority {}",
        build_params.signer,
        metadata_authority
    );
    Ok(())
}

pub fn validate_executable_verification_pair(
    build_params: &OtterBuildParams,
    program_id: &Address,
    metadata_authority: &Address,
    idl_metadata: &IdlVerificationMetadata,
) -> anyhow::Result<()> {
    validate_executable_verification_identity(build_params, program_id, metadata_authority)?;
    let pair = compare_executable_verification_pair(build_params, idl_metadata);
    ensure!(
        pair.repo_url_matches,
        "Executable verification repo URL '{}' does not match IDL repo URL '{}'",
        build_params.git_url,
        idl_metadata.repo_url
    );
    ensure!(
        pair.commit_matches,
        "Executable verification commit '{}' does not match IDL commit '{}'",
        build_params.commit,
        idl_metadata.commit
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn upload_idl_verification_metadata(
    client: &RpcClient,
    program_id: Address,
    target: &MetadataTarget,
    metadata: &IdlVerificationMetadata,
    skip_prompt: bool,
    path_to_keypair: Option<String>,
    compute_unit_price: u64,
    config_path: Option<String>,
) -> anyhow::Result<Address> {
    metadata.validate()?;
    if !(skip_prompt
        || prompt_user_input(
            "Do you want to upload the IDL verification metadata to the Solana Blockchain? (y/n) ",
        ))
    {
        println!("Exiting without uploading the IDL verification metadata.");
        return Ok(target.pda_for_seed(&program_id, IDL_VERIFICATION_SEED));
    }

    let signer = load_upload_signer(path_to_keypair, config_path)?;
    ensure!(
        signer.pubkey() == target.authority,
        "IDL metadata signer {} does not match selected metadata authority {}",
        signer.pubkey(),
        target.authority
    );

    let metadata_address = target.pda_for_seed(&program_id, IDL_VERIFICATION_SEED);
    let data = metadata.to_pretty_json_bytes()?;
    let maybe_existing = client.get_account(&metadata_address).ok();

    let mut instructions = Vec::new();
    if compute_unit_price > 0 {
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
            compute_unit_price,
        ));
    }

    if let Some(existing) = maybe_existing {
        ensure!(
            existing.owner == PROGRAM_METADATA_ID,
            "Existing account {} is not owned by program-metadata",
            metadata_address
        );
        let decoded = Metadata::from_bytes(&existing.data).map_err(|err| {
            anyhow!(
                "Failed to decode existing IDL verification metadata account {}: {}",
                metadata_address,
                err
            )
        })?;
        ensure!(
            decoded.mutable,
            "IDL verification metadata account {} is immutable",
            metadata_address
        );

        let required_lamports =
            client.get_minimum_balance_for_rent_exemption(METADATA_HEADER_LENGTH + data.len())?;
        if existing.lamports < required_lamports {
            instructions.push(system_instruction::transfer(
                &signer.pubkey(),
                &metadata_address,
                required_lamports - existing.lamports,
            ));
        }
        instructions.push(set_data_instruction(
            metadata_address,
            signer.pubkey(),
            program_id,
            target.program_data,
            data,
        ));
        println!(
            "Updating IDL verification metadata account: {}",
            metadata_address
        );
    } else {
        let rent =
            client.get_minimum_balance_for_rent_exemption(METADATA_HEADER_LENGTH + data.len())?;
        instructions.push(system_instruction::transfer(
            &signer.pubkey(),
            &metadata_address,
            rent,
        ));
        instructions.push(initialize_instruction(
            metadata_address,
            signer.pubkey(),
            program_id,
            target.program_data,
            IDL_VERIFICATION_SEED,
            data,
        )?);
        println!(
            "Creating IDL verification metadata account: {}",
            metadata_address
        );
    }

    let mut tx = Transaction::new_unsigned(Message::new(&instructions, Some(&signer.pubkey())));
    tx.sign(&[&signer], client.get_latest_blockhash()?);
    let tx_id = client
        .send_and_confirm_transaction_with_spinner(&tx)
        .map_err(|err| {
            anyhow!(
                "Failed to send IDL verification metadata transaction: {}",
                err
            )
        })?;
    println!(
        "IDL verification metadata uploaded successfully. Transaction ID: {}",
        tx_id
    );

    Ok(metadata_address)
}

fn load_upload_signer(
    path_to_keypair: Option<String>,
    config_path: Option<String>,
) -> anyhow::Result<Keypair> {
    if let Some(path_to_keypair) = path_to_keypair {
        get_keypair_from_path(&path_to_keypair)
    } else {
        Ok(get_user_config_with_path(config_path)?.0)
    }
}

fn initialize_instruction(
    metadata: Address,
    authority: Address,
    program: Address,
    program_data: Option<Address>,
    seed: &str,
    data: Vec<u8>,
) -> anyhow::Result<Instruction> {
    let mut instruction_data = vec![1_u8];
    instruction_data.extend_from_slice(&seed_from_str(seed)?);
    instruction_data.push(encoding_to_u8(Encoding::Utf8));
    instruction_data.push(compression_to_u8(Compression::None));
    instruction_data.push(format_to_u8(Format::Json));
    instruction_data.push(data_source_to_u8(DataSource::Direct));
    instruction_data.extend_from_slice(&data);

    Ok(Instruction {
        program_id: PROGRAM_METADATA_ID,
        accounts: vec![
            AccountMeta::new(metadata, false),
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new_readonly(program, false),
            AccountMeta::new_readonly(program_data.unwrap_or(PROGRAM_METADATA_ID), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: instruction_data,
    })
}

fn set_data_instruction(
    metadata: Address,
    authority: Address,
    program: Address,
    program_data: Option<Address>,
    data: Vec<u8>,
) -> Instruction {
    let mut instruction_data = vec![3_u8];
    instruction_data.push(encoding_to_u8(Encoding::Utf8));
    instruction_data.push(compression_to_u8(Compression::None));
    instruction_data.push(format_to_u8(Format::Json));
    instruction_data.push(data_source_to_u8(DataSource::Direct));
    instruction_data.extend_from_slice(&data);

    Instruction {
        program_id: PROGRAM_METADATA_ID,
        accounts: vec![
            AccountMeta::new(metadata, false),
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new_readonly(PROGRAM_METADATA_ID, false),
            AccountMeta::new_readonly(
                program_data.map(|_| program).unwrap_or(PROGRAM_METADATA_ID),
                false,
            ),
            AccountMeta::new_readonly(program_data.unwrap_or(PROGRAM_METADATA_ID), false),
        ],
        data: instruction_data,
    }
}

fn encoding_to_u8(value: Encoding) -> u8 {
    match value {
        Encoding::None => 0,
        Encoding::Utf8 => 1,
        Encoding::Base58 => 2,
        Encoding::Base64 => 3,
    }
}

fn compression_to_u8(value: Compression) -> u8 {
    match value {
        Compression::None => 0,
        Compression::Gzip => 1,
        Compression::Zlib => 2,
    }
}

fn format_to_u8(value: Format) -> u8 {
    match value {
        Format::None => 0,
        Format::Json => 1,
        Format::Yaml => 2,
        Format::Toml => 3,
    }
}

fn data_source_to_u8(value: DataSource) -> u8 {
    match value {
        DataSource::Direct => 0,
        DataSource::Url => 1,
        DataSource::External => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn valid_metadata() -> IdlVerificationMetadata {
        IdlVerificationMetadata::new(
            "https://github.com/example/program".to_string(),
            "0123456789abcdef".to_string(),
            "target/idl/program.json".to_string(),
        )
        .unwrap()
    }

    #[test]
    fn parses_and_serializes_idl_verification_json() -> anyhow::Result<()> {
        let metadata = valid_metadata();
        let json = metadata.to_pretty_json_bytes()?;
        let parsed = parse_idl_verification_metadata(&json)?;
        assert_eq!(parsed, metadata);
        Ok(())
    }

    #[test]
    fn rejects_missing_required_metadata_fields() {
        let missing_repo = br#"{
            "kind":"idl-verification",
            "version":1,
            "commit":"abc",
            "path":"idl.json"
        }"#;
        assert!(parse_idl_verification_metadata(missing_repo).is_err());

        let missing_commit = br#"{
            "kind":"idl-verification",
            "version":1,
            "repo_url":"https://example.com/repo",
            "path":"idl.json"
        }"#;
        assert!(parse_idl_verification_metadata(missing_commit).is_err());

        let missing_path = br#"{
            "kind":"idl-verification",
            "version":1,
            "repo_url":"https://example.com/repo",
            "commit":"abc"
        }"#;
        assert!(parse_idl_verification_metadata(missing_path).is_err());
    }

    #[test]
    fn cli_idl_flags_are_optional_but_complete_when_present() -> anyhow::Result<()> {
        let none = idl_metadata_from_cli("https://example.com/repo", "abc", None)?;
        assert!(none.is_none());

        let missing_path = idl_metadata_from_cli("https://example.com/repo", "abc", Some(""));
        assert!(missing_path.is_err());

        let metadata =
            idl_metadata_from_cli("https://example.com/repo", "abc", Some("idl/program.json"))?
                .expect("expected IDL metadata");
        assert_eq!(metadata.path, "idl/program.json");
        Ok(())
    }

    #[test]
    fn resolves_repo_relative_paths() -> anyhow::Result<()> {
        let root = PathBuf::from("/tmp/repo");
        assert_eq!(
            resolve_repo_path(&root, "target/idl/program.json")?,
            PathBuf::from("/tmp/repo/target/idl/program.json")
        );
        assert!(resolve_repo_path(&root, "/tmp/idl.json").is_err());
        assert!(resolve_repo_path(&root, "../idl.json").is_err());
        Ok(())
    }

    #[test]
    fn idl_hash_uses_exact_raw_bytes() {
        let hash_a = idl_hash_from_bytes(b"{\"a\":1}\0");
        let hash_b = idl_hash_from_bytes(b"{\"a\":1}");
        assert_ne!(hash_a, hash_b);
    }

    #[test]
    fn derives_canonical_and_non_canonical_metadata_pdas() -> anyhow::Result<()> {
        let program = Address::from_str("11111111111111111111111111111111")?;
        let authority = Address::from_str("Sysvar1111111111111111111111111111111111111")?;
        let canonical = metadata_pda(&program, None, IDL_SEED);
        let non_canonical = metadata_pda(&program, Some(&authority), IDL_SEED);
        assert_ne!(canonical, non_canonical);
        Ok(())
    }

    #[test]
    fn validates_executable_pda_pairing() -> anyhow::Result<()> {
        let program = Address::from_str("11111111111111111111111111111111")?;
        let authority = Address::from_str("Sysvar1111111111111111111111111111111111111")?;
        let metadata = valid_metadata();
        let params = OtterBuildParams {
            address: program,
            signer: authority,
            version: "0.0.0".to_string(),
            git_url: metadata.repo_url.clone(),
            commit: metadata.commit.clone(),
            args: vec![],
            deployed_slot: 0,
            bump: 0,
        };
        let pair = compare_executable_verification_pair(&params, &metadata);
        assert!(pair.success());
        validate_executable_verification_pair(&params, &program, &authority, &metadata)?;

        let mut mismatched = metadata.clone();
        mismatched.commit = "different".to_string();
        let pair = compare_executable_verification_pair(&params, &mismatched);
        assert!(pair.repo_url_matches);
        assert!(!pair.commit_matches);
        assert!(
            validate_executable_verification_pair(&params, &program, &authority, &mismatched)
                .is_err()
        );
        Ok(())
    }
}
