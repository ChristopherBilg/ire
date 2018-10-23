//! Transports used for point-to-point communication between I2P routers.

use futures::{stream::Select, sync::mpsc, Async, Future, Poll, Sink, StartSend, Stream};
use std::io;
use std::iter::once;
use std::sync::Arc;
use tokio_io::IoFuture;

use crypto::dh::DHSessionKeyBuilder;
use data::{RouterAddress, RouterInfo};
use i2np::Message;
use router::{
    config,
    types::{CommSystem, InboundMessageHandler, OutboundMessageHandler},
    Context,
};

pub mod ntcp;
pub mod ntcp2;
mod session;

/// Shorthand for the transmit half of a Transport-bound message channel.
type MessageTx = mpsc::UnboundedSender<(RouterInfo, Message)>;

/// Shorthand for the receive half of a Transport-bound message channel.
type MessageRx = mpsc::UnboundedReceiver<(RouterInfo, Message)>;

/// Shorthand for the transmit half of a Transport-bound timestamp channel.
type TimestampTx = mpsc::UnboundedSender<(RouterInfo, u32)>;

/// Shorthand for the receive half of a Transport-bound timestamp channel.
type TimestampRx = mpsc::UnboundedReceiver<(RouterInfo, u32)>;

/// A reference to a transport, that can be used to send messages and
/// timestamps to other routers (if they are reachable via this transport).
#[derive(Clone)]
pub struct Handle {
    message: MessageTx,
    timestamp: TimestampTx,
}

impl Handle {
    pub fn send(&self, peer: RouterInfo, msg: Message) -> io::Result<()> {
        self.message
            .unbounded_send((peer, msg))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    pub fn timestamp(&self, peer: RouterInfo, ts: u32) -> io::Result<()> {
        self.timestamp
            .unbounded_send((peer, ts))
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
    type SinkItem = (RouterInfo, Message);
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
    ntcp_engine: Option<ntcp::Engine>,
    ntcp2: ntcp2::Manager,
    ntcp2_engine: Option<ntcp2::Engine>,
}

pub struct Engine {
    engines: Select<ntcp::Engine, ntcp2::Engine>,
    msg_handler: Arc<InboundMessageHandler>,
}

trait Transport {
    fn bid(&self, peer: &RouterInfo, msg_size: usize) -> Option<Bid>;
}

impl Manager {
    pub fn from_config(config: &config::Config) -> Self {
        let ntcp_addr = config
            .get_str(config::NTCP_LISTEN)
            .expect("Must configure an NTCP address")
            .parse()
            .unwrap();
        let ntcp2_addr = config
            .get_str(config::NTCP2_LISTEN)
            .expect("Must configure an NTCP2 address")
            .parse()
            .unwrap();
        let ntcp2_keyfile = config.get_str(config::NTCP2_KEYFILE).unwrap();

        let (ntcp_manager, ntcp_engine) = ntcp::Manager::new(ntcp_addr);
        let (ntcp2_manager, ntcp2_engine) =
            match ntcp2::Manager::from_file(ntcp2_addr, &ntcp2_keyfile) {
                Ok(ret) => ret,
                Err(_) => {
                    let (ntcp2_manager, ntcp2_engine) = ntcp2::Manager::new(ntcp2_addr);
                    ntcp2_manager.to_file(&ntcp2_keyfile).unwrap();
                    (ntcp2_manager, ntcp2_engine)
                }
            };
        Manager {
            ntcp: ntcp_manager,
            ntcp_engine: Some(ntcp_engine),
            ntcp2: ntcp2_manager,
            ntcp2_engine: Some(ntcp2_engine),
        }
    }
}

impl OutboundMessageHandler for Manager {
    /// Send an I2NP message to a peer over one of our transports.
    ///
    /// Returns an Err giving back the message if it cannot be sent over any of
    /// our transports.
    fn send(&self, peer: RouterInfo, msg: Message) -> Result<IoFuture<()>, (RouterInfo, Message)> {
        match once(self.ntcp.bid(&peer, msg.size()))
            .chain(once(self.ntcp2.bid(&peer, msg.ntcp2_size())))
            .filter_map(|b| b)
            .min_by_key(|b| b.bid)
        {
            Some(bid) => Ok(Box::new(bid.send((peer, msg)).map(|_| ()).map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "Error in transport::Engine")
            }))),
            None => Err((peer, msg)),
        }
    }
}

impl CommSystem for Manager {
    fn addresses(&self) -> Vec<RouterAddress> {
        vec![self.ntcp.address(), self.ntcp2.address()]
    }

    fn start(&mut self, ctx: Arc<Context>) -> IoFuture<()> {
        let mut ntcp_engine = self.ntcp_engine.take().expect("Cannot call listen() twice");
        let mut ntcp2_engine = self
            .ntcp2_engine
            .take()
            .expect("Cannot call listen() twice");

        ntcp_engine.set_context(ctx.clone());
        ntcp2_engine.set_context(ctx.clone());

        let listener = self
            .ntcp
            .listen(ctx.keys.rid.clone(), ctx.keys.signing_private_key.clone())
            .map_err(|e| {
                error!("NTCP listener error: {}", e);
                e
            });

        let listener2 = self.ntcp2.listen(&ctx.keys.rid).map_err(|e| {
            error!("NTCP2 listener error: {}", e);
            e
        });

        let engine = Engine {
            engines: ntcp_engine.select(ntcp2_engine),
            msg_handler: ctx.msg_handler.clone(),
        };

        Box::new(
            engine
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "Error in transport::Engine"))
                .join3(listener, listener2)
                .map(|_| ()),
        )
    }
}

impl Future for Engine {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        while let Async::Ready(f) = self.engines.poll()? {
            if let Some((from, msg)) = f {
                self.msg_handler.handle(from, msg);
            }
        }
        Ok(Async::NotReady)
    }
}

#[cfg(test)]
mod tests {
    use futures::{lazy, Async, Stream};
    use std::io::{self, Read, Write};
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;
    use tokio_io::{AsyncRead, AsyncWrite};

    use super::*;
    use data::{Hash, RouterSecretKeys};

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
            let ri = RouterInfo::new(RouterSecretKeys::new().rid);
            handle.send(ri.clone(), msg).unwrap();

            // Check it was received
            assert_eq!(
                (message_rx.poll(), timestamp_rx.poll()),
                (Ok(Async::Ready(Some((ri, msg2)))), Ok(Async::NotReady))
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
            let ri = RouterInfo::new(RouterSecretKeys::new().rid);
            handle.timestamp(ri.clone(), 42).unwrap();

            // Check it was received
            assert_eq!(
                (message_rx.poll(), timestamp_rx.poll()),
                (Ok(Async::NotReady), Ok(Async::Ready(Some((ri, 42)))))
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

        let ntcp_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let ntcp2_addr: SocketAddr = "127.0.0.2:0".parse().unwrap();
        let ntcp2_keyfile = dir.path().join("test.ntcp2.keys.dat");

        let mut config = config::Config::default();
        config
            .set(config::NTCP_LISTEN, ntcp_addr.to_string())
            .unwrap();
        config
            .set(config::NTCP2_LISTEN, ntcp2_addr.to_string())
            .unwrap();
        config
            .set(config::NTCP2_KEYFILE, ntcp2_keyfile.to_str())
            .unwrap();

        let manager = Manager::from_config(&config);
        let addrs = manager.addresses();

        assert_eq!(addrs[0].addr(), Some(ntcp_addr));
        assert_eq!(addrs[1].addr(), Some(ntcp2_addr));
    }
}
