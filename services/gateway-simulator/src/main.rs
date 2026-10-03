//! Developer-only, deliberately SIMULATED gateway command line.
use std::{
    env, fs,
    fs::OpenOptions,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use openpush_client_core::{
    Client, ClientConfig, DatabaseKey, IncomingSms, KeyProfile, PermitDecision, SendResult,
    VaultCheckHeader,
};
use openpush_crypto::{create_vault_check_header, derive_root_key};
use openpush_domain::{DeviceId, VaultId};
use openpush_gateway_simulator::{
    BOOTSTRAP_FILE, CREDENTIAL_FILE, DESKTOP_IMPORT_FILE, DEVELOPER_KEY_FILE, SimulatorError,
    SimulatorState, bootstrap_snapshot, declare_compaction_capability, drain_media, effects_path,
    publish_capabilities, read_bounded_json, read_or_create_route, read_state,
    run_one_carrier_effect_for_route, sync_http, upload_pending, write_state,
};
use openpush_protocol::pairing_proof_message;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Serialize, Deserialize)]
struct Bootstrap {
    profile: KeyProfile,
    header: VaultCheckHeader,
    fingerprint: String,
}

const DATABASE_FILE: &str = "gateway.sqlcipher";
const INITIALIZING_FILE: &str = "SIMULATED-client-initializing.json";
const INITIALIZED_FILE: &str = "SIMULATED-client-initialized.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct RuntimeMarker {
    version: u8,
    vault_id: String,
    device_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DatabaseBootstrap {
    Existing,
    FirstStart,
}

fn usage() {
    eprintln!(
        "SIMULATED gateway: prepare-vault STATE_DIR | pair STATE_DIR OWNER_CREDENTIAL_FILE [gateway|device] | run STATE_DIR\nSecrets are read from protected files, stdin, or OPENPUSH_SIMULATOR_PASSPHRASE; never argv."
    );
}
fn protected(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|_| format!("refusing to replace {}", path.display()))?;
    file.write_all(bytes)
        .map_err(|_| "SIMULATED file write failed".to_string())?;
    file.sync_all()
        .map_err(|_| "SIMULATED file sync failed".to_string())
}
fn sync_directory(path: &Path) -> Result<(), String> {
    std::fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "SIMULATED state directory sync failed".to_string())
}

fn runtime_marker(state: &SimulatorState) -> RuntimeMarker {
    RuntimeMarker {
        version: 1,
        vault_id: state.vault_id.clone(),
        device_id: state.device_id.clone(),
    }
}

fn read_runtime_marker(path: &Path, state: &SimulatorState) -> Result<(), String> {
    let marker: RuntimeMarker = serde_json::from_slice(
        &fs::read(path).map_err(|_| "SIMULATED initialization marker read failed")?,
    )
    .map_err(|_| "SIMULATED initialization marker invalid")?;
    if marker != runtime_marker(state) {
        return Err("SIMULATED state identity mismatch; fresh pairing required".into());
    }
    Ok(())
}

fn make_existing_database_private(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(
            "SIMULATED client database is missing for an initialized identity; fresh pairing required"
                .into(),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| "SIMULATED database permissions failed")?;
    }
    Ok(())
}

fn prepare_database_bootstrap(
    dir: &Path,
    state: &SimulatorState,
) -> Result<DatabaseBootstrap, String> {
    let database = dir.join(DATABASE_FILE);
    let initializing = dir.join(INITIALIZING_FILE);
    let initialized = dir.join(INITIALIZED_FILE);

    if initialized.exists() {
        read_runtime_marker(&initialized, state)?;
        make_existing_database_private(&database)?;
        return Ok(DatabaseBootstrap::Existing);
    }

    if initializing.exists() {
        read_runtime_marker(&initializing, state)?;
    } else if database.exists() {
        // Upgrade an existing pre-marker simulator database without discarding its durable ledger.
        make_existing_database_private(&database)?;
        protected(
            &initialized,
            &serde_json::to_vec_pretty(&runtime_marker(state))
                .map_err(|_| "SIMULATED initialization marker encoding failed")?,
        )?;
        sync_directory(dir)?;
        return Ok(DatabaseBootstrap::Existing);
    } else {
        protected(
            &initializing,
            &serde_json::to_vec_pretty(&runtime_marker(state))
                .map_err(|_| "SIMULATED initialization marker encoding failed")?,
        )?;
        sync_directory(dir)?;
    }

    if database.exists() {
        make_existing_database_private(&database)?;
    } else {
        protected(&database, b"")?;
        sync_directory(dir)?;
    }
    Ok(DatabaseBootstrap::FirstStart)
}

