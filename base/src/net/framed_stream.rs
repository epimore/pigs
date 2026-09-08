use super::transport::{
    LengthDelimitedCodec, TransportAddress, TransportError, TransportMessage, TransportResult,
};
use bytes::Bytes;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

const READ_BUFFER_SIZE: usize = 64 * 1024;

pub(crate) async fn run<S>(
    stream: S,
    decoder: LengthDelimitedCodec,
    mut outbound: mpsc::Receiver<Bytes>,
    inbound: mpsc::Sender<TransportMessage>,
    cancel: CancellationToken,
    peer: Option<TransportAddress>,
    transport_name: &'static str,
) -> TransportResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let read_cancel = cancel.clone();
    let read_loop = async move {
        let mut decoder = decoder;
        let mut buffer = vec![0u8; READ_BUFFER_SIZE];
        loop {
            let read = tokio::select! {
                _ = read_cancel.cancelled() => return Ok(()),
                read = reader.read(&mut buffer) => read.map_err(|error| {
                    TransportError::from_io(&format!("read {transport_name}"), &error)
                })?,
            };
            if read == 0 {
                decoder.finish()?;
                return Ok(());
            }
            for payload in decoder.push(&buffer[..read])? {
                let message = TransportMessage {
                    payload,
                    peer: peer.clone(),
                };
                tokio::select! {
                    _ = read_cancel.cancelled() => return Ok(()),
                    sent = inbound.send(message) => {
                        if sent.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
        }
    };
    let write_cancel = cancel.clone();
    let write_loop = async move {
        loop {
            let payload = tokio::select! {
                _ = write_cancel.cancelled() => return Ok(()),
                payload = outbound.recv() => match payload {
                    Some(payload) => payload,
                    None => return Ok(()),
                }
            };
            writer.write_all(&payload).await.map_err(|error| {
                TransportError::from_io(&format!("write {transport_name}"), &error)
            })?;
        }
    };

    tokio::pin!(read_loop);
    tokio::pin!(write_loop);
    let result = tokio::select! {
        result = &mut read_loop => result,
        result = &mut write_loop => result,
        _ = cancel.cancelled() => Ok(()),
    };
    cancel.cancel();
    result
}
