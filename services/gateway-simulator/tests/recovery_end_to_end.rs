//! Encrypted recovery acceptance over isolated PostgreSQL and SeaweedFS Compose fixtures.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey};
use peppy_client_core::*;
use peppy_crypto::{create_vault_check_header, derive_root_key};
use peppy_gateway_simulator::{drain_media, effects_path, run_one_carrier_effect, upload_pending};
use peppy_protocol::pairing_proof_message;
use peppy_server::{
    api::{create_owner, router},
    config::S3Config,
    storage::Storage,
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{
    fs,
    future::Future,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::Duration,
};
use tempfile::TempDir;
use tokio::{net::TcpListener, task::JoinHandle};
use uuid::Uuid;

const PHRASE: &str = "correct horse battery staple";
const ADDRESS: &str = "+15555550100";
const PG_IMAGE: &str =
    "postgres:18.1@sha256:1090bc3a8ccfb0b55f78a494d76f8d603434f7e4553543d6e807bc7bd6bbd17f";
const SEAWEED_IMAGE: &str = "chrislusf/seaweedfs:4.09@sha256:353c69c8ddd7e13c85c1290dca9d1690bf3d066237b7ede7920b8fd2864858e7";
const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);

async fn bounded<T>(phase: &'static str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(NETWORK_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("phase={phase} status=timeout"))
}

