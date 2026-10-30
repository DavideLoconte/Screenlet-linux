//! The link to the app (PROTOCOL.md §2): TLS 1.3 with a self-signed
//! certificate on each side, length-framed JSON, pairing by a six-digit code
//! both screens show. This PC's identity and the paired tablets live in
//! ~/.config/screenlet.

use crate::protocol::{self, PAIRED_MAX, UNPAIRED_MAX, VERSION};
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::ssl::{SslConnector, SslMethod, SslStream, SslVerifyMode, SslVersion};
use openssl::x509::{X509Name, X509};
use serde_json::{json, Map, Value};
use std::io::{self, ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn config() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("screenlet")
}

fn private(path: &std::path::Path, contents: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let part = path.with_extension("part");
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&part)?.write_all(contents)?;
    std::fs::rename(part, path)
}

/// This PC's certificate and key, made on first use (EC P-256, self-signed).
pub struct Identity {
    pub cert: X509,
    pub key: PKey<Private>,
}

impl Identity {
    pub fn load() -> Result<Identity> {
        let (cert_path, key_path) = (config().join("cert.pem"), config().join("key.pem"));
        if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            return Ok(Identity { cert: X509::from_pem(&cert)?, key: PKey::private_key_from_pem(&key)? });
        }
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
        let key = PKey::from_ec_key(EcKey::generate(&group)?)?;
        let mut name = X509Name::builder()?;
        name.append_entry_by_nid(Nid::COMMONNAME, &format!("Screenlet on {}", hostname()))?;
        let name = name.build();
        let mut cert = X509::builder()?;
        cert.set_version(2)?;
        let mut serial = openssl::bn::BigNum::new()?;
        serial.rand(64, openssl::bn::MsbOption::MAYBE_ZERO, false)?;
        let serial = serial.to_asn1_integer()?;
        cert.set_serial_number(&serial)?;
        cert.set_subject_name(&name)?;
        cert.set_issuer_name(&name)?;
        cert.set_pubkey(&key)?;
        let (from, to) = (openssl::asn1::Asn1Time::days_from_now(0)?, openssl::asn1::Asn1Time::days_from_now(30 * 365)?);
        cert.set_not_before(&from)?;
        cert.set_not_after(&to)?;
        cert.sign(&key, MessageDigest::sha256())?;
        let cert = cert.build();
        private(&key_path, &key.private_key_to_pem_pkcs8()?)?;
        private(&cert_path, &cert.to_pem()?)?;
        Ok(Identity { cert, key })
    }

    pub fn der(&self) -> Vec<u8> {
        self.cert.to_der().unwrap_or_default()
    }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).ok().filter(|s| !s.is_empty())
        .unwrap_or_else(|| "a PC".into())
}

/// The paired tablets, in paired.json: {fingerprint: {"name": ..., "p2p": {...}}}.
pub mod paired {
    use super::*;
    static LOCK: Mutex<()> = Mutex::new(());

    fn path() -> PathBuf {
        config().join("paired.json")
    }

    pub fn all() -> Map<String, Value> {
        std::fs::read(path()).ok().and_then(|d| serde_json::from_slice::<Value>(&d).ok())
            .and_then(|v| v.as_object().cloned()).unwrap_or_default()
    }

    pub fn add(fingerprint: &str, name: &str, p2p: Option<&Value>) {
        let _held = LOCK.lock();
        let mut all = all();
        let entry = all.entry(fingerprint.to_string()).or_insert_with(|| json!({}));
        entry["name"] = json!(name);
        if let Some(p2p) = p2p {
            entry["p2p"] = p2p.clone();
        }
        let _ = private(&path(), &serde_json::to_vec_pretty(&all).unwrap_or_default());
    }

    /// The paired tablet whose fingerprint starts with id (Bonjour's TXT "id").
    pub fn by_id(id: &str) -> Option<String> {
        if id.is_empty() {
            return None;
        }
        all().keys().find(|fp| fp.starts_with(id)).cloned()
    }
}

/// A byte stream to the app: a TCP connection, or one carried by the cable.
/// Reads time out (TimedOut or WouldBlock) so the link can ping meanwhile.
pub trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

#[derive(Debug)]
pub struct Refused(pub String);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Refused {}

pub struct Link {
    tls: SslStream<Box<dyn Stream>>,
    inbox: Vec<u8>,
    pub name: String,
}