fn finish_database_bootstrap(dir: &Path, state: &SimulatorState) -> Result<(), String> {
    let initialized = dir.join(INITIALIZED_FILE);
    if !initialized.exists() {
        protected(
            &initialized,
            &serde_json::to_vec_pretty(&runtime_marker(state))
                .map_err(|_| "SIMULATED initialization marker encoding failed")?,
        )?;
        sync_directory(dir)?;
    }
    let initializing = dir.join(INITIALIZING_FILE);
    if initializing.exists() {
        fs::remove_file(initializing)
            .map_err(|_| "SIMULATED initialization marker cleanup failed")?;
        sync_directory(dir)?;
    }
    Ok(())
}

fn runtime_error(context: &str, error: SimulatorError) -> String {
    if matches!(error, SimulatorError::FreshPairingRequired) {
        "SIMULATED identity has prior producer history; fresh pairing required".into()
    } else {
        format!("SIMULATED {context}")
    }
}

fn phrase() -> Result<String, String> {
    if let Ok(value) = env::var("OPENPUSH_SIMULATOR_PASSPHRASE") {
        return Ok(value);
    }
    eprintln!("SIMULATED developer passphrase (stdin, one line):");
    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .map_err(|_| "SIMULATED stdin failed".to_string())?;
    Ok(value.trim_end_matches(['\n', '\r']).to_string())
}
fn key_file(dir: &Path) -> Result<[u8; 32], String> {
    let path = dir.join(DEVELOPER_KEY_FILE);
    if path.exists() {
        return hex::decode(
            fs::read_to_string(path).map_err(|_| "SIMULATED keystore read failed")?,
        )
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or("invalid SIMULATED developer keystore".into());
    }
    let mut key = [0; 32];
    rand::RngCore::fill_bytes(&mut OsRng, &mut key);
    protected(&path, hex::encode(key).as_bytes())?;
    Ok(key)
}
fn load_bootstrap(dir: &Path) -> Result<Bootstrap, String> {
    serde_json::from_slice(
        &fs::read(dir.join(BOOTSTRAP_FILE)).map_err(|_| "missing SIMULATED vault bootstrap")?,
    )
    .map_err(|_| "invalid SIMULATED vault bootstrap".into())
}

fn prepare(dir: PathBuf) -> Result<(), String> {
    fs::create_dir_all(&dir).map_err(|_| "SIMULATED state directory failed")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|_| "SIMULATED state permissions failed")?;
    }
    let profile = KeyProfile::new(VaultId::new().0, 1).map_err(|_| "SIMULATED crypto failed")?;
    let root =
        derive_root_key(&phrase()?, &profile).map_err(|_| "SIMULATED passphrase rejected")?;
    let header =
        create_vault_check_header(&root, profile.clone()).map_err(|_| "SIMULATED header failed")?;
    let bootstrap = Bootstrap {
        fingerprint: profile
            .fingerprint()
            .map_err(|_| "SIMULATED fingerprint failed")?,
        profile,
        header,
    };
    protected(
        &dir.join(BOOTSTRAP_FILE),
        &serde_json::to_vec_pretty(&bootstrap)
            .map_err(|_| "SIMULATED bootstrap encoding failed")?,
    )?;
    let _ = key_file(&dir)?;
    let _ = read_or_create_route(&dir).map_err(|_| "SIMULATED route setup failed")?;
    println!(
        "SIMULATED vault bootstrap prepared: {}",
        dir.join(BOOTSTRAP_FILE).display()
    );
    Ok(())
}