fn checked(output: Output, phase: &str) -> Output {
    assert!(
        output.status.success(),
        "phase={phase} status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

struct ComposeFixture {
    project: String,
    path: PathBuf,
    cleaned: bool,
}

impl ComposeFixture {
    fn create(root: &Path, label: &str) -> Self {
        let project = format!("recovery{}{}", label, Uuid::new_v4().simple());
        let path = root.join(&project);
        fs::create_dir(&path).unwrap();
        let env = format!(
            "POSTGRES_DB=recovery\nPOSTGRES_USER=recovery\nPOSTGRES_PASSWORD=recovery_fixture_password\nS3_ACCESS_KEY=recovery_access_{}\nS3_SECRET_KEY=recovery_secret_{}\nS3_BUCKET=recovery-private\n",
            &project[project.len() - 8..],
            &project[project.len() - 12..]
        );
        let env_path = path.join(".env");
        fs::write(&env_path, env).unwrap();
        fs::set_permissions(&env_path, fs::Permissions::from_mode(0o600)).unwrap();
        let compose = format!(
            r#"name: {project}
services:
  postgres:
    image: {PG_IMAGE}
    environment:
      POSTGRES_DB: ${{POSTGRES_DB}}
      POSTGRES_USER: ${{POSTGRES_USER}}
      POSTGRES_PASSWORD: ${{POSTGRES_PASSWORD}}
    volumes: [postgres-data:/var/lib/postgresql]
    ports: ["127.0.0.1::5432"]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U $$POSTGRES_USER -d $$POSTGRES_DB"]
      interval: 2s
      timeout: 2s
      retries: 30
  seaweedfs:
    image: {SEAWEED_IMAGE}
    command: >-
      server -s3 -s3.port=8333 -s3.config=/etc/seaweedfs/s3.json -dir=/data
      -master.port=9333 -master.volumeSizeLimitMB=32 -volume.port=8080 -volume.max=8
      -filer -filer.port=8888 -ip=seaweedfs
    configs:
      - source: seaweed-s3-config
        target: /etc/seaweedfs/s3.json
    volumes:
      - seaweed-volume-data:/data
      - seaweed-filer-data:/root/.seaweedfs
    ports: ["127.0.0.1::8333"]
    healthcheck:
      test: ["CMD-SHELL", "wget -S -O /dev/null http://127.0.0.1:8333/ 2>&1 | grep -Eq 'HTTP/.* (401|403)'"]
      interval: 2s
      timeout: 2s
      retries: 30
  api:
    image: {PG_IMAGE}
    command: ["sleep", "infinity"]
  migrate:
    image: {PG_IMAGE}
    command: ["sh", "-c", "exit 0"]
volumes:
  postgres-data:
  seaweed-volume-data:
  seaweed-filer-data:
configs:
  seaweed-s3-config:
    content: |
      {{"identities":[{{"name":"recovery","credentials":[{{"accessKey":"${{S3_ACCESS_KEY}}","secretKey":"${{S3_SECRET_KEY}}"}}],"actions":["Read","Write","List","Tagging","Admin"]}}]}}
"#
        );
        fs::write(path.join("docker-compose.yml"), compose).unwrap();
        Self {
            project,
            path,
            cleaned: false,
        }
    }

    fn compose(&self, arguments: &[&str]) -> Output {
        let mut command = Command::new("docker");
        command
            .current_dir(&self.path)
            // Shell environment has higher Compose interpolation precedence than --env-file.
            // Pin fixture values so a sourced developer test.env cannot change container auth.
            .env("POSTGRES_DB", "recovery")
            .env("POSTGRES_USER", "recovery")
            .env("POSTGRES_PASSWORD", "recovery_fixture_password")
            .env(
                "S3_ACCESS_KEY",
                format!(
                    "recovery_access_{}",
                    &self.project[self.project.len() - 8..]
                ),
            )
            .env(
                "S3_SECRET_KEY",
                format!(
                    "recovery_secret_{}",
                    &self.project[self.project.len() - 12..]
                ),
            )
            .env("S3_BUCKET", "recovery-private")
            .args(["compose", "--env-file", ".env", "-f", "docker-compose.yml"])
            .args(arguments);
        command.output().unwrap()
    }

    fn up_source(&self) {
        checked(
            self.compose(&["create", "api", "migrate"]),
            "source-create-stopped",
        );
        checked(
            self.compose(&["up", "-d", "--wait", "postgres", "seaweedfs"]),
            "source-infra-up",
        );
    }

    fn port(&self, service: &str, container_port: &str) -> u16 {
        let output = checked(
            self.compose(&["port", service, container_port]),
            "compose-port",
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn running_services(&self) -> Vec<String> {
        let output = checked(
            self.compose(&["ps", "--status", "running", "--services"]),
            "compose-running-services",
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn container_state(&self, service: &str) -> String {
        let container = checked(
            self.compose(&["ps", "--all", "-q", service]),
            "compose-container-id",
        );
        let container = String::from_utf8(container.stdout).unwrap();
        let output = checked(
            Command::new("docker")
                .args(["inspect", "--format", "{{.State.Status}}", container.trim()])
                .output()
                .unwrap(),
            "compose-container-state",
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn cleanup(&mut self) {
        if !self.cleaned {
            checked(
                self.compose(&["down", "-v", "--remove-orphans"]),
                "fixture-cleanup",
            );
            self.cleaned = true;
        }
    }
}

impl Drop for ComposeFixture {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.compose(&["down", "-v", "--remove-orphans"]);
            self.cleaned = true;
        }
    }
}

struct RouterRuntime {
    pool: PgPool,
    task: JoinHandle<()>,
    url: String,
}

impl RouterRuntime {
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
        self.pool.close().await;
    }
}

struct VaultFixture {
    vault: VaultId,
    profile: KeyProfile,
    header: VaultCheckHeader,
    fingerprint: String,
    owner_token: String,
}

struct Paired {
    id: DeviceId,
    token: String,
}

fn database_url(fixture: &ComposeFixture) -> String {
    format!(
        "postgres://recovery:recovery_fixture_password@127.0.0.1:{}/recovery",
        fixture.port("postgres", "5432")
    )
}

fn s3_config(fixture: &ComposeFixture) -> S3Config {
    let suffix = &fixture.project[fixture.project.len() - 8..];
    S3Config {
        endpoint: format!("http://127.0.0.1:{}", fixture.port("seaweedfs", "8333"))
            .parse()
            .unwrap(),
        bucket: "recovery-private".into(),
        access_key: format!("recovery_access_{suffix}"),
        secret_key: format!(
            "recovery_secret_{}",
            &fixture.project[fixture.project.len() - 12..]
        ),
        readiness_timeout: Duration::from_secs(2),
    }
}

fn configure_router(database_url: &str, s3: &S3Config) {
    unsafe {
        std::env::set_var("PEPPY_ENV", "development");
        std::env::set_var("DATABASE_URL", database_url);
        std::env::set_var("BIND_ADDR", "127.0.0.1:0");
        std::env::set_var("S3_INTERNAL_ENDPOINT", s3.endpoint.as_str());
        std::env::set_var("S3_ACCESS_KEY", &s3.access_key);
        std::env::set_var("S3_SECRET_KEY", &s3.secret_key);
        std::env::set_var("S3_BUCKET", &s3.bucket);
    }
}

async fn start_router(fixture: &ComposeFixture, migrate: bool) -> RouterRuntime {
    let database_url = database_url(fixture);
    let s3 = s3_config(fixture);
    configure_router(&database_url, &s3);
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(NETWORK_TIMEOUT)
        .connect(&database_url)
        .await
        .unwrap();
    if migrate {
        sqlx::migrate!("../server/migrations")
            .run(&pool)
            .await
            .unwrap();
        Storage::new(&s3).ensure_bucket().await.unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_pool = pool.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, router(server_pool)).await.unwrap();
    });
    RouterRuntime {
        pool,
        task,
        url: format!("http://{address}"),
    }
}

async fn create_vault(pool: &PgPool) -> VaultFixture {
    let vault = VaultId::new();
    let profile = KeyProfile::new(vault.0, 1).unwrap();
    let fingerprint = profile.fingerprint().unwrap();
    let root = derive_root_key(PHRASE, &profile).unwrap();
    let header = create_vault_check_header(&root, profile.clone()).unwrap();
    let owner = create_owner(
        pool,
        serde_json::to_value(&profile).unwrap(),
        serde_json::to_vec(&header).unwrap(),
        fingerprint.clone(),
        1,
    )
    .await
    .unwrap();
    VaultFixture {
        vault,
        profile,
        header,
        fingerprint,
        owner_token: owner.device_token,
    }
}

async fn pair(runtime: &RouterRuntime, vault: &VaultFixture, role: &str, byte: u8) -> Paired {
    let key = SigningKey::from_bytes(&[byte; 32]);
    let id = DeviceId::new();
    let public =
        json!({"ed25519_public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())});
    let http = reqwest::Client::builder()
        .timeout(NETWORK_TIMEOUT)
        .build()
        .unwrap();
    let challenge: Value = bounded(
        "recovery-pair-create",
        http.post(format!("{}/v1/pairing", runtime.url))
            .bearer_auth(&vault.owner_token)
            .json(&json!({"device_id":id.0,"public_key":public,"profile_fingerprint":vault.fingerprint,"key_epoch":1,"requested_role":role}))
            .send(),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    let challenge_bytes: [u8; 32] = URL_SAFE_NO_PAD
        .decode(challenge["challenge_token"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let proof = pairing_proof_message(
        &challenge_bytes,
        vault.vault,
        id,
        &vault.fingerprint,
        1,
        role,
    );
    let response: Value = bounded(
        "recovery-pair-consume",
        http.post(format!("{}/v1/pairing/consume", runtime.url))
            .json(&json!({"challenge_token":challenge["challenge_token"],"device_id":id.0,"public_key":public,"profile_fingerprint":vault.fingerprint,"key_epoch":1,"signature":URL_SAFE_NO_PAD.encode(key.sign(&proof).to_bytes())}))
            .send(),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    Paired {
        id,
        token: response["device_token"].as_str().unwrap().into(),
    }
}

fn core_client(root: &Path, name: &str, vault: &VaultFixture, device_id: DeviceId) -> Client {
    let client = Client::open(
        ClientConfig {
            database_path: root.join(format!("{name}.db")),
            vault_id: vault.vault,
            device_id,
        },
        DatabaseKey::new(&[name.as_bytes()[0]; 32]).unwrap(),
    )
    .unwrap();
    client
        .unlock(&vault.profile, &vault.header, PHRASE)
        .unwrap();
    client
}

fn route(gateway_device_id: DeviceId) -> GatewayRoute {
    GatewayRoute {
        gateway_device_id,
        subscription_id: "recovery-sim-1".into(),
    }
}

async fn import_restore_snapshot(client: &Client, origin: &str, token: &str) {
    let http = reqwest::Client::builder()
        .timeout(NETWORK_TIMEOUT)
        .build()
        .unwrap();
    let snapshot: Value = bounded(
        "recovery-snapshot",
        http.get(format!("{origin}/v1/snapshot"))
            .bearer_auth(token)
            .send(),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    let high_water = Cursor(
        snapshot["high_water_cursor"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    );
    let count: u64 = snapshot["record_count"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        count, 1,
        "fixture must contain one encrypted command envelope"
    );
    let progress = client
        .begin_snapshot(high_water, count, SnapshotPurpose::Restore)
        .unwrap();
    let page: Value = bounded(
        "recovery-snapshot-records",
        http.get(format!(
            "{origin}/v1/snapshot/records?high_water={}&after=0&limit=200",
            high_water.0
        ))
        .bearer_auth(token)
        .send(),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    let records: Vec<_> = page["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| RawSnapshotRecord {
            cursor: Cursor(record["cursor"].as_str().unwrap().parse().unwrap()),
            envelope_json: serde_json::to_vec(&record["envelope"]).unwrap(),
        })
        .collect();
    let progress = client
        .append_snapshot_raw_page(progress.generation, &records)
        .unwrap();
    assert_eq!(progress.received_records, count);
    client.finish_snapshot(progress.generation).unwrap();
    let applied = client.apply_pending(100).unwrap();
    assert_eq!(applied.snapshot_remaining, 0);
    assert!(client.restore_guarded().unwrap());
}

fn run_recovery_script(cwd: &Path, script: &Path, arguments: &[&str], phase: &str) -> Output {
    let output = Command::new("bash")
        .current_dir(cwd)
        .arg(script)
        .args(arguments)
        .output()
        .unwrap();
    checked(output, phase)
}

fn sha256_hex(root: &Path, name: &str, bytes: &[u8]) -> String {
    let path = root.join(name);
    fs::write(&path, bytes).unwrap();
    let output = checked(
        Command::new("shasum")
            .args(["-a", "256"])
            .arg(&path)
            .output()
            .unwrap(),
        "ciphertext-sha256",
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encrypted_mms_survives_volume_restore_without_historical_carrier_effect() {
    eprintln!("phase=ER01 status=start");
    let root = TempDir::new().unwrap();
    let mut source = ComposeFixture::create(root.path(), "source");
    let mut target = ComposeFixture::create(root.path(), "target");
    source.up_source();

    let source_router = start_router(&source, true).await;
    let vault = create_vault(&source_router.pool).await;
    let desktop_pair = pair(&source_router, &vault, "device", 7).await;
    let gateway_pair = pair(&source_router, &vault, "gateway", 9).await;
    let desktop = core_client(root.path(), "recovery-desktop", &vault, desktop_pair.id);
    let png = fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media/valid-1x1.png"),
    )
    .unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    let png_path = root.path().join("recovery-valid.png");
    fs::write(&png_path, &png).unwrap();
    let attachment = desktop
        .prepare_attachment(&png_path, "image/png", "recovery.png")
        .unwrap();
    let conversation = ConversationId::new();
    let queued = desktop
        .queue_mms(
            OutgoingMms {
                conversation_id: conversation,
                recipients: vec![ADDRESS.into()],
                body: "encrypted recovery MMS".into(),
                subject: None,
                attachment_ids: vec![attachment.attachment_id],
            },
            route(gateway_pair.id),
        )
        .unwrap();
    bounded(
        "recovery-source-media-upload",
        drain_media(
            &desktop,
            &source_router.url,
            &desktop_pair.token,
            root.path(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        bounded(
            "recovery-source-command-upload",
            upload_pending(&desktop, &source_router.url, &desktop_pair.token),
        )
        .await
        .unwrap(),
        1
    );

    let source_command_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM commands WHERE vault_id=$1 AND command_id=$2 AND gateway_device_id=$3",
    )
    .bind(vault.vault.0)
    .bind(queued.command_id.0)
    .bind(gateway_pair.id.0)
    .fetch_one(&source_router.pool)
    .await
    .unwrap();
    assert_eq!(source_command_count, 1);
    let source_envelope: String = sqlx::query_scalar(
        "SELECT envelope::text FROM encrypted_records WHERE vault_id=$1 AND envelope_id=$2",
    )
    .bind(vault.vault.0)
    .bind(queued.envelope_id.0)
    .fetch_one(&source_router.pool)
    .await
    .unwrap();
    let (source_object_key, source_cipher_hash, source_cipher_bytes): (String, String, i64) =
        sqlx::query_as(
            "SELECT object_key, encode(ciphertext_sha256, 'hex'), ciphertext_bytes FROM attachments WHERE vault_id=$1 AND attachment_id=$2",
        )
        .bind(vault.vault.0)
        .bind(attachment.attachment_id.0)
        .fetch_one(&source_router.pool)
        .await
        .unwrap();
    assert!(!source_object_key.is_empty());
    let source_ciphertext = bounded(
        "recovery-source-ciphertext",
        reqwest::Client::builder()
            .timeout(NETWORK_TIMEOUT)
            .build()
            .unwrap()
            .get(format!(
                "{}/v1/attachments/{}",
                source_router.url, attachment.attachment_id.0
            ))
            .bearer_auth(&desktop_pair.token)
            .send(),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .bytes()
    .await
    .unwrap();
    assert_eq!(source_ciphertext.len() as i64, source_cipher_bytes);
    assert_eq!(
        sha256_hex(root.path(), "source-ciphertext.bin", &source_ciphertext),
        source_cipher_hash
    );
    eprintln!(
        "phase=ER01 envelope_bytes={} ciphertext_bytes={} ciphertext_sha256={}",
        source_envelope.len(),
        source_cipher_bytes,
        source_cipher_hash
    );
    drop(desktop);
    source_router.stop().await;
    eprintln!("phase=ER01 status=seeded_and_router_stopped");

    // Docker Desktop reports success but does not propagate bind-mount writes from macOS's
    // /private/var/folders tree. Keep backup artifacts in a repository-local temporary directory
    // so the reviewed helpers exercise a Docker Desktop shared /Users path.
    let recovery_scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/recovery");
    fs::create_dir_all(&recovery_scratch).unwrap();
    let backup_host = tempfile::Builder::new()
        .prefix("encrypted-archive-with-spaces ")
        .tempdir_in(&recovery_scratch)
        .unwrap();
    let backup_parent = backup_host.path();
    let backup_script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../infra/compose/backup.sh");
    let backup_parent_arg = backup_parent.to_str().unwrap();
    run_recovery_script(
        &source.path,
        &backup_script,
        &[backup_parent_arg],
        "reviewed-backup",
    );
    let archive = fs::read_dir(backup_parent)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .expect("backup script must create one archive directory");

    let restore_script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../infra/compose/restore.sh");
    checked(
        target.compose(&["create", "api", "migrate"]),
        "target-create-stopped",
    );
    assert_eq!(target.container_state("api"), "created");
    assert_eq!(target.container_state("migrate"), "created");
    let archive_arg = archive.to_str().unwrap();
    run_recovery_script(
        &target.path,
        &restore_script,
        &[archive_arg, &target.project],
        "reviewed-restore",
    );
    let running_after_restore = target.running_services();
    assert_eq!(running_after_restore, vec!["postgres"]);
    assert!(!running_after_restore.iter().any(|service| service == "api"));
    assert!(
        !running_after_restore
            .iter()
            .any(|service| service == "migrate")
    );
    assert_eq!(target.container_state("api"), "created");
    assert_eq!(target.container_state("migrate"), "created");
    eprintln!("phase=ER02 status=restored_api_and_migrate_stopped");

    checked(
        target.compose(&["up", "-d", "--wait", "seaweedfs"]),
        "target-storage-up",
    );
    let target_router = start_router(&target, false).await;
    let target_command_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM commands WHERE vault_id=$1 AND command_id=$2 AND gateway_device_id=$3",
    )
    .bind(vault.vault.0)
    .bind(queued.command_id.0)
    .bind(gateway_pair.id.0)
    .fetch_one(&target_router.pool)
    .await
    .unwrap();
    assert_eq!(target_command_count, source_command_count);
    let target_envelope: String = sqlx::query_scalar(
        "SELECT envelope::text FROM encrypted_records WHERE vault_id=$1 AND envelope_id=$2",
    )
    .bind(vault.vault.0)
    .bind(queued.envelope_id.0)
    .fetch_one(&target_router.pool)
    .await
    .unwrap();
    let (target_object_key, target_cipher_hash, target_cipher_bytes): (String, String, i64) =
        sqlx::query_as(
            "SELECT object_key, encode(ciphertext_sha256, 'hex'), ciphertext_bytes FROM attachments WHERE vault_id=$1 AND attachment_id=$2",
        )
        .bind(vault.vault.0)
        .bind(attachment.attachment_id.0)
        .fetch_one(&target_router.pool)
        .await
        .unwrap();
    assert_eq!(target_envelope.as_bytes(), source_envelope.as_bytes());
    assert_eq!(target_object_key, source_object_key);
    assert_eq!(target_cipher_hash, source_cipher_hash);
    assert_eq!(target_cipher_bytes, source_cipher_bytes);
    let target_ciphertext = bounded(
        "recovery-target-ciphertext",
        reqwest::Client::builder()
            .timeout(NETWORK_TIMEOUT)
            .build()
            .unwrap()
            .get(format!(
                "{}/v1/attachments/{}",
                target_router.url, attachment.attachment_id.0
            ))
            .bearer_auth(&gateway_pair.token)
            .send(),
    )
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .bytes()
    .await
    .unwrap();
    assert_eq!(target_ciphertext.as_ref(), source_ciphertext.as_ref());
    assert_eq!(
        sha256_hex(root.path(), "target-ciphertext.bin", &target_ciphertext),
        source_cipher_hash
    );
    eprintln!(
        "phase=ER03 envelope_bytes={} ciphertext_bytes={} ciphertext_sha256={} restored_bytes_match=true",
        target_envelope.len(),
        target_cipher_bytes,
        target_cipher_hash
    );

    let restored_gateway = core_client(root.path(), "restored-gateway", &vault, gateway_pair.id);
    import_restore_snapshot(&restored_gateway, &target_router.url, &gateway_pair.token).await;
    let downloads = restored_gateway.pending_downloads().unwrap();
    assert_eq!(downloads.len(), 1);
    assert_eq!(downloads[0].attachment_id, attachment.attachment_id);
    bounded(
        "recovery-target-install",
        drain_media(
            &restored_gateway,
            &target_router.url,
            &gateway_pair.token,
            root.path(),
        ),
    )
    .await
    .unwrap();
    let plaintext = restored_gateway
        .open_native_plaintext(attachment.attachment_id)
        .unwrap();
    assert_eq!(fs::read(plaintext.path()).unwrap(), png);
    drop(plaintext);

    assert!(matches!(
        restored_gateway
            .begin_send_attempt(queued.command_id)
            .unwrap(),
        PermitDecision::Blocked(PermitBlock::Historical | PermitBlock::RestoreGuarded)
    ));
    let effects = effects_path(root.path());
    assert!(!run_one_carrier_effect(&restored_gateway, &effects, false).unwrap());
    assert!(
        !effects.exists(),
        "restored history produced a fake carrier effect"
    );
    eprintln!("phase=ER03 status=decrypted_and_historical_effect_blocked");

    drop(restored_gateway);
    target_router.stop().await;
    source.cleanup();
    target.cleanup();
    let volumes = checked(
        Command::new("docker")
            .args(["volume", "ls", "--format", "{{.Name}}"])
            .output()
            .unwrap(),
        "cleanup-volume-list",
    );
    let volumes = String::from_utf8(volumes.stdout).unwrap();
    assert!(
        !volumes
            .lines()
            .any(|name| name.starts_with(&source.project))
    );
    assert!(
        !volumes
            .lines()
            .any(|name| name.starts_with(&target.project))
    );
    eprintln!("phase=ER04 status=fixture_cleanup_ok");
}
