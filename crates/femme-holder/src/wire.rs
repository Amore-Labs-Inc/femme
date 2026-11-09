//! Framing on the holder socket: a `u32` little-endian length, then a postcard message, no
//! compression. Used by the holder itself and by clients (`femmed`, tests).

use std::io;

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest message either side accepts (matches `femme_proto::codec::MAX_FRAME`).
pub const MAX_MSG: usize = 16 << 20;

fn invalid(e: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// One wire frame for `msg`: 4-byte little-endian payload length, then the postcard payload.
pub fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let mut v = vec![0u8; 4];
    postcard::to_io(msg, &mut v).map_err(invalid)?;
    let n = v.len() - 4;
    if n > MAX_MSG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message of {n} bytes exceeds the limit"),
        ));
    }
    v[..4].copy_from_slice(&(n as u32).to_le_bytes());
    Ok(v)
}

/// Read one message. `buf` is scratch space reused between calls. `Ok(None)` means the peer closed
/// the stream. Not cancel-safe: drop the connection if this future is dropped mid-read.
pub async fn read<T: DeserializeOwned, R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_MSG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message of {n} bytes exceeds the limit"),
        ));
    }
    buf.clear();
    buf.resize(n, 0);
    r.read_exact(buf).await?;
    postcard::from_bytes(buf).map(Some).map_err(invalid)
}

/// Encode and write one message.
pub async fn write<T: Serialize, W: AsyncWrite + Unpin>(w: &mut W, msg: &T) -> io::Result<()> {
    w.write_all(&encode(msg)?).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::holder::{HolderReq, Target};

    #[tokio::test]
    async fn roundtrip_and_eof() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let msg = HolderReq::Signal {
            target: Target::Pane(3),
            sig: 15,
        };
        write(&mut a, &msg).await.unwrap();
        write(&mut a, &HolderReq::Hello { v: 1 }).await.unwrap();
        drop(a);
        let mut buf = Vec::new();
        assert_eq!(
            read::<HolderReq, _>(&mut b, &mut buf).await.unwrap(),
            Some(msg)
        );
        assert_eq!(
            read::<HolderReq, _>(&mut b, &mut buf).await.unwrap(),
            Some(HolderReq::Hello { v: 1 })
        );
        assert_eq!(read::<HolderReq, _>(&mut b, &mut buf).await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_oversized_and_garbage() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&(u32::MAX).to_le_bytes()).await.unwrap();
        let mut buf = Vec::new();
        assert!(read::<HolderReq, _>(&mut b, &mut buf).await.is_err());
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[2, 0, 0, 0, 0xff, 0xff]).await.unwrap();
        assert!(read::<HolderReq, _>(&mut b, &mut buf).await.is_err());
    }
}
