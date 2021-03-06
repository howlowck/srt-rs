use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use failure::{bail, Error};
use futures::prelude::*;
use futures::try_ready;
use futures_timer::Interval;
use log::{info, warn};

use crate::connected::Connected;
use crate::crypto::CryptoManager;
use crate::packet::{
    CipherType, ControlPacket, ControlTypes, HandshakeControlInfo, HandshakeVSInfo, Packet,
    ShakeType, SocketType, SrtControlPacket, SrtHandshake, SrtKeyMessage, SrtShakeFlags,
};
use crate::{ConnectionSettings, SocketID, SrtVersion};

pub struct Connect<T> {
    remote: SocketAddr,
    sock: Option<T>,
    local_socket_id: SocketID,

    state: State,

    send_interval: Interval,
    tsbpd_latency: Duration,

    crypto: Option<CryptoManager>,
}

enum State {
    Starting(Packet),
    First(Packet),
}

impl<T> Connect<T> {
    pub fn new(
        sock: T,
        remote: SocketAddr,
        local_socket_id: SocketID,
        local_addr: IpAddr,
        tsbpd_latency: Duration,
        crypto: Option<(u8, String)>,
    ) -> Connect<T> {
        info!("Connecting to {:?}", remote);

        let init_seq_num = rand::random();

        Connect {
            remote,
            sock: Some(sock),
            local_socket_id,
            send_interval: Interval::new(Duration::from_millis(100)),
            state: State::Starting(Packet::Control(ControlPacket {
                dest_sockid: SocketID(0),
                timestamp: 0, // TODO: this is not zero in the reference implementation
                control_type: ControlTypes::Handshake(HandshakeControlInfo {
                    init_seq_num,
                    max_packet_size: 1500, // TODO: take as a parameter
                    max_flow_size: 8192,   // TODO: take as a parameter
                    socket_id: local_socket_id,
                    shake_type: ShakeType::Induction,
                    peer_addr: local_addr,
                    syn_cookie: 0,
                    info: HandshakeVSInfo::V4(SocketType::Datagram),
                }),
            })),
            tsbpd_latency,
            crypto: crypto.map(|(sz, pw)| CryptoManager::new(sz, pw)),
        }
    }
}

