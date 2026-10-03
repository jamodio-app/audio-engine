//! Lecture du manifeste de mise à jour, telle que la fait l'updater.
//!
//! jamodio.com sert `latest.json` compressé en brotli à TOUS les clients, même
//! à ceux qui ne l'acceptent pas (mesuré le 02/10/2026). Le plugin updater tire
//! `reqwest` sans aucun décodeur ; c'est la déclaration de `reqwest` (gzip +
//! brotli) dans le Cargo.toml de l'agent qui les active pour le crate partagé.
//! Ces tests utilisent un client `reqwest` par défaut, comme le plugin : s'ils
//! cassent, l'updater ne sait plus lire le manifeste.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PLAIN: &str = r#"{"version":"0.6.6","notes":"Jamodio Audio Engine 0.6.6","platforms":{"darwin-aarch64":{"signature":"sig","url":"https://jamodio.com/installer/v/0.6.6/a.tar.gz"}}}"#;

// PLAIN compressé (zlib de Node : brotliCompressSync / gzipSync, mtime 0).
const BROTLI: &[u8] = &[
    27, 161, 0, 72, 140, 211, 21, 243, 162, 132, 117, 15, 194, 181, 165, 126, 221, 205, 32, 43,
    193, 210, 134, 47, 239, 68, 125, 216, 131, 130, 159, 58, 44, 125, 79, 143, 3, 11, 76, 219, 42,
    138, 218, 218, 130, 192, 194, 44, 139, 249, 96, 215, 147, 133, 39, 65, 38, 16, 190, 189, 68,
    147, 84, 49, 232, 191, 192, 44, 38, 70, 72, 19, 230, 234, 119, 43, 209, 214, 205, 60, 134, 208,
    162, 114, 127, 58, 6, 197, 243, 139, 103, 114, 207, 237, 176, 197, 171, 71, 85, 116, 68, 36,
    224, 58, 151, 79, 237, 228, 203, 63, 135, 122, 79, 125, 213, 82, 93, 210, 149, 82, 127, 126,
];
const GZIP: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 0, 19, 53, 140, 177, 14, 194, 48, 12, 68, 127, 165, 242, 12, 9, 3,
    202, 208, 141, 129, 133, 191, 176, 218, 144, 6, 37, 118, 229, 184, 173, 68, 149, 127, 39, 5,
    177, 156, 238, 78, 119, 111, 135, 213, 75, 137, 76, 208, 195, 197, 56, 227, 224, 4, 196, 234,
    75, 203, 15, 204, 60, 70, 238, 110, 203, 161, 119, 10, 145, 124, 247, 31, 205, 9, 245, 201,
    146, 219, 112, 135, 17, 101, 139, 116, 70, 148, 97, 114, 215, 163, 41, 49, 16, 234, 34, 190,
    113, 154, 111, 135, 69, 82, 243, 147, 234, 92, 122, 107, 95, 63, 182, 25, 56, 219, 72, 69, 49,
    37, 47, 118, 181, 95, 188, 69, 163, 40, 38, 188, 161, 214, 250, 1, 235, 245, 61, 138, 162, 0,
    0, 0,
];

/// Sert UNE réponse `body` encodée `encoding` ; la tâche rend l'en-tête
/// Accept-Encoding de la requête reçue.
async fn serve_once(
    encoding: &'static str,
    body: &'static [u8],
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/installer/latest.json",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 1024];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut buf).await.unwrap();
            assert!(n > 0, "requête interrompue");
            req.extend_from_slice(&buf[..n]);
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-encoding: {encoding}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        sock.write_all(head.as_bytes()).await.unwrap();
        sock.write_all(body).await.unwrap();
        String::from_utf8_lossy(&req)
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("accept-encoding:")
                    .map(|v| v.trim().to_string())
            })
            .unwrap_or_default()
    });
    (url, server)
}

async fn read_manifest(encoding: &'static str, body: &'static [u8]) -> (serde_json::Value, String) {
    let (url, server) = serve_once(encoding, body).await;
    // Comme le plugin updater (updater.rs) : fournisseur TLS, puis client par défaut.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    let client = reqwest::ClientBuilder::new().build().expect("client HTTP");
    let manifest = client
        .get(&url)
        .send()
        .await
        .expect("requête")
        .json::<serde_json::Value>()
        .await
        .expect("manifeste illisible : décodeur absent du client de l'updater ?");
    (manifest, server.await.unwrap())
}

#[tokio::test]
async fn manifeste_brotli_lu() {
    let (manifest, accept) = read_manifest("br", BROTLI).await;
    assert_eq!(
        manifest,
        serde_json::from_str::<serde_json::Value>(PLAIN).unwrap()
    );
    assert!(
        accept.contains("br"),
        "le client n'annonce pas brotli : {accept:?}"
    );
}

#[tokio::test]
async fn manifeste_gzip_lu() {
    let (manifest, accept) = read_manifest("gzip", GZIP).await;
    assert_eq!(
        manifest,
        serde_json::from_str::<serde_json::Value>(PLAIN).unwrap()
    );
    assert!(
        accept.contains("gzip"),
        "le client n'annonce pas gzip : {accept:?}"
    );
}

/// L'updater lit jamodio.com, plus GitHub (le dépôt de l'agent devient privé).
#[test]
fn endpoint_sur_jamodio() {
    let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
    assert_eq!(
        conf["plugins"]["updater"]["endpoints"],
        serde_json::json!(["https://jamodio.com/installer/latest.json"])
    );
}
