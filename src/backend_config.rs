use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use aws_config::BehaviorVersion;
use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::sign;
use aws_sigv4::http_request::SignableBody;
use aws_sigv4::http_request::SignableRequest;
use aws_sigv4::http_request::SigningSettings;
use aws_sigv4::sign::v4;
use bytes::{Bytes, BytesMut};
use eyre::{eyre, Result};
use futures::SinkExt;
use postgres_native_tls::TlsConnector;
use postgres_native_tls::TlsStream;
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_postgres::tls::TlsConnect;
use tokio_stream::StreamExt;
use tokio_util::codec::BytesCodec;
use tokio_util::codec::Framed;

#[derive(Debug)]
pub struct DbSpec {
    user: String,
    database: String,
}

impl DbSpec {
    pub fn new(user: String, database: String) -> DbSpec {
        DbSpec { user, database }
    }

    fn startup_message(&self) -> Result<Bytes> {
        let mut params = vec![("client_encoding", "UTF8")];
        params.push(("user", self.user.as_str()));
        params.push(("database", self.database.as_str()));
        let mut buf = BytesMut::new();
        frontend::startup_message(params, &mut buf)?;
        Ok(buf.freeze())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Addr {
    hostname: String,
    port: u16,
}

impl Addr {
    fn connect_str(&self) -> String {
        format!("{}:{}", self.hostname, self.port)
    }
}

#[derive(Clone, Debug)]
pub struct BackendConfig {
    endpoint: Addr,
    connect_endpoint: Addr,
    password_cache_ttl: Duration,
    password_cache: Arc<Mutex<PasswordCache>>,
}

impl BackendConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_vars(std::env::vars())
    }

    fn from_vars<I, K, V>(vars: I) -> Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let vars: HashMap<String, String> = vars
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();

        let db_host = required_var(&vars, "DB_HOST")?;
        let db_port = parse_port(&vars, "DB_PORT", 5432)?;
        let password_cache_ttl = parse_duration_secs(
            &vars,
            "PASSWORD_CACHE_TTL_SECS",
            DEFAULT_PASSWORD_CACHE_TTL_SECS,
        )?;
        let connect_host = vars
            .get("CONNECT_HOST")
            .filter(|value| !value.is_empty())
            .cloned()
            .unwrap_or_else(|| db_host.clone());
        let connect_port = parse_port(&vars, "CONNECT_PORT", db_port)?;

        Ok(BackendConfig {
            endpoint: Addr {
                hostname: db_host,
                port: db_port,
            },
            connect_endpoint: Addr {
                hostname: connect_host,
                port: connect_port,
            },
            password_cache_ttl,
            password_cache: Arc::new(Mutex::new(PasswordCache::default())),
        })
    }

    fn connect_endpoint(&self) -> &Addr {
        &self.connect_endpoint
    }

    pub async fn get_server_conn(&self, db_spec: DbSpec) -> Result<TlsStream<TcpStream>> {
        let password = self.get_password(db_spec.user.as_str()).await?;
        let stream = self.backend_conn(db_spec, password).await?;
        Ok(stream)
    }

    async fn get_password(&self, username: &str) -> Result<String> {
        let key = PasswordCacheKey {
            hostname: self.endpoint.hostname.clone(),
            port: self.endpoint.port,
            username: username.to_owned(),
        };

        let now = Instant::now();
        if let Some(password) = self.password_cache.lock().await.get(&key, now) {
            return Ok(password);
        }

        let password = get_rds_password(
            self.endpoint.hostname.as_ref(),
            self.endpoint.port,
            username,
        )
        .await?;

        self.password_cache.lock().await.insert(
            key,
            password.clone(),
            Instant::now() + self.password_cache_ttl,
        );

        Ok(password)
    }

    async fn backend_conn(
        &self,
        db_spec: DbSpec,
        password: String,
    ) -> Result<TlsStream<TcpStream>> {
        let stream = TcpStream::connect(self.connect_endpoint().connect_str()).await?;
        let mut tls_stream = self.upgrade_to_tls(stream).await?;
        send_password(&db_spec, &mut tls_stream, password).await?;
        Ok(tls_stream)
    }

    async fn upgrade_to_tls<S>(&self, mut tcp: S) -> Result<TlsStream<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin + 'static + Send,
    {
        let mut buf = BytesMut::new();
        frontend::ssl_request(&mut buf);
        tcp.write_all(&buf).await?;
        let mut buf = [0];
        tcp.read_exact(&mut buf).await?;
        if buf[0] != b'S' {
            Err(eyre!("server does not support TLS"))
        } else {
            let native_conn = native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .build()?;
            let tls = TlsConnector::new(native_conn, self.endpoint.hostname.as_ref());
            let stream = tls.connect(tcp).await?;
            Ok(stream)
        }
    }
}