impl<T> Future for Connect<T>
where
    T: Stream<Item = (Packet, SocketAddr), Error = Error>
        + Sink<SinkItem = (Packet, SocketAddr), SinkError = Error>,
{
    type Item = Connected<T>;
    type Error = Error;

    fn poll(&mut self) -> Poll<Connected<T>, Error> {
        self.sock.as_mut().unwrap().poll_complete()?;

        // handle incoming packetsval: Interval::new(Duration::from_millis(100))
        loop {
            let (pack, addr) = match self.sock.as_mut().unwrap().poll() {
                Ok(Async::Ready(Some((pack, addr)))) => (pack, addr),
                Ok(Async::Ready(None)) => bail!("Underlying stream ended unexpectedly"),
                Ok(Async::NotReady) => break,
                Err(e) => {
                    warn!("Failed to parse packet: {}", e);
                    continue;
                }
            };

            // make sure it's the right addr
            if self.remote != addr {
                continue;
            }

            if let Packet::Control(ControlPacket {
                timestamp,
                dest_sockid,
                control_type: ControlTypes::Handshake(info),
            }) = pack
            {
                // make sure the sockid is right
                if dest_sockid != self.local_socket_id {
                    continue;
                }

                match self.state {
                    State::Starting(_) => {
                        info!("Got first handshake packet from {:?}", addr);

                        if info.shake_type != ShakeType::Induction {
                            info!(
                                "Was waiting for Induction (1) packet, got {:?} ({})",
                                info.shake_type, info.shake_type as u32
                            )
                        }

                        if info.info.version() != 5 {
                            bail!("Handshake was HSv4, expected HSv5");
                        }

                        // send back the same SYN cookie
                        let pack_to_send = Packet::Control(ControlPacket {
                            dest_sockid: SocketID(0),
                            timestamp,
                            control_type: ControlTypes::Handshake(HandshakeControlInfo {
                                shake_type: ShakeType::Conclusion,
                                info: HandshakeVSInfo::V5 {
                                    crypto_size: 0, // TODO: implement
                                    ext_hs: Some(SrtControlPacket::HandshakeRequest(
                                        SrtHandshake {
                                            version: SrtVersion::CURRENT,
                                            // TODO: this is hyper bad, don't blindly set send flag
                                            // if you don't pass TSBPDRCV, it doens't set the latency correctly for some reason. Requires more research
                                            peer_latency: Duration::from_secs(0), // TODO: research
                                            flags: SrtShakeFlags::TSBPDSND
                                                | SrtShakeFlags::TSBPDRCV, // TODO: the reference implementation sets a lot more of these, research
                                            latency: self.tsbpd_latency,
                                        },
                                    )),
                                    ext_km: self.crypto.as_mut().map(|manager| {
                                        SrtControlPacket::KeyManagerRequest(SrtKeyMessage {
                                            pt: 2,       // TODO: what is this
                                            sign: 8_233, // TODO: again
                                            keki: 0,
                                            cipher: CipherType::CTR,
                                            auth: 0,
                                            se: 2,
                                            salt: Vec::from(manager.salt()),
                                            even_key: Some(Vec::from(manager.wrap_key().unwrap())),
                                            odd_key: None,
                                            wrap_data: [0; 8],
                                        })
                                    }),
                                    ext_config: None,
                                },
                                ..info
                            }),
                        });

                        self.sock
                            .as_mut()
                            .unwrap()
                            .start_send((pack_to_send.clone(), self.remote))?;
                        self.sock.as_mut().unwrap().poll_complete()?;

                        // move on to the next stage
                        self.state = State::First(pack_to_send);
                    }
                    State::First(_) => {
                        if info.shake_type != ShakeType::Conclusion {
                            info!(
                                "Was waiting for Conclusion (-1) hanshake type type, got {:?}",
                                info.shake_type
                            );
                            // discard
                            continue;
                        }

                        let latency = if let HandshakeVSInfo::V5 {
                            ext_hs: Some(SrtControlPacket::HandshakeResponse(hs)),
                            ..
                        } = info.info
                        {
                            hs.latency
                        } else {
                            warn!("Did not get SRT handshake response in final handshake packet, using latency from this end");
                            self.tsbpd_latency
                        };

                        info!("Got second handshake, connection established to {:?} with latency {}ms", addr, latency.as_secs() * 1000 + u64::from(latency.subsec_nanos()) / 1_000_000);

                        // this packet has the final settings in it, and after this the connection is done
                        return Ok(Async::Ready(Connected::new(
                            self.sock.take().unwrap(),
                            ConnectionSettings {
                                remote: self.remote,
                                max_flow_size: info.max_flow_size,
                                max_packet_size: info.max_packet_size,
                                init_seq_num: info.init_seq_num,
                                socket_start_time: Instant::now(), // restamp the socket start time, so TSBPD works correctly
                                local_sockid: self.local_socket_id,
                                remote_sockid: info.socket_id,
                                tsbpd_latency: latency,
                                // TODO: is this right? Needs testing.
                                handshake_returner: Box::new(move |_| None),
                            },
                        )));
                    }
                }
            } else {
                info!("Non-handshake packet received during handshake phase")
            }
        }

        loop {
            try_ready!(self.send_interval.poll());

            info!("Sending handshake packet to: {}", self.remote);

            match self.state {
                State::Starting(ref pack) => {
                    // send a handshake
                    self.sock
                        .as_mut()
                        .unwrap()
                        .start_send((pack.clone(), self.remote))?;
                    self.sock.as_mut().unwrap().poll_complete()?;
                }
                State::First(ref pack) => {
                    self.sock
                        .as_mut()
                        .unwrap()
                        .start_send((pack.clone(), self.remote))?;
                    self.sock.as_mut().unwrap().poll_complete()?;
                }
            }
        }
    }
}
