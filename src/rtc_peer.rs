//! Un `PeerConnection` WebRTC por visor. Señalización sin trickle: el
//! navegador hace POST de su oferta y recibe la respuesta con todos los
//! candidatos ICE ya incluidos (en LAN sólo hacen falta candidatos host).

use std::net::{IpAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_H264;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use tokio::sync::{broadcast, mpsc};
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceGatheringState, RTCPeerConnectionState, RTCSessionDescription,
    Registry, register_default_interceptors,
};

use crate::encoder::{EncodedFrame, KeyframeRequest};

struct Handler {
    gathered: mpsc::Sender<()>,
    state: mpsc::UnboundedSender<RTCPeerConnectionState>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.state.send(state);
    }
}

fn h264_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            .to_owned(),
        rtcp_feedback: vec![],
    }
}

/// IP local con la que se alcanza a `peer` (la interfaz correcta en equipos
/// con varias redes: wifi, ethernet, tethering USB…).
fn local_ip_towards(peer: IpAddr) -> IpAddr {
    let probe = || -> std::io::Result<IpAddr> {
        let sock = UdpSocket::bind(("0.0.0.0", 0))?;
        sock.connect((peer, 9))?;
        Ok(sock.local_addr()?.ip())
    };
    probe().unwrap_or(IpAddr::from([0, 0, 0, 0]))
}

/// Negocia una sesión con la oferta del navegador y empieza a enviar vídeo.
pub async fn answer(
    offer: RTCSessionDescription,
    peer_ip: IpAddr,
    frames: broadcast::Receiver<EncodedFrame>,
    keyreq: KeyframeRequest,
) -> Result<RTCSessionDescription> {
    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: h264_codec(),
            payload_type: 102,
            ..Default::default()
        },
        RtpCodecKind::Video,
    )?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

    let (gathered_tx, mut gathered_rx) = mpsc::channel(1);
    let (state_tx, state_rx) = mpsc::unbounded_channel();
    let bind_ip = if peer_ip.is_loopback() {
        IpAddr::from([127, 0, 0, 1])
    } else {
        local_ip_towards(peer_ip)
    };

    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler { gathered: gathered_tx, state: state_tx }))
        .with_udp_addrs(vec![format!("{bind_ip}:0")])
        .build()
        .await?;
    let pc: Arc<dyn PeerConnection> = Arc::new(pc);

    let ssrc = rand_ssrc();
    let track = Arc::new(TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "spacedesk".to_owned(),
            "screen".to_owned(),
            "screen".to_owned(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: h264_codec(),
                ..Default::default()
            }],
        ),
    )?);
    let sender = pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await?;

    pc.set_remote_description(offer).await?;
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer).await?;
    tokio::time::timeout(Duration::from_secs(5), gathered_rx.recv())
        .await
        .context("timeout reuniendo candidatos ICE")?;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("sin descripción local"))?;

    let payload_type = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .ok_or_else(|| anyhow!("el navegador no aceptó H.264"))?;

    tokio::spawn(async move {
        if let Err(e) = pump(&track, ssrc, payload_type, frames, keyreq, state_rx).await {
            tracing::warn!("WebRTC {peer_ip}: {e:#}");
        }
        let _ = pc.close().await;
        tracing::info!("WebRTC {peer_ip}: desconectado");
    });

    Ok(local)
}

async fn pump(
    track: &TrackLocalStaticSample,
    ssrc: u32,
    payload_type: u8,
    mut frames: broadcast::Receiver<EncodedFrame>,
    keyreq: KeyframeRequest,
    mut state_rx: mpsc::UnboundedReceiver<RTCPeerConnectionState>,
) -> Result<()> {
    // Esperar a que DTLS/ICE estén listos.
    loop {
        match state_rx.recv().await {
            Some(RTCPeerConnectionState::Connected) => break,
            Some(
                RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed
                | RTCPeerConnectionState::Disconnected,
            )
            | None => return Err(anyhow!("la conexión no llegó a establecerse")),
            _ => {}
        }
    }
    tracing::info!("WebRTC conectado, enviando vídeo");
    keyreq.request();
    let mut synced = false;
    let mut last_pts: Option<u64> = None;

    loop {
        tokio::select! {
            state = state_rx.recv() => match state {
                Some(RTCPeerConnectionState::Connected) | Some(RTCPeerConnectionState::Connecting) => {}
                _ => return Ok(()),
            },
            frame = frames.recv() => {
                let frame = match frame {
                    Ok(f) => f,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        synced = false;
                        keyreq.request();
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                };
                if !synced && !frame.keyframe {
                    continue;
                }
                synced = true;
                let duration = last_pts
                    .map(|p| Duration::from_micros(frame.pts_us.saturating_sub(p)))
                    .unwrap_or_default();
                last_pts = Some(frame.pts_us);
                track
                    .sample_writer(ssrc, payload_type)
                    .write_sample(&Sample {
                        data: frame.data,
                        duration,
                        ..Sample::new(Instant::now())
                    })
                    .await?;
            }
        }
    }
}

fn rand_ssrc() -> u32 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(Instant::now()) as u32
}
