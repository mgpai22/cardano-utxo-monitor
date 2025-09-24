use std::marker::PhantomData;
use std::path::Path;

use cml_core::serialization::Serialize;
use cml_crypto::blake2b256;
use log::{trace, warn};
use pallas_network::miniprotocols::handshake::RefuseReason;
use pallas_network::miniprotocols::localtxsubmission::{EraTx, Response, TxValidationError};
use pallas_network::miniprotocols::{
    handshake, localtxsubmission, PROTOCOL_N2C_HANDSHAKE, PROTOCOL_N2C_TX_SUBMISSION,
};
use pallas_network::multiplexer;
use pallas_network::multiplexer::{Bearer, RunningPlexer};

pub struct LocalTxSubmissionClient<const ERA: u16, Tx> {
    plexer: RunningPlexer,
    tx_submission: localtxsubmission::Client,
    tx: PhantomData<Tx>,
}

impl<const ERA: u16, Tx> LocalTxSubmissionClient<ERA, Tx> {
    async fn from_bearer(bearer: Bearer, magic: u64) -> Result<Self, Error> {
        let mut mplex = multiplexer::Plexer::new(bearer);

        let hs_channel = mplex.subscribe_client(PROTOCOL_N2C_HANDSHAKE);
        let ts_channel = mplex.subscribe_client(PROTOCOL_N2C_TX_SUBMISSION);

        let plexer = mplex.spawn();

        let versions = handshake::n2c::VersionTable::v10_and_above(magic);
        let mut client = handshake::Client::new(hs_channel);

        let handshake = client
            .handshake(versions)
            .await
            .map_err(Error::HandshakeProtocol)?;

        if let handshake::Confirmation::Rejected(reason) = handshake {
            return Err(Error::HandshakeRefused(reason));
        }

        let ts_client = localtxsubmission::Client::new(ts_channel);

        Ok(Self {
            plexer,
            tx_submission: ts_client,
            tx: PhantomData::default(),
        })
    }

    #[cfg(not(target_os = "windows"))]
    pub async fn init(path: impl AsRef<Path>, magic: u64) -> Result<Self, Error> {
        let bearer = Bearer::connect_unix(path)
            .await
            .map_err(Error::ConnectFailure)?;

        Self::from_bearer(bearer, magic).await
    }

    #[cfg(target_os = "windows")]
    pub async fn init(path: impl AsRef<Path>, magic: u64) -> Result<Self, Error> {
        use std::ffi::OsString;
        use tokio::io::ErrorKind;

        let pipe_name: OsString = path.as_ref().as_os_str().into();
        let mut attempts = 0usize;
        let bearer = loop {
            match Bearer::connect_named_pipe(&pipe_name) {
                Ok(bearer) => break bearer,
                Err(err)
                    if matches!(
                        err.kind(),
                        ErrorKind::NotFound | ErrorKind::WouldBlock | ErrorKind::ConnectionRefused
                    ) =>
                {
                    attempts += 1;
                    if attempts == 1 {
                        warn!(
                            "Waiting for Cardano node pipe {}: {}",
                            pipe_name.to_string_lossy(),
                            err
                        );
                    }
                    if attempts > 50 {
                        return Err(Error::ConnectFailure(err));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(err) => return Err(Error::ConnectFailure(err)),
            }
        };

        Self::from_bearer(bearer, magic).await
    }

    pub async fn submit_tx(&mut self, tx: Tx) -> Result<Response<TxValidationError>, Error>
    where
        Tx: Serialize,
    {
        let tx_bytes = tx.to_cbor_bytes();
        let hash = hex::encode(&blake2b256(&tx_bytes)[0..8]);
        trace!("[{}] Going to submit TX", hash);
        let result = self
            .tx_submission
            .submit_tx(EraTx(ERA, tx_bytes))
            .await
            .map_err(Error::TxSubmissionProtocol);
        trace!("[{}] Submit attempt finished", hash);
        result
    }

    pub async fn close(self) {
        self.plexer.abort().await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("error connecting bearer")]
    ConnectFailure(#[source] tokio::io::Error),

    #[error("handshake protocol error")]
    HandshakeProtocol(handshake::Error),

    #[error("chain-sync protocol error")]
    TxSubmissionProtocol(#[source] localtxsubmission::Error),

    #[error("handshake version not accepted")]
    HandshakeRefused(RefuseReason),
}