const DEFAULT_PASSWORD_CACHE_TTL_SECS: u64 = 10 * 60;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct PasswordCacheKey {
    hostname: String,
    port: u16,
    username: String,
}

#[derive(Clone, Debug)]
struct PasswordCacheEntry {
    password: String,
    expires_at: Instant,
}

#[derive(Debug, Default)]
struct PasswordCache {
    passwords: HashMap<PasswordCacheKey, PasswordCacheEntry>,
}

impl PasswordCache {
    fn get(&mut self, key: &PasswordCacheKey, now: Instant) -> Option<String> {
        match self.passwords.get(key) {
            Some(entry) if entry.expires_at > now => Some(entry.password.clone()),
            Some(_) => {
                self.passwords.remove(key);
                None
            }
            None => None,
        }
    }

    fn insert(&mut self, key: PasswordCacheKey, password: String, expires_at: Instant) {
        self.passwords.insert(
            key,
            PasswordCacheEntry {
                password,
                expires_at,
            },
        );
    }
}

fn required_var(vars: &HashMap<String, String>, name: &str) -> Result<String> {
    vars.get(name)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| eyre!("missing required environment variable {name}"))
}

fn parse_port(vars: &HashMap<String, String>, name: &str, default: u16) -> Result<u16> {
    match vars.get(name).filter(|value| !value.is_empty()) {
        Some(value) => value
            .parse::<u16>()
            .map_err(|_| eyre!("{name} must be a valid TCP port, got {value:?}")),
        None => Ok(default),
    }
}

fn parse_duration_secs(
    vars: &HashMap<String, String>,
    name: &str,
    default: u64,
) -> Result<Duration> {
    match vars.get(name).filter(|value| !value.is_empty()) {
        Some(value) => value.parse::<u64>().map(Duration::from_secs).map_err(|_| {
            eyre!("{name} must be a non-negative integer number of seconds, got {value:?}")
        }),
        None => Ok(Duration::from_secs(default)),
    }
}

const HTTPS_LEN: usize = "https://".len();

pub async fn get_rds_password(rds_host: &str, port: u16, username: &str) -> Result<String> {
    let config = aws_config::load_defaults(BehaviorVersion::v2023_11_09()).await;
    let region = config
        .region()
        .ok_or_else(|| eyre!("AWS region not resolved; set AWS_REGION or AWS_DEFAULT_REGION"))?;
    let provider = config
        .credentials_provider()
        .ok_or(eyre!("no credentials provider found"))?;
    let creds = provider.provide_credentials().await?;
    let identity = creds.into();

    let mut signing_settings = SigningSettings::default();
    signing_settings.expires_in = Some(Duration::from_secs(900));
    signing_settings.signature_location = aws_sigv4::http_request::SignatureLocation::QueryParams;

    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region.as_ref())
        .name("rds-db")
        .time(SystemTime::now())
        .settings(signing_settings)
        .build()?;

    let mut url = url::Url::parse(&format!(
        "https://{rds_host}:{port}/?Action=connect&DBUser={username}"
    ))?;

    let signable_request = SignableRequest::new(
        "GET",
        url.as_str(),
        std::iter::empty(),
        SignableBody::Bytes(&[]),
    )?;
    let (instructions, _) = sign(signable_request, &signing_params.into())?.into_parts();

    for (name, value) in instructions.params() {
        url.query_pairs_mut().append_pair(name, value);
    }

    let password = url.to_string().split_off(HTTPS_LEN);
    Ok(password)
}