fn timed_out(e: &io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

impl Link {
    /// TLS over stream, hello, and pairing when ask is given (it shows the
    /// code and returns whether the user accepted). expect: the id the app
    /// announced; a paired tablet must then present its own certificate.
    pub fn open(stream: Box<dyn Stream>, expect: Option<&str>, ask: Option<&dyn Fn(&str, &str) -> bool>) -> Result<Link> {
        let me = Identity::load()?;
        let mut tls = SslConnector::builder(SslMethod::tls_client())?;
        tls.set_min_proto_version(Some(SslVersion::TLS1_3))?;
        tls.set_certificate(&me.cert)?;
        tls.set_private_key(&me.key)?;
        // No CA signs either certificate: the app's is checked below against
        // the paired list, or by the user comparing the code.
        tls.set_verify_callback(SslVerifyMode::PEER, |_, _| true);
        let mut config = tls.build().configure()?;
        config.set_verify_hostname(false);
        config.set_use_server_name_indication(false);
        let tls = connect_patiently(config.into_ssl("screenlet")?, stream)?;
        let theirs = tls.ssl().peer_certificate().ok_or("the tablet showed no certificate")?.to_der()?;
        let fingerprint = protocol::fingerprint(&theirs);
        let mut link = Link { tls, inbox: Vec::new(), name: "tablet".into() };
        if let Some(pinned) = expect.and_then(paired::by_id) {
            if pinned != fingerprint {
                return Err("an app announced as a paired tablet, with another certificate".into());
            }
        }
        let known = paired::all().contains_key(&fingerprint);
        link.send(&json!({"type": "hello", "name": hostname(), "version": VERSION, "paired": known, "pair": ask.is_some()}))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let hello = link.wait(UNPAIRED_MAX, deadline)?;
        link.name = hello["name"].as_str().filter(|n| !n.is_empty()).unwrap_or("tablet").chars().take(64).collect();
        if hello["version"].as_i64() != Some(VERSION) {
            return Err(format!("{} speaks version {}, this PC {VERSION}: update both", link.name, hello["version"]).into());
        }
        if !known || hello["paired"] != json!(true) {
            let ask = ask.ok_or_else(|| Refused(format!("{} is not paired", link.name)))?;
            let commit = link.wait(UNPAIRED_MAX, deadline)?;
            let mut nonce = [0u8; 16];
            openssl::rand::rand_bytes(&mut nonce)?;
            link.send(&json!({"type": "pair-nonce", "nonce": protocol::hex(&nonce)}))?;
            let reveal = link.wait(UNPAIRED_MAX, deadline)?;
            let theirs_nonce = reveal["nonce"].as_str().and_then(protocol::unhex).filter(|n| n.len() == 16)
                .ok_or_else(|| Refused(format!("{} sent no number", link.name)))?;
            if commit["type"] != "pair-commit" || commit["commit"].as_str() != Some(&protocol::commitment(&theirs_nonce, &theirs, &me.der())) {
                return Err(Refused(format!("{} broke its commitment: not paired", link.name)).into());
            }
            let code = protocol::pairing_code(&me.der(), &theirs, &nonce, &theirs_nonce);
            eprintln!("link: pairing with {}, code {code}", link.name);
            let accepted = ask(&link.name, &code);
            link.send(&json!({"type": if accepted { "pair-ok" } else { "pair-no" }}))?;
            let answer = link.wait(UNPAIRED_MAX, Instant::now() + Duration::from_secs(150))?;
            if !accepted || answer["type"] != "pair-ok" {
                return Err(Refused(format!("not paired with {}", link.name)).into());
            }
        }
        // Stored (again): the name may change, and the tablet's Wi-Fi Direct group comes with a paired hello.
        paired::add(&fingerprint, &link.name, hello.get("p2p"));
        Ok(link)
    }

    pub fn send(&mut self, message: &Value) -> Result<()> {
        let body = serde_json::to_vec(message)?;
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend(body);
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut at = 0;
        while at < frame.len() {
            match self.tls.write(&frame[at..]) {
                Ok(0) => return Err("the tablet closed the link".into()),
                Ok(n) => at += n,
                Err(e) if timed_out(&e) && Instant::now() < deadline => continue,
                Err(e) => return Err(e.into()),
            }
        }
        self.tls.flush()?;
        Ok(())
    }

    /// The next message if one arrives before the stream's read timeout.
    pub fn poll(&mut self, limit: usize) -> Result<Option<Value>> {
        loop {
            if self.inbox.len() >= 4 {
                let n = u32::from_be_bytes(self.inbox[..4].try_into()?) as usize;
                if n == 0 || n > limit {
                    return Err(format!("a message of {n} bytes").into());
                }
                if self.inbox.len() >= 4 + n {
                    let message = serde_json::from_slice(&self.inbox[4..4 + n])?;
                    self.inbox.drain(..4 + n);
                    return Ok(Some(message));
                }
            }
            let mut chunk = [0u8; 4096];
            match self.tls.read(&mut chunk) {
                Ok(0) => return Err("the tablet closed the link".into()),
                Ok(n) => self.inbox.extend_from_slice(&chunk[..n]),
                Err(e) if timed_out(&e) => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn wait(&mut self, limit: usize, deadline: Instant) -> Result<Value> {
        loop {
            if let Some(message) = self.poll(limit)? {
                return Ok(message);
            }
            if Instant::now() > deadline {
                return Err("the tablet did not answer in time".into());
            }
        }
    }

    pub fn request(&mut self, message: &Value) -> Result<Value> {
        self.send(message)?;
        self.wait(PAIRED_MAX, Instant::now() + Duration::from_secs(30))
    }

    /// Pings until the app hangs up or stopped() says so, then says stop.
    pub fn hold(&mut self, stopped: impl Fn() -> bool) -> Result<()> {
        let mut pinged = Instant::now();
        while !stopped() {
            self.poll(PAIRED_MAX)?;
            if pinged.elapsed() >= Duration::from_secs(5) {
                self.send(&json!({"type": "ping"}))?;
                pinged = Instant::now();
            }
        }
        let _ = self.send(&json!({"type": "stop"}));
        Ok(())
    }
}

/// The handshake over a stream whose reads time out: tried again until it is done.
fn connect_patiently(ssl: openssl::ssl::Ssl, stream: Box<dyn Stream>) -> Result<SslStream<Box<dyn Stream>>> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut attempt = ssl.connect(stream);
    loop {
        match attempt {
            Ok(tls) => return Ok(tls),
            Err(openssl::ssl::HandshakeError::WouldBlock(mid)) if Instant::now() < deadline => attempt = mid.handshake(),
            Err(openssl::ssl::HandshakeError::Failure(mid)) => return Err(format!("TLS: {}", mid.error()).into()),
            Err(openssl::ssl::HandshakeError::WouldBlock(mid)) => return Err(format!("TLS: {}", mid.error()).into()),
            Err(openssl::ssl::HandshakeError::SetupFailure(e)) => return Err(format!("TLS: {e}").into()),
        }
    }
}
