//! A Noise-encrypted wrapper around a `TcpStream`, providing framed read/write I/O using the SV2
//! protocol and a stateful Noise handshake.
//!
//! This module provides `NoiseTcpStream`, which wraps a `TcpStream` and performs a Noise-based
//! authenticated key exchange, as the initiator ([`NoiseTcpStream::connect`]) or as the responder
//! ([`NoiseTcpStream::accept`]).
//!
//! After a successful handshake, the stream can be split into a `NoiseTcpReadHalf` and
//! `NoiseTcpWriteHalf`, which support frame-based encoding/decoding of SV2 messages with optional
//! non-blocking behavior.

use std::time::Duration;

use crate::network_helpers::Error;
use stratum_core::{
    codec_sv2::{
        EncodableFrame, Handshake, NoiseEncoder, StandardNoiseDecoder, Transport,
        TransportDecryptState, TransportEncryptState,
        state::{ExpectsHandshakeMessage, InitiatorSent},
    },
    noise_sv2::{INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE, Initiator, Responder},
};
use tokio::net::{
    TcpStream,
    tcp::{OwnedReadHalf, OwnedWriteHalf},
};

use stratum_core::{
    codec_sv2::StandardSerializedFrame, framing_sv2::framing::HandshakeFrame,
    noise_sv2::ELLSWIFT_ENCODING_SIZE,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

/// A Noise-secured duplex stream over TCP that wraps a `TcpStream`
/// and provides secure read/write capabilities using the Noise protocol.
///
/// This stream performs the full Noise handshake during construction
/// and returns a bidirectional encrypted stream split into read and write halves.
///
/// **Note:** This struct is **not cancellation-safe**.
/// If `read_frame()` or `write_frame()` is canceled mid-way,
/// internal state may be left in an inconsistent state, which can lead to
/// protocol errors or dropped frames.
pub struct NoiseTcpStream {
    reader: NoiseTcpReadHalf,
    writer: NoiseTcpWriteHalf,
}

/// The reading half of a `NoiseTcpStream`.
///
/// It buffers incoming encrypted bytes, attempts to decode full Noise frames,
/// and exposes a method to retrieve structured messages of type `Message`.
pub struct NoiseTcpReadHalf {
    reader: OwnedReadHalf,
    decoder: StandardNoiseDecoder,
    state: TransportDecryptState,
    current_frame_buf: Vec<u8>,
    bytes_read: usize,
}

/// The writing half of a `NoiseTcpStream`.
///
/// It accepts structured messages, encodes them via the Noise protocol,
/// and writes the result to the socket.
pub struct NoiseTcpWriteHalf {
    writer: OwnedWriteHalf,
    encoder: NoiseEncoder,
    state: TransportEncryptState,
}

impl NoiseTcpStream {
    /// Connects as the Noise initiator over the given TCP stream, performing the handshake.
    ///
    /// On success, returns a stream with encrypted communication channels.
    ///
    /// `timeout` applies to each individual handshake read. Prefer [`super::connect_with_noise`]
    /// for typical use, which applies a sensible default timeout automatically.
    pub async fn connect(
        stream: TcpStream,
        initiator: Box<Initiator>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let (mut reader, mut writer) = stream.into_split();
        let mut decoder = StandardNoiseDecoder::new();
        let mut encoder = NoiseEncoder::new();
        let handshake = Handshake::new(initiator);

        let (first_msg, handshake) = handshake.step_0()?;
        send_handshake_frame(&mut writer, first_msg, &mut encoder).await?;
        debug!("First handshake message sent");

        let second_msg =
            receive_handshake_frame::<InitiatorSent>(&mut reader, &mut decoder, timeout).await?;
        debug!("Second handshake message received");
        let payload: [u8; INITIATOR_EXPECTED_HANDSHAKE_MESSAGE_SIZE] = second_msg
            .payload()
            .try_into()
            .map_err(|_| Error::HandshakeRemoteInvalidMessage)?;

        Ok(Self::from_transport(
            reader,
            writer,
            decoder,
            encoder,
            handshake.step_2(payload)?,
        ))
    }

    /// Accepts a connection as the Noise responder over the given TCP stream, performing the
    /// handshake.
    ///
    /// On success, returns a stream with encrypted communication channels.
    ///
    /// `timeout` applies to each individual handshake read. Prefer
    /// [`super::accept_noise_connection`] for typical use, which applies a sensible default
    /// timeout automatically.
    pub async fn accept(
        stream: TcpStream,
        responder: Box<Responder>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        let (mut reader, mut writer) = stream.into_split();
        let mut decoder = StandardNoiseDecoder::new();
        let mut encoder = NoiseEncoder::new();
        let handshake = Handshake::new(responder);

        let first_msg =
            receive_handshake_frame::<Responder>(&mut reader, &mut decoder, timeout).await?;
        debug!("First handshake message received");
        let payload: [u8; ELLSWIFT_ENCODING_SIZE] = first_msg
            .payload()
            .try_into()
            .map_err(|_| Error::HandshakeRemoteInvalidMessage)?;

        let (second_msg, transport) = handshake.step_1(payload)?;
        send_handshake_frame(&mut writer, second_msg, &mut encoder).await?;
        debug!("Second handshake message sent");

        Ok(Self::from_transport(
            reader, writer, decoder, encoder, transport,
        ))
    }

    // Give each half only the direction it uses: the writer encrypts, the reader decrypts.
    fn from_transport(
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
        decoder: StandardNoiseDecoder,
        encoder: NoiseEncoder,
        transport: Transport,
    ) -> Self {
        let (encrypt_state, decrypt_state) = transport.split();

        Self {
            reader: NoiseTcpReadHalf {
                reader,
                decoder,
                state: decrypt_state,
                current_frame_buf: vec![],
                bytes_read: 0,
            },
            writer: NoiseTcpWriteHalf {
                writer,
                encoder,
                state: encrypt_state,
            },
        }
    }

    /// Consumes the stream and returns its reader and writer halves.
    pub fn into_split(self) -> (NoiseTcpReadHalf, NoiseTcpWriteHalf) {
        (self.reader, self.writer)
    }
}

impl NoiseTcpWriteHalf {
    /// Encrypts and writes a full message frame to the socket.
    ///
    /// Returns an error if the socket is closed or the message cannot be encoded.
    ///
    /// Not cancellation-safe: A canceled write may cause partial writes or state corruption.
    pub async fn write_frame<F: EncodableFrame>(&mut self, frame: F) -> Result<(), Error> {
        let buf = self.encoder.encode_transport(frame, &mut self.state)?;
        self.writer
            .write_all(buf.as_ref())
            .await
            .map_err(|_| Error::SocketClosed)?;
        Ok(())
    }

    /// Attempts to write a message without blocking.
    ///
    /// Returns:
    /// - `Ok(true)` if the entire frame was written successfully.
    /// - `Ok(false)` if the socket is not ready (would block).
    /// - `Err(_)` on socket or encoding errors.
    pub fn try_write_frame<F: EncodableFrame>(&mut self, frame: F) -> Result<bool, Error> {
        let buf = self.encoder.encode_transport(frame, &mut self.state)?;

        match self.writer.try_write(buf.as_ref()) {
            Ok(n) if n == buf.len() => Ok(true),
            Ok(_) => Err(Error::SocketClosed),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
            Err(_) => Err(Error::SocketClosed),
        }
    }

    /// Gracefully shuts down the writing half of the stream.
    ///
    /// Returns an error if the shutdown fails.
    pub async fn shutdown(&mut self) -> Result<(), Error> {
        self.writer
            .shutdown()
            .await
            .map_err(|_| Error::SocketClosed)
    }
}

impl NoiseTcpReadHalf {
    /// Reads and decodes a complete frame from the socket.
    ///
    /// This method blocks until a full frame is read and decoded,
    /// handling `MissingBytes` errors from the codec automatically.
    ///
    /// Not cancellation-safe: Cancellation may leave partially-read state behind.
    pub async fn read_frame(&mut self) -> Result<StandardSerializedFrame, Error> {
        loop {
            let expected = self.decoder.writable_len();

            if self.current_frame_buf.len() != expected {
                self.current_frame_buf.resize(expected, 0);
                self.bytes_read = 0;
            }

            while self.bytes_read < expected {
                let n = self
                    .reader
                    .read(&mut self.current_frame_buf[self.bytes_read..])
                    .await
                    .map_err(|_| Error::SocketClosed)?;

                if n == 0 {
                    return Err(Error::SocketClosed);
                }

                self.bytes_read += n;
            }

            self.decoder
                .writable()
                .copy_from_slice(&self.current_frame_buf[..]);

            self.bytes_read = 0;

            match self.decoder.next_transport_frame(&mut self.state) {
                Ok(frame) => return Ok(frame),
                Err(stratum_core::codec_sv2::Error::MissingBytes(_)) => {
                    tokio::task::yield_now().await;
                    continue;
                }
                Err(e) => return Err(Error::CodecError(e)),
            }
        }
    }

    /// Attempts to read and decode a frame without blocking.
    ///
    /// Returns:
    /// - `Ok(Some(frame))` if a full frame is successfully decoded.
    /// - `Ok(None)` if not enough data is available yet.
    /// - `Err(_)` on socket or decoding errors.
    pub fn try_read_frame(&mut self) -> Result<Option<StandardSerializedFrame>, Error> {
        let expected = self.decoder.writable_len();

        if self.current_frame_buf.len() != expected {
            self.current_frame_buf.resize(expected, 0);
            self.bytes_read = 0;
        }

        match self
            .reader
            .try_read(&mut self.current_frame_buf[self.bytes_read..])
        {
            Ok(0) => return Err(Error::SocketClosed),
            Ok(n) => self.bytes_read += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(_) => return Err(Error::SocketClosed),
        }

        if self.bytes_read < expected {
            return Ok(None);
        }

        self.decoder
            .writable()
            .copy_from_slice(&self.current_frame_buf[..]);

        self.bytes_read = 0;

        match self.decoder.next_transport_frame(&mut self.state) {
            Ok(frame) => Ok(Some(frame)),
            Err(stratum_core::codec_sv2::Error::MissingBytes(_)) => Ok(None),
            Err(e) => Err(Error::CodecError(e)),
        }
    }
}

async fn send_handshake_frame(
    writer: &mut OwnedWriteHalf,
    frame: HandshakeFrame,
    encoder: &mut NoiseEncoder,
) -> Result<(), Error> {
    let buffer = encoder.encode_handshake(frame);
    writer
        .write_all(buffer.as_ref())
        .await
        .map_err(|_| Error::SocketClosed)?;
    Ok(())
}

async fn receive_handshake_frame<R: ExpectsHandshakeMessage>(
    reader: &mut OwnedReadHalf,
    decoder: &mut StandardNoiseDecoder,
    timeout: Duration,
) -> Result<HandshakeFrame, Error> {
    loop {
        let mut buffer = vec![0u8; decoder.writable_len()];
        tokio::time::timeout(timeout, reader.read_exact(&mut buffer))
            .await
            .map_err(|_| Error::HandshakeTimeout)?
            .map_err(|_| Error::SocketClosed)?;
        decoder.writable().copy_from_slice(&buffer);

        match decoder.next_handshake_frame::<R>() {
            Ok(frame) => return Ok(frame),
            Err(stratum_core::codec_sv2::Error::MissingBytes(_)) => {
                debug!("Waiting for more bytes during handshake");
            }
            Err(e) => return Err(Error::CodecError(e)),
        }
    }
}