async fn send_password<S>(db_spec: &DbSpec, stream: &mut S, password: String) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let buf = db_spec.startup_message()?;
    let mut framed = Framed::new(stream, BytesCodec::new());
    framed.send(buf).await?;
    if let Some(mut resp) = framed.try_next().await? {
        if let Ok(Some(Message::AuthenticationCleartextPassword)) = Message::parse(&mut resp) {
            let mut pw_buf = BytesMut::new();
            frontend::password_message(password.as_ref(), &mut pw_buf)?;
            framed.send(pw_buf.freeze()).await?;
            Ok(())
        } else {
            Err(eyre!("Unexpected auth prompt"))
        }
    } else {
        Err(eyre!("Unexpected backed message"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_db_host() {
        let err = BackendConfig::from_vars([] as [(&str, &str); 0]).unwrap_err();
        assert!(err.to_string().contains("DB_HOST"));
    }

    #[test]
    fn config_defaults_ports_and_connect_endpoint() {
        let config = BackendConfig::from_vars([("DB_HOST", "db.example.com")]).unwrap();

        assert_eq!(
            config.endpoint,
            Addr {
                hostname: "db.example.com".to_owned(),
                port: 5432,
            }
        );
        assert_eq!(
            config.connect_endpoint,
            Addr {
                hostname: "db.example.com".to_owned(),
                port: 5432,
            }
        );
        assert_eq!(
            config.password_cache_ttl,
            Duration::from_secs(DEFAULT_PASSWORD_CACHE_TTL_SECS)
        );
    }

    #[test]
    fn config_allows_connect_endpoint_override() {
        let config = BackendConfig::from_vars([
            ("DB_HOST", "db.example.com"),
            ("DB_PORT", "5433"),
            ("CONNECT_HOST", "localhost"),
            ("CONNECT_PORT", "15432"),
        ])
        .unwrap();

        assert_eq!(
            config.endpoint,
            Addr {
                hostname: "db.example.com".to_owned(),
                port: 5433,
            }
        );
        assert_eq!(
            config.connect_endpoint,
            Addr {
                hostname: "localhost".to_owned(),
                port: 15432,
            }
        );
    }

    #[test]
    fn config_allows_password_cache_ttl_override() {
        let config = BackendConfig::from_vars([
            ("DB_HOST", "db.example.com"),
            ("PASSWORD_CACHE_TTL_SECS", "30"),
        ])
        .unwrap();

        assert_eq!(config.password_cache_ttl, Duration::from_secs(30));
    }

    #[test]
    fn config_rejects_invalid_password_cache_ttl() {
        let err = BackendConfig::from_vars([
            ("DB_HOST", "db.example.com"),
            ("PASSWORD_CACHE_TTL_SECS", "nope"),
        ])
        .unwrap_err();

        assert!(err.to_string().contains("PASSWORD_CACHE_TTL_SECS"));
    }

    #[test]
    fn config_rejects_invalid_ports() {
        let err = BackendConfig::from_vars([("DB_HOST", "db.example.com"), ("DB_PORT", "nope")])
            .unwrap_err();

        assert!(err.to_string().contains("DB_PORT"));
    }

    #[test]
    fn password_cache_returns_unexpired_password() {
        let mut cache = PasswordCache::default();
        let key = PasswordCacheKey {
            hostname: "db.example.com".to_owned(),
            port: 5432,
            username: "db_user".to_owned(),
        };
        let now = Instant::now();

        cache.insert(
            key.clone(),
            "password".to_owned(),
            now + Duration::from_secs(DEFAULT_PASSWORD_CACHE_TTL_SECS),
        );

        assert_eq!(cache.get(&key, now), Some("password".to_owned()));
    }

    #[test]
    fn password_cache_evicts_expired_password() {
        let mut cache = PasswordCache::default();
        let key = PasswordCacheKey {
            hostname: "db.example.com".to_owned(),
            port: 5432,
            username: "db_user".to_owned(),
        };
        let now = Instant::now();

        cache.insert(key.clone(), "password".to_owned(), now);

        assert_eq!(cache.get(&key, now + Duration::from_secs(1)), None);
        assert!(cache.passwords.is_empty());
    }
}
