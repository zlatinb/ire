//! Transports used for point-to-point communication between I2P routers.

use futures::{sync::mpsc, Async, Future, Poll, Sink, StartSend, Stream};
use num::bigint::{BigUint, RandBigInt};
use rand;
use std::io;
use std::iter::{once, repeat};
use std::net::SocketAddr;
use tokio_io::IoFuture;

use constants::CryptoConstants;
use crypto::math::rectify;
use crypto::SessionKey;
use data::{Hash, RouterAddress, RouterSecretKeys};
use i2np::Message;
use router::types::CommSystem;

pub mod ntcp;
pub mod ntcp2;
mod session;
mod util;

/// Shorthand for the transmit half of a Transport-bound message channel.
type MessageTx = mpsc::UnboundedSender<(Hash, Message)>;

/// Shorthand for the receive half of a Transport-bound message channel.
type MessageRx = mpsc::UnboundedReceiver<(Hash, Message)>;

/// Shorthand for the transmit half of a Transport-bound timestamp channel.
type TimestampTx = mpsc::UnboundedSender<(Hash, u32)>;

/// Shorthand for the receive half of a Transport-bound timestamp channel.
type TimestampRx = mpsc::UnboundedReceiver<(Hash, u32)>;

/// A reference to a transport, that can be used to send messages and
/// timestamps to other routers (if they are reachable via this transport).
#[derive(Clone)]
pub struct Handle {
    message: MessageTx,
    timestamp: TimestampTx,
}

impl Handle {
    pub fn send(&self, hash: Hash, msg: Message) -> io::Result<()> {
        self.message
            .unbounded_send((hash, msg))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    pub fn timestamp(&self, hash: Hash, ts: u32) -> io::Result<()> {
        self.timestamp
            .unbounded_send((hash, ts))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }
}

/// A bid from a transport indicating how much it thinks it will "cost" to
/// send a particular message.
struct Bid {
    bid: u32,
    handle: Handle,
}

impl Sink for Bid {
    type SinkItem = (Hash, Message);
    type SinkError = ();

    fn start_send(
        &mut self,
        message: Self::SinkItem,
    ) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.handle.message.start_send(message).map_err(|_| ())
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.handle.message.poll_complete().map_err(|_| ())
    }
}

/// Coordinates the sending and receiving of frames over the various supported
/// transports.
pub struct Manager {
    ntcp: ntcp::Manager,
    ntcp2: ntcp2::Manager,
    engine: Option<Engine>,
}

pub struct Engine {
    ntcp: ntcp::Engine,
    ntcp2: ntcp2::Engine,
    select_flag: bool,
}

trait Transport {
    fn bid(&self, hash: &Hash, msg_size: usize) -> Option<Bid>;
}

impl Manager {
    pub fn new(ntcp_addr: SocketAddr, ntcp2_addr: SocketAddr, ntcp2_keyfile: &str) -> Self {
        let (ntcp_manager, ntcp_engine) = ntcp::Manager::new(ntcp_addr);
        let (ntcp2_manager, ntcp2_engine) =
            match ntcp2::Manager::from_file(ntcp2_addr, ntcp2_keyfile) {
                Ok(ret) => ret,
                Err(_) => {
                    let (ntcp2_manager, ntcp2_engine) = ntcp2::Manager::new(ntcp2_addr);
                    ntcp2_manager.to_file(ntcp2_keyfile).unwrap();
                    (ntcp2_manager, ntcp2_engine)
                }
            };
        Manager {
            ntcp: ntcp_manager,
            ntcp2: ntcp2_manager,
            engine: Some(Engine {
                ntcp: ntcp_engine,
                ntcp2: ntcp2_engine,
                select_flag: false,
            }),
        }
    }
}

impl CommSystem for Manager {
    fn addresses(&self) -> Vec<RouterAddress> {
        vec![self.ntcp.address(), self.ntcp2.address()]
    }

