//! A body cut short, classified by the four sibling client errors.
#![cfg(feature = "tokenization")]

use crate::Permanence;
use crate::broker::{AlpacaBrokerApiError, AlpacaMarketDataError};
use crate::tokenization::AlpacaTokenizationError;
use crate::wallet::AlpacaWalletError;

/// The error `Response::bytes`, which every Alpaca client here reads a body
/// with, gives for a 200 whose body is cut short: the server declares a
/// longer `Content-Length` than it sends, then closes, as a connection reset
/// mid answer does. reqwest reports it as a decode error.
async fn body_cut_short_error() -> reqwest::Error {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "the client closed before sending its request");
            request.extend_from_slice(&chunk[..read]);
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n{\"")
            .await
            .unwrap();
    });
    let error = reqwest::get(url).await.unwrap().bytes().await.unwrap_err();
    assert!(error.is_decode(), "{error:?}");
    error
}

/// A body cut short while it streams is a transport failure a fresh read
/// can clear, though reqwest reports it as a decode error.
#[tokio::test]
async fn a_body_cut_short_is_transient_for_every_sibling_client() {
    let permanences = [
        AlpacaBrokerApiError::HttpClient(body_cut_short_error().await).permanence(),
        AlpacaMarketDataError::Http(body_cut_short_error().await).permanence(),
        AlpacaWalletError::Reqwest(body_cut_short_error().await).permanence(),
        AlpacaTokenizationError::Reqwest(body_cut_short_error().await).permanence(),
    ];

    assert_eq!(permanences, [Permanence::Transient; 4]);
}