async fn pair(dir: PathBuf, owner: PathBuf, role: String) -> Result<(), String> {
    if !matches!(role.as_str(), "gateway" | "device") {
        return Err("role must be gateway or device".into());
    }
    let owner = read_state(&owner).map_err(|_| "invalid owner credential file")?;
    let bootstrap = load_bootstrap(&dir)?;
    let mut signing_bytes = [0; 32];
    rand::RngCore::fill_bytes(&mut OsRng, &mut signing_bytes);
    let key = SigningKey::from_bytes(&signing_bytes);
    let device = DeviceId::new();
    let public =
        json!({"ed25519_public_key":URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())});
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| "SIMULATED HTTP client failed")?;
    let challenge:Value=read_bounded_json(http.post(format!("{}/v1/pairing",owner.origin)).bearer_auth(&owner.device_token).json(&json!({"device_id":device.0,"public_key":public,"profile_fingerprint":bootstrap.fingerprint,"key_epoch":bootstrap.profile.key_epoch,"requested_role":role})).send().await.map_err(|_|"SIMULATED pairing request failed")?).await.map_err(|_|"SIMULATED pairing response invalid")?;
    let token = challenge["challenge_token"]
        .as_str()
        .ok_or("SIMULATED pairing token missing")?;
    let raw: [u8; 32] = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| "SIMULATED pairing token invalid")?
        .try_into()
        .map_err(|_| "SIMULATED pairing token invalid")?;
    let proof = pairing_proof_message(
        &raw,
        VaultId(bootstrap.profile.vault_id),
        device,
        &bootstrap.fingerprint,
        bootstrap.profile.key_epoch,
        &role,
    );
    let response:Value=read_bounded_json(http.post(format!("{}/v1/pairing/consume",owner.origin)).json(&json!({"challenge_token":token,"device_id":device.0,"public_key":public,"profile_fingerprint":bootstrap.fingerprint,"key_epoch":bootstrap.profile.key_epoch,"signature":URL_SAFE_NO_PAD.encode(key.sign(&proof).to_bytes())})).send().await.map_err(|_|"SIMULATED pairing consume failed")?).await.map_err(|_|"SIMULATED pairing response invalid")?;
    let credentials = SimulatorState {
        version: 1,
        origin: owner.origin,
        vault_id: bootstrap.profile.vault_id.to_string(),
        device_id: device.0.to_string(),
        device_token: response["device_token"]
            .as_str()
            .ok_or("SIMULATED credential missing")?
            .to_owned(),
    };
    let name = if role == "device" {
        DESKTOP_IMPORT_FILE
    } else {
        CREDENTIAL_FILE
    };
    write_state(&dir.join(name), &credentials).map_err(|_| "SIMULATED credential write failed")?;
    protected(
        &dir.join("SIMULATED-device-signing-key.hex"),
        hex::encode(key.to_bytes()).as_bytes(),
    )?;
    println!(
        "SIMULATED {} pairing complete; protected import written",
        role
    );
    Ok(())
}