    fn start(&mut self, rsk: RouterSecretKeys) -> IoFuture<()> {
        let engine = self.engine.take().expect("Cannot call listen() twice");

        let listener = self
            .ntcp
            .listen(rsk.rid.clone(), rsk.signing_private_key.clone())
            .map_err(|e| {
                error!("NTCP listener error: {}", e);
                e
            });

        let listener2 = self.ntcp2.listen(rsk.rid).map_err(|e| {
            error!("NTCP2 listener error: {}", e);
            e
        });

        Box::new(
            engine
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "Error in transport::Engine"))
                .join3(listener, listener2)
                .map(|_| ()),
        )
    }

    /// Send an I2NP message to a peer over one of our transports.
    ///
    /// Returns an Err giving back the message if it cannot be sent over any of
    /// our transports.
    fn send(&self, hash: Hash, msg: Message) -> Result<IoFuture<()>, (Hash, Message)> {
        match once(self.ntcp.bid(&hash, msg.size()))
            .chain(once(self.ntcp2.bid(&hash, msg.ntcp2_size())))
            .filter_map(|b| b)
            .min_by_key(|b| b.bid)
        {
            Some(bid) => Ok(Box::new(bid.send((hash, msg)).map(|_| ()).map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "Error in transport::Engine")
            }))),
            None => Err((hash, msg)),
        }
    }
}

impl Future for Engine {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        let mut select = util::Select {
            stream1: &mut self.ntcp,
            stream2: &mut self.ntcp2,
            flag: &mut self.select_flag,
        };
        while let Async::Ready(f) = select.poll()? {
            if let Some((from, msg)) = f {
                // TODO: Do something
                debug!("Received message from {}: {:?}", from, msg);
            }
        }
        Ok(Async::NotReady)
    }
}

pub struct DHSessionKeyBuilder {
    dh_priv: BigUint,
    dh_pub: BigUint,
}

impl DHSessionKeyBuilder {
    pub fn new() -> Self {
        let mut rng = rand::thread_rng();
        let dh_priv = rng.gen_biguint(2048);
        let cc = CryptoConstants::new();
        let dh_pub = cc.elg_g.modpow(&dh_priv, &cc.elg_p);
        DHSessionKeyBuilder { dh_priv, dh_pub }
    }

    pub fn get_pub(&self) -> Vec<u8> {
        rectify(&self.dh_pub, 256)
    }

    pub fn build_session_key(&self, peer_pub: &[u8; 256]) -> SessionKey {
        // Calculate the exchanged DH key
        let peer_pub = BigUint::from_bytes_be(peer_pub);
        let cc = CryptoConstants::new();
        let dh_key = peer_pub.modpow(&self.dh_priv, &cc.elg_p);
        // Represent the exchanged key as a positive minimal-length two's-complement
        // big-endian byte array. If most significant bit is 1, prepend a zero-byte
        // (to match Java's BigInteger.toByteArray() representation).
        let mut buf = dh_key.to_bytes_be();
        if buf[0] & 0x80 != 0 {
            buf.insert(0, 0x00);
        }
        // If that byte array is less than 32 bytes, append 0x00 bytes to extend to
        // 32 bytes. This is vanishingly unlikely, but have to do it for compatibility.
        let length = buf.len();
        if length < 32 {
            buf.extend(repeat(0).take(32 - length));
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&buf[0..32]);
        SessionKey(key)
    }
}

#[cfg(test)]
mod tests {
    use futures::{lazy, Async, Stream};
    use num::Num;
    use std::io::{self, Read, Write};
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;
    use tokio_io::{AsyncRead, AsyncWrite};

    use super::*;

    pub struct NetworkCable {
        alice_to_bob: Vec<u8>,
        bob_to_alice: Vec<u8>,
    }

    impl NetworkCable {
        pub fn new() -> Arc<Mutex<Self>> {
            Arc::new(Mutex::new(NetworkCable {
                alice_to_bob: Vec::new(),
                bob_to_alice: Vec::new(),
            }))
        }
    }

    pub struct AliceNet {
        cable: Arc<Mutex<NetworkCable>>,
    }

