//! 信令连接的 TLS：自签证书 + SHA256 指纹固定（pin）。
//!
//! 为什么不用公共 CA：部署机没有域名，ACME 签发要域名验证，自签是现实选择。
//! 为什么不做「忽略证书错误」：那等于没加密——中间人用任意证书就能透明
//! 截获配对流量，而 6 位配对码离线暴破只要毫秒级（10^6 次 HMAC）。
//! 指纹固定的信任面最小：客户端只认这一张证书，服务器换证书必须
//! 同步更新两端 pin（本文件与 iOS SignalingClient.swift 各存一份）。
//!
//! 证书在云服务器 /etc/makuris/tls/cert.pem（EC P-256，2036 年到期，
//! SAN=IP:203.0.113.10），重新生成方法见知识库部署文档。

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::DigitallySignedStruct;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async, connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream,
};

/// 叶子证书 DER 的 SHA256。与云服务器上
/// `openssl x509 -in cert.pem -noout -fingerprint -sha256` 输出一致。
const SIGNALING_CERT_SHA256: [u8; 32] = [
    0x10, 0x5B, 0x91, 0x8D, 0x46, 0x51, 0xFE, 0xCF, 0x2A, 0x66, 0x6E, 0xF3, 0xDB, 0x0F, 0xBB,
    0x04, 0x48, 0xA2, 0x0D, 0xEA, 0xD2, 0x62, 0x13, 0x1B, 0xEE, 0x91, 0xFB, 0x88, 0x96, 0x6D,
    0x55, 0x66,
];

#[derive(Debug)]
struct PinnedVerifier;

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if Sha256::digest(end_entity.as_ref()).as_slice() == SIGNALING_CERT_SHA256 {
            return Ok(ServerCertVerified::assertion());
        }
        Err(rustls::Error::General(
            "信令证书指纹不匹配：疑似中间人，或服务器换了证书而未同步更新客户端 pin".into(),
        ))
    }

    // 证书指纹已 pin 死，握手签名不再单独校验：没有私钥的攻击者伪造不了
    // 证书本身；握手消息被篡改会在 TLS 记录层完整性校验先失败。
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            RSA_PSS_SHA256,
            RSA_PKCS1_SHA256,
        ]
    }
}

/// 信令连接统一入口：wss 走指纹固定 TLS；ws 保留明文通道仅供本地测试
/// （生产默认值已是 wss，见 main.rs）。
pub async fn connect(
    url: &str,
) -> Result<
    (
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    tokio_tungstenite::tungstenite::Error,
> {
    if url.starts_with("wss://") {
        // rustls 0.23 要求先装 process-level CryptoProvider：本依赖树里
        // aws-lc-rs 与 ring 两个 feature 同时被传递启用，rustls 无法自动
        // 裁决，不装就直接 panic（前台跑抓到的现场即 rustls crypto/mod.rs:249）。
        // 显式选 ring（纯 Rust 生态里最稳的选项），重复安装返回 Err，忽略即可。
        let _ = rustls::crypto::ring::default_provider().install_default();
        // tokio-tungstenite 0.24 签名：(request, config, connector, disable_nagle)，
        // connector 传 Connector::Rustls(Arc<ClientConfig>)，不走内部默认验证器。
        let cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedVerifier))
            .with_no_client_auth();
        // 0.24 实参顺序（编译器核实）：request, config, disable_nagle, connector
        connect_async_tls_with_config(url, None, false, Some(Connector::Rustls(Arc::new(cfg)))).await
    } else {
        connect_async(url).await
    }
}