async fn run(dir: PathBuf) -> Result<(), String> {
    let state = read_state(&dir.join(CREDENTIAL_FILE))
        .map_err(|error| format!("missing SIMULATED gateway credentials: {error}"))?;
    let bootstrap = load_bootstrap(&dir)?;
    let route = read_or_create_route(&dir).map_err(|_| "SIMULATED route read failed")?;
    let vault_id = VaultId(state.vault_id.parse().map_err(|_| "invalid vault id")?);
    let device_id = DeviceId(state.device_id.parse().map_err(|_| "invalid device id")?);
    let bootstrap_state = prepare_database_bootstrap(&dir, &state)?;
    let config = ClientConfig {
        database_path: dir.join(DATABASE_FILE),
        vault_id,
        device_id,
    };
    let client = Client::open(
        config,
        DatabaseKey::new(&key_file(&dir)?).map_err(|_| "invalid SIMULATED developer keystore")?,
    )
    .map_err(|_| "SIMULATED client open failed")?;
    client
        .unlock(&bootstrap.profile, &bootstrap.header, &phrase()?)
        .map_err(|_| "SIMULATED unlock failed")?;
    let _ = declare_compaction_capability(&state.origin, &state.device_token)
        .await
        .map_err(|_| "SIMULATED compaction capability declaration failed")?;
    if bootstrap_state == DatabaseBootstrap::FirstStart {
        bootstrap_snapshot(&client, &state.origin, &state.device_token, device_id)
            .await
            .map_err(|error| runtime_error("bootstrap snapshot failed", error))?;
        finish_database_bootstrap(&dir, &state)?;
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if tx.send(line.unwrap_or_default()).is_err() {
                break;
            }
        }
    });
    publish_capabilities(&state.origin, &state.device_token, &route.subscription_id)
        .await
        .map_err(|_| "SIMULATED capability publish failed")?;
    println!(
        "SIMULATED gateway running; JSON lines: incoming, pause, resume, outcome, crash-after-effect, quit"
    );
    let mut paused = false;
    loop {
        while let Ok(line) = rx.try_recv() {
            let value: Value =
                serde_json::from_str(&line).map_err(|_| "SIMULATED control must be JSON")?;
            match value["type"].as_str() {
                Some("incoming") => {
                    client
                        .capture_incoming(IncomingSms {
                            conversation_id: None,
                            sender_address: value["senderAddress"]
                                .as_str()
                                .ok_or("senderAddress required")?
                                .into(),
                            body: value["body"].as_str().ok_or("body required")?.into(),
                            provider_message_id: value["providerMessageId"]
                                .as_str()
                                .map(str::to_owned),
                            imported: value["imported"].as_bool().unwrap_or(false),
                        })
                        .map_err(|_| "SIMULATED capture failed")?;
                }
                Some("pause") => paused = true,
                Some("resume") => paused = false,
                Some("crash-after-effect") => {
                    let _ = run_one_carrier_effect_for_route(
                        &client,
                        &effects_path(&dir),
                        &route.subscription_id,
                        true,
                    )
                    .map_err(|_| "SIMULATED effect failed")?;
                    return Ok(());
                }
                Some("outcome") => {
                    if let Some(command) = client
                        .pending_commands()
                        .map_err(|_| "SIMULATED command lookup failed")?
                        .into_iter()
                        .next()
                        && matches!(
                            client
                                .begin_send_attempt(command.command_id)
                                .map_err(|_| "SIMULATED permit failed")?,
                            PermitDecision::Permit(_)
                        )
                    {
                        client
                            .record_send_result(
                                command.command_id,
                                if value["state"] == "failed_confirmed" {
                                    SendResult::FailedConfirmed
                                } else {
                                    SendResult::Sent
                                },
                            )
                            .map_err(|_| "SIMULATED outcome failed")?;
                    }
                }
                Some("quit") => return Ok(()),
                _ => return Err("unknown SIMULATED control".into()),
            }
        }
        if !paused {
            eprintln!("SIMULATED phase=sync");
            publish_capabilities(&state.origin, &state.device_token, &route.subscription_id)
                .await
                .map_err(|_| "SIMULATED capability publish failed")?;
            let _ = declare_compaction_capability(&state.origin, &state.device_token)
                .await
                .map_err(|_| "SIMULATED compaction capability declaration failed")?;
            sync_http(&client, &state.origin, &state.device_token)
                .await
                .map_err(|_| "SIMULATED sync failed")?;
            eprintln!("SIMULATED phase=media");
            drain_media(&client, &state.origin, &state.device_token, &dir)
                .await
                .map_err(|_| "SIMULATED media transfer failed")?;
            upload_pending(&client, &state.origin, &state.device_token)
                .await
                .map_err(|error| runtime_error("upload failed", error))?;
            let _ = run_one_carrier_effect_for_route(
                &client,
                &effects_path(&dir),
                &route.subscription_id,
                false,
            )
            .map_err(|_| "SIMULATED effect failed")?;
            upload_pending(&client, &state.origin, &state.device_token)
                .await
                .map_err(|error| runtime_error("upload failed", error))?;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::main]
async fn main() {
    let mut args = env::args().skip(1);
    let result: Result<(), String> = match args.next().as_deref() {
        Some("prepare-vault") => match args.next() {
            Some(dir) => prepare(PathBuf::from(dir)),
            None => Err("STATE_DIR required".into()),
        },
        Some("pair") => {
            let d = args.next().map(PathBuf::from);
            let o = args.next().map(PathBuf::from);
            let r = args.next().unwrap_or_else(|| "gateway".into());
            match (d, o) {
                (Some(d), Some(o)) => pair(d, o, r).await,
                _ => Err("STATE_DIR and OWNER_CREDENTIAL_FILE required".into()),
            }
        }
        Some("run") => match args.next() {
            Some(d) => run(PathBuf::from(d)).await,
            None => Err("STATE_DIR required".into()),
        },
        _ => {
            usage();
            return;
        }
    };
    if let Err(error) = result {
        eprintln!("SIMULATED gateway failed: {error}");
        std::process::exit(2)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn developer_secret_file_is_private_at_creation() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("developer-key");
        protected(&path, b"secret").unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn lost_database_requires_fresh_pairing_without_touching_credentials_or_effects() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = SimulatorState {
            version: 1,
            origin: "http://127.0.0.1:12345".into(),
            vault_id: VaultId::new().0.to_string(),
            device_id: DeviceId::new().0.to_string(),
            device_token: "synthetic-token".into(),
        };
        let credentials = dir.path().join(CREDENTIAL_FILE);
        write_state(&credentials, &state).unwrap();
        let credential_bytes = fs::read(&credentials).unwrap();

        assert_eq!(
            prepare_database_bootstrap(dir.path(), &state).unwrap(),
            DatabaseBootstrap::FirstStart
        );
        let database = dir.path().join(DATABASE_FILE);
        assert!(database.exists());
        assert_eq!(
            fs::metadata(&database).unwrap().permissions().mode() & 0o777,
            0o600
        );
        finish_database_bootstrap(dir.path(), &state).unwrap();
        assert_eq!(
            prepare_database_bootstrap(dir.path(), &state).unwrap(),
            DatabaseBootstrap::Existing
        );

        let effects = effects_path(dir.path());
        protected(&effects, b"existing-effect\n").unwrap();
        let effect_bytes = fs::read(&effects).unwrap();
        fs::remove_file(&database).unwrap();

        let error = prepare_database_bootstrap(dir.path(), &state).unwrap_err();
        assert!(error.contains("fresh pairing required"));
        assert_eq!(fs::read(credentials).unwrap(), credential_bytes);
        assert_eq!(fs::read(effects).unwrap(), effect_bytes);
    }
}