    impl AliceNet {
        pub fn new(cable: Arc<Mutex<NetworkCable>>) -> Self {
            AliceNet { cable }
        }
    }

    impl Read for AliceNet {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut cable = self.cable.lock().unwrap();
            let n_in = cable.bob_to_alice.len();
            let n_out = buf.len();
            if n_in == 0 {
                Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
            } else if n_out < n_in {
                buf.copy_from_slice(&cable.bob_to_alice[..n_out]);
                cable.bob_to_alice = cable.bob_to_alice.split_off(n_out);
                Ok(n_out)
            } else {
                (&mut buf[..n_in]).copy_from_slice(&cable.bob_to_alice);
                cable.bob_to_alice.clear();
                Ok(n_in)
            }
        }
    }

    impl Write for AliceNet {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut cable = self.cable.lock().unwrap();
            cable.alice_to_bob.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl AsyncRead for AliceNet {}
    impl AsyncWrite for AliceNet {
        fn shutdown(&mut self) -> io::Result<Async<()>> {
            Ok(().into())
        }
    }

    pub struct BobNet {
        cable: Arc<Mutex<NetworkCable>>,
    }

    impl BobNet {
        pub fn new(cable: Arc<Mutex<NetworkCable>>) -> Self {
            BobNet { cable }
        }
    }

    impl Read for BobNet {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut cable = self.cable.lock().unwrap();
            let n_in = cable.alice_to_bob.len();
            let n_out = buf.len();
            if n_in == 0 {
                Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
            } else if n_out < n_in {
                buf.copy_from_slice(&cable.alice_to_bob[..n_out]);
                cable.alice_to_bob = cable.alice_to_bob.split_off(n_out);
                Ok(n_out)
            } else {
                (&mut buf[..n_in]).copy_from_slice(&cable.alice_to_bob);
                cable.alice_to_bob.clear();
                Ok(n_in)
            }
        }
    }

    impl Write for BobNet {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut cable = self.cable.lock().unwrap();
            cable.bob_to_alice.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl AsyncRead for BobNet {}
    impl AsyncWrite for BobNet {
        fn shutdown(&mut self) -> io::Result<Async<()>> {
            Ok(().into())
        }
    }

    #[test]
    fn handle_send() {
        let (message, mut message_rx) = mpsc::unbounded();
        let (timestamp, mut timestamp_rx) = mpsc::unbounded();
        let handle = Handle { message, timestamp };

        let hash = Hash::from_bytes(&[0; 32]);
        let msg = Message::dummy_data();
        let mut msg2 = Message::dummy_data();
        // Ensure the two messages are identical
        msg2.expiration = msg.expiration.clone();

        // Run on a task context
        lazy(move || {
            // Check the queue is empty
            assert_eq!(
                (message_rx.poll(), timestamp_rx.poll()),
                (Ok(Async::NotReady), Ok(Async::NotReady))
            );

            // Send a message
            handle.send(hash.clone(), msg).unwrap();

            // Check it was received
            assert_eq!(
                (message_rx.poll(), timestamp_rx.poll()),
                (Ok(Async::Ready(Some((hash, msg2)))), Ok(Async::NotReady))
            );

            // Check the queue is empty again
            assert_eq!(
                (message_rx.poll(), timestamp_rx.poll()),
                (Ok(Async::NotReady), Ok(Async::NotReady))
            );

            Ok::<(), ()>(())
        }).wait()
        .unwrap();
    }

    #[test]
    fn handle_timestamp() {
        let (message, mut message_rx) = mpsc::unbounded();
        let (timestamp, mut timestamp_rx) = mpsc::unbounded();
        let handle = Handle { message, timestamp };

        // Run on a task context
        lazy(move || {
            // Check the queue is empty
            assert_eq!(
                (message_rx.poll(), timestamp_rx.poll()),
                (Ok(Async::NotReady), Ok(Async::NotReady))
            );

            // Send a message
            let hash = Hash::from_bytes(&[0; 32]);
            handle.timestamp(hash.clone(), 42).unwrap();

            // Check it was received
            assert_eq!(
                (message_rx.poll(), timestamp_rx.poll()),
                (Ok(Async::NotReady), Ok(Async::Ready(Some((hash, 42)))))
            );

            // Check the queue is empty again
            assert_eq!(
                (message_rx.poll(), timestamp_rx.poll()),
                (Ok(Async::NotReady), Ok(Async::NotReady))
            );

            Ok::<(), ()>(())
        }).wait()
        .unwrap();
    }

    #[test]
    fn manager_addresses() {
        let dir = tempdir().unwrap();

        let ntcp_addr = "127.0.0.1:0".parse().unwrap();
        let ntcp2_addr = "127.0.0.2:0".parse().unwrap();
        let ntcp2_keyfile = dir.path().join("test.ntcp2.keys.dat");

        let manager = Manager::new(ntcp_addr, ntcp2_addr, ntcp2_keyfile.to_str().unwrap());
        let addrs = manager.addresses();

        assert_eq!(addrs[0].addr(), Some(ntcp_addr));
        assert_eq!(addrs[1].addr(), Some(ntcp2_addr));
    }

    #[test]
    fn build_session_key() {
        struct TestVector<'a> {
            dh_priv: &'a str,
            dh_pub: [u8; 256],
            peer_pub: [u8; 256],
            session_key: SessionKey,
        }
        // Generated via Java's DHSessionKeyBuilder
        let test_vectors = vec![
            TestVector {
                dh_priv: "D100BF92_2E965504_BC6E2E6D_8AB968D9_883890B7_65EF673C\
                          54479E8B_570A8157_80092340_A8E6178F_C7FB9732_3016A6C4\
                          4F953792_5C32C30C_7BC9117F_7078A214_96853C44_9D7AAC7B\
                          124CCB9A_AB14CF3B_67D2EDA3_E6251B6D_6A48AD72_FBD466B2\
                          B331C9A2_6F269DF4_8E7DC944_17546F7A_B20E39B8_57CC7A0A\
                          C572C30E_BFD06EA6_2E63E7D5_C921EAB6_A9B8FC02_F31B4103\
                          5CF8850A_6DE31D07_53785A9F_20A7DE4D_8E2CCFB9_79C62E0B\
                          05624443_7E7149CB_0B6D65BD_F7B4ADE9_1045D432_1F173603\
                          C314FA4F_541F6E0A_DDF73002_0D7F19A3_0C61BB4D_51483239\
                          16F52D21_CC765916_6E7141B5_61877053_ECA28EAF_0D221D22\
                          907D6E78_539797EF_1D29E4C9_B61169E0",
                dh_pub: [
                    0xeb, 0x84, 0x78, 0x1e, 0xe2, 0x2c, 0x07, 0xbe, 0xde, 0x67, 0xce, 0x83, 0x89,
                    0xeb, 0x34, 0x01, 0x92, 0xaf, 0x25, 0x95, 0x2e, 0x6c, 0x35, 0x35, 0x21, 0x7f,
                    0xc7, 0x60, 0xd9, 0x59, 0x0d, 0x11, 0x17, 0x70, 0xbd, 0xb8, 0x35, 0x79, 0x03,
                    0x4a, 0x65, 0x5b, 0xb8, 0xf2, 0x03, 0xd6, 0x90, 0x41, 0xf7, 0x20, 0x7c, 0x57,
                    0xe2, 0xa5, 0x46, 0xb0, 0xc3, 0xfd, 0x75, 0x5e, 0x4e, 0xf9, 0x7f, 0x6e, 0x76,
                    0xf1, 0x07, 0xa6, 0xd6, 0xcd, 0x6c, 0xa9, 0x42, 0xc5, 0xc4, 0x09, 0xd0, 0xce,
                    0x55, 0x3c, 0x53, 0xa0, 0xd8, 0xc0, 0xc9, 0x66, 0x9f, 0xce, 0xe3, 0xd8, 0xb8,
                    0xe8, 0x92, 0x33, 0x62, 0x72, 0x85, 0xd9, 0x6c, 0x07, 0x11, 0x52, 0x6d, 0x8a,
                    0x80, 0x92, 0xe1, 0x37, 0xe2, 0x43, 0x01, 0x52, 0xc9, 0x94, 0xac, 0x70, 0xf1,
                    0x74, 0x46, 0xde, 0x1f, 0x22, 0x77, 0x56, 0x5e, 0x8c, 0xf0, 0x4e, 0xb8, 0xcb,
                    0xf2, 0x44, 0x16, 0xc8, 0x3c, 0x50, 0x9e, 0x25, 0xb6, 0x61, 0x2f, 0x4f, 0x16,
                    0x89, 0xe5, 0xd0, 0x9f, 0x0b, 0x29, 0x06, 0x01, 0x0c, 0x24, 0x37, 0x99, 0x5d,
                    0xd4, 0xf8, 0x7b, 0x4f, 0x92, 0xf3, 0x99, 0x8d, 0xa4, 0x76, 0xb3, 0x9b, 0xdf,
                    0xbb, 0x34, 0x7f, 0x5b, 0x7f, 0x3e, 0x72, 0x4c, 0xc1, 0x20, 0x8b, 0x85, 0x70,
                    0xbf, 0xce, 0x0d, 0xe7, 0x3f, 0x40, 0x51, 0x3d, 0xc2, 0x80, 0xcb, 0x36, 0x25,
                    0x52, 0x54, 0x74, 0xbb, 0x42, 0x1f, 0x3f, 0xd6, 0x50, 0x60, 0x3c, 0x2e, 0x9f,
                    0x83, 0xd0, 0x9d, 0x00, 0x82, 0x61, 0x40, 0x92, 0xd9, 0x9b, 0x5e, 0x1f, 0xa2,
                    0xa0, 0xff, 0x83, 0x99, 0x38, 0x2f, 0xf1, 0xee, 0xe3, 0x9e, 0x6a, 0x99, 0x41,
                    0xee, 0x9f, 0x20, 0xd1, 0xda, 0x2f, 0x7f, 0xdf, 0xc3, 0x88, 0x62, 0x49, 0x26,
                    0xb2, 0x59, 0xf3, 0x7e, 0x30, 0x3e, 0x76, 0x7f, 0x83,
                ],
                peer_pub: [
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x1c, 0x7d, 0x69, 0x68,
                    0x50, 0x54, 0x3a, 0x3d, 0x30, 0x56, 0xa8, 0xa4, 0xf9, 0x32, 0x18, 0x1f, 0xf3,
                    0x79, 0xb6, 0x98, 0x73, 0xb2, 0x7b, 0x81, 0x4c, 0x58, 0x19, 0x11, 0xdb, 0x36,
                    0x9a, 0xd5, 0xb3, 0xa0, 0x9d, 0x2e, 0x10, 0xa1, 0xb6, 0xec, 0xa6, 0x9d, 0x7a,
                    0x8d, 0x3c, 0xed, 0x10, 0x22, 0x93, 0xd9, 0x0f, 0xff, 0x4d, 0x65, 0xfb, 0x39,
                    0xde, 0x97, 0xaa, 0x41, 0x56, 0x2a, 0x3a, 0x02, 0x28, 0x5a, 0x28, 0x95, 0xbb,
                    0xa3, 0x9b, 0x29, 0x47, 0x9b, 0x10, 0xe3, 0xba, 0xc8, 0x9e, 0x67, 0xbb, 0x3c,
                    0x29, 0x7e, 0x8a, 0x17, 0x50, 0x28, 0x44, 0x67, 0xc3, 0x2d, 0xa7, 0x5c, 0x4d,
                    0x1d, 0x75, 0xa3, 0xab, 0x69, 0x8c, 0x0a, 0xed, 0x38, 0x59, 0xb6, 0xa6, 0xa5,
                    0xe5, 0x85, 0x02, 0x48, 0x07, 0x88, 0xdc, 0xc3, 0x8a, 0x45, 0x08, 0x5b, 0x1a,
                    0x5d, 0x22, 0x35, 0x21, 0xb7, 0x92, 0xc6, 0x3e, 0x49, 0x34, 0x8c, 0xa6, 0x79,
                    0xf2, 0x82, 0x40, 0x38, 0x5a, 0x08, 0x5e, 0x93, 0x91, 0x9e, 0x8b, 0x60, 0xb7,
                    0x65, 0x3a, 0x00, 0xa2, 0x59, 0x24, 0x5e, 0x96, 0xcb, 0x21, 0x2e, 0x47, 0xbb,
                    0x47, 0x2e, 0x05, 0x16, 0xc1, 0x51, 0x3d, 0xb2, 0x7e, 0xe5, 0x70, 0xdb, 0xf4,
                    0x9a, 0x4d, 0x2b, 0xbf, 0x26, 0xf8, 0x43, 0x3e, 0x5f, 0x98, 0x80, 0xc8, 0xa1,
                    0x83, 0x6f, 0x3f, 0x1f, 0xfb, 0x0b, 0x6c, 0xd8, 0xa8, 0xac, 0x77, 0x81, 0x57,
                    0x4f, 0x27, 0xdc, 0x96, 0xab, 0xc7, 0x1f, 0x01, 0x45, 0x70, 0x4d, 0xac, 0xd0,
                    0xbc, 0x52, 0x00, 0x93, 0xfe, 0x16, 0xae, 0x5a, 0xc5, 0x2a, 0x64, 0x02, 0x49,
                    0x31, 0xd4, 0x88, 0xd8, 0x5c, 0x74, 0x07, 0xd8, 0xef, 0x86, 0x1a, 0x22, 0xcb,
                    0x20, 0xa2, 0x7c, 0xe3, 0x7c, 0x20, 0xd9, 0x3f, 0x17,
                ],
                session_key: SessionKey([
                    0x11, 0x28, 0x94, 0x3b, 0xcb, 0x88, 0x3b, 0x5e, 0x1d, 0xf5, 0xda, 0xb0, 0xce,
                    0xf9, 0xa6, 0x1d, 0x82, 0xa4, 0xed, 0x69, 0x10, 0x35, 0xca, 0xf9, 0xe9, 0x59,
                    0x2f, 0x33, 0x32, 0xfc, 0x0d, 0xd9,
                ]),
            },
            TestVector {
                dh_priv: "67E14314_D5FBC506_99A9E30F_E59AC5DE_7A4EF8A3_30DE7F28\
                          82792FE1_CD5F693E_B49C0225_940A61EE_768F1544_FEDC125F\
                          52F31FD2_87F4CF68_5A848CCF_C6BB23FC_8CF7D52C_D47A271E\
                          A920A56E_C6B46B64_5CDED831_DAFE34DC_852D8215_7A3D093C\
                          B5ABE447_E4EEEF99_4A811A96_D4331765_8A78A683_32013CDA\
                          BFFB5BA4_060D103B_C64514FF_B47BDE97_2BB72A50_7B39854A\
                          FA4F8B58_125DCF3E_9C39F14B_D9B67FA9_B8B12896_86CC56BB\
                          9B1C8B64_9AFD2BEB_F64AEA9D_4D73B968_68871F9A_694E416C\
                          2F5E0217_0EB97175_8AEA1B9E_93EED7C3_5B8E16F6_054D1244\
                          68DA3E0B_5B80FA66_9F0A7041_7FD2B29D_B20AED55_18DE8F33\
                          B064D4A2_A28FE378_3D94BC77_3F6FB6BB",
                dh_pub: [
                    0x59, 0x00, 0x5c, 0x7f, 0x22, 0x4b, 0x41, 0x8f, 0xb8, 0x91, 0xe7, 0xad, 0x31,
                    0x56, 0xc0, 0x1f, 0xbc, 0x5e, 0x2a, 0xb0, 0x3a, 0xf1, 0x56, 0x3a, 0x7b, 0x28,
                    0x17, 0x92, 0x4d, 0x50, 0xdf, 0xc1, 0xd8, 0x38, 0x84, 0x24, 0xe5, 0x82, 0x96,
                    0x1a, 0xb3, 0x60, 0xcd, 0xf5, 0xec, 0xca, 0x1a, 0xcf, 0x66, 0x98, 0x31, 0xd3,
                    0x46, 0x4e, 0x58, 0x3f, 0xd2, 0xbd, 0x98, 0x8f, 0x6b, 0x07, 0x20, 0x36, 0xc7,
                    0xce, 0xc6, 0x4f, 0x7b, 0xcc, 0x77, 0xe2, 0x06, 0x95, 0x2c, 0x84, 0xf6, 0x65,
                    0x0f, 0x0d, 0x01, 0xc9, 0x66, 0xab, 0xe4, 0x7c, 0x08, 0xa3, 0x9c, 0xbe, 0x82,
                    0x28, 0x2b, 0xc8, 0x7d, 0x89, 0x2a, 0xba, 0x98, 0x0e, 0x4c, 0x28, 0xe5, 0x0f,
                    0x81, 0x32, 0x13, 0xb9, 0x31, 0x4f, 0x05, 0x90, 0x7b, 0x8b, 0x23, 0xc8, 0xf1,
                    0x2a, 0x2c, 0xc4, 0x93, 0xcf, 0xbd, 0xe2, 0x1e, 0x91, 0x9f, 0xb2, 0x84, 0x8a,
                    0xb2, 0xe7, 0x4f, 0x24, 0x11, 0x40, 0x19, 0x84, 0x7f, 0x15, 0xda, 0xf6, 0x8e,
                    0xda, 0x4c, 0x86, 0x13, 0x60, 0x78, 0xdf, 0xb7, 0xe4, 0x46, 0x17, 0x88, 0xf7,
                    0x04, 0x49, 0xf3, 0xf2, 0x9a, 0x0b, 0xd5, 0x84, 0x7b, 0xca, 0xab, 0x5d, 0x07,
                    0x5a, 0x88, 0x3a, 0xee, 0xc1, 0xb4, 0xcb, 0xbc, 0x55, 0x6f, 0x85, 0xc4, 0x0f,
                    0xa7, 0xaa, 0x4e, 0xe3, 0x29, 0xb1, 0x10, 0x0e, 0x00, 0xd6, 0x15, 0x05, 0x0b,
                    0x44, 0x84, 0x56, 0x29, 0x3c, 0x43, 0xaf, 0x36, 0x49, 0x1c, 0xbd, 0xd9, 0x78,
                    0x0d, 0x9f, 0x68, 0xb8, 0x62, 0x90, 0xb7, 0xb9, 0x81, 0x17, 0xfe, 0x59, 0x71,
                    0x88, 0x17, 0x0b, 0x41, 0x08, 0xe4, 0x4d, 0xfa, 0x97, 0xf0, 0x5f, 0x97, 0x01,
                    0x03, 0xa5, 0x2a, 0x0d, 0xc3, 0x0c, 0x8e, 0xe4, 0xa7, 0xb6, 0xab, 0xab, 0xe6,
                    0x49, 0x06, 0x38, 0x4e, 0xec, 0x3e, 0xf8, 0x2f, 0xfd,
                ],
                peer_pub: [
                    0xd1, 0x2f, 0x7d, 0x48, 0xea, 0x85, 0xd3, 0x6c, 0x32, 0x85, 0x76, 0xf9, 0xf3,
                    0x68, 0x21, 0x11, 0x17, 0x37, 0x3b, 0x19, 0xc4, 0xb1, 0xb2, 0x0c, 0xa4, 0x23,
                    0xa9, 0x9a, 0xfb, 0xa4, 0xa1, 0xe7, 0xc3, 0xb7, 0xad, 0x26, 0xa2, 0xed, 0xc4,
                    0x3d, 0xc8, 0xc3, 0x07, 0xe6, 0x81, 0x36, 0x59, 0x39, 0xd1, 0xe3, 0xf0, 0xd4,
                    0x76, 0xee, 0xfe, 0x1c, 0xb0, 0x31, 0xfe, 0xf7, 0xe8, 0x4f, 0x57, 0xd8, 0x3c,
                    0xa2, 0x84, 0x8c, 0x05, 0xe0, 0x0c, 0x1d, 0x30, 0xb8, 0x55, 0xdc, 0x72, 0x34,
                    0x03, 0x46, 0x23, 0x76, 0x92, 0x6b, 0x3e, 0x7f, 0x23, 0x7d, 0x95, 0x57, 0x68,
                    0x0d, 0xdf, 0x39, 0xe6, 0x43, 0x77, 0x37, 0xb8, 0x0b, 0x69, 0xc3, 0x51, 0xe9,
                    0x90, 0xb2, 0xce, 0x18, 0xd0, 0xcd, 0x21, 0x9b, 0x4f, 0xe0, 0x3c, 0xac, 0x6d,
                    0x91, 0xa7, 0x07, 0x08, 0xeb, 0x16, 0x20, 0x69, 0xb7, 0x57, 0x23, 0x16, 0xba,
                    0xbc, 0x11, 0x22, 0x52, 0xbc, 0x00, 0x5d, 0x62, 0x0a, 0xae, 0xdd, 0xc3, 0xed,
                    0x7a, 0xb4, 0xb1, 0xa3, 0xd1, 0x32, 0xb4, 0x39, 0x1b, 0x6e, 0xc2, 0xc2, 0x97,
                    0xfa, 0x72, 0xb7, 0x27, 0x62, 0x3d, 0xec, 0xa5, 0x90, 0xd6, 0x2b, 0xed, 0x06,
                    0x85, 0x44, 0x35, 0x9b, 0x93, 0xcb, 0xcc, 0xc0, 0x6d, 0x44, 0x47, 0x41, 0x03,
                    0xca, 0x02, 0x27, 0xcf, 0x40, 0xaf, 0x5f, 0xe4, 0x04, 0x9b, 0xd6, 0x80, 0xf4,
                    0x86, 0x1a, 0xf2, 0x8e, 0x1c, 0x2c, 0x22, 0x30, 0x1d, 0xc7, 0xd7, 0x54, 0x64,
                    0xf2, 0x3e, 0x4c, 0xcd, 0x9b, 0x2d, 0x8a, 0x05, 0x4e, 0x2f, 0xc0, 0x14, 0xb9,
                    0xf4, 0x40, 0xe4, 0x90, 0xf9, 0x13, 0x0e, 0xdd, 0xc8, 0x90, 0x96, 0xa9, 0x8d,
                    0x51, 0x9c, 0x52, 0x3d, 0xdd, 0xb9, 0x5c, 0x4c, 0xbc, 0x34, 0x4f, 0x81, 0x4f,
                    0xc2, 0x11, 0x32, 0xed, 0x1d, 0x91, 0xa7, 0x0d, 0x07,
                ],
                session_key: SessionKey([
                    0x00, 0xae, 0x45, 0x63, 0xa5, 0x62, 0xca, 0x68, 0x88, 0x93, 0xf6, 0xa4, 0xf6,
                    0xb9, 0xb9, 0x7d, 0xd1, 0x6b, 0xfe, 0xa2, 0xca, 0x2b, 0x64, 0xa1, 0x08, 0xcf,
                    0x7d, 0xea, 0xe6, 0x23, 0x4f, 0x79,
                ]),
            },
        ];

        for tv in test_vectors.iter() {
            let dh_priv = BigUint::from_str_radix(tv.dh_priv, 16).unwrap();
            let dh_pub = BigUint::from_bytes_be(&tv.dh_pub[..]);
            let builder = DHSessionKeyBuilder { dh_priv, dh_pub };
            assert_eq!(builder.get_pub(), Vec::from(&tv.dh_pub[..]));
            let session_key = builder.build_session_key(&tv.peer_pub);
            assert_eq!(session_key.0, tv.session_key.0);
        }
    }
}
