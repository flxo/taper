use std::{env, fmt, io, iter::once, net::ToSocketAddrs};

use anyhow::{anyhow, Context, Error};
use bytes::{Bytes, BytesMut};
use clap::{Parser, Subcommand};
use futures::{future, Sink, SinkExt, Stream, StreamExt, TryStreamExt};
use log::{debug, info, trace, warn};
use tokio::{
    io::{join, stdin, stdout},
    net::{TcpListener, TcpStream},
    select,
};
use tokio_util::{
    codec::{Framed, LengthDelimitedCodec},
    sync::CancellationToken,
};
use tun::{AsyncDevice, Device, TunPacket};

/// Taper - create and connect tap devices via TCP (or stdio)
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Attach tap device to bridge. The bridge is created if it does not exist.
    /// `ip link add name <bridge> type bridge && ip link set <bridge> up`
    #[arg(short, long)]
    bridge: Option<String>,

    /// Attach device(s) to bridge: `ip link set <dev> master bridge`
    #[arg(short, long)]
    attach: Option<Vec<String>>,

    /// Mode
    #[command(subcommand)]
    command: Mode,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Server mode
    Server { bind: String },
    /// Client mode
    Client { connect: String },
    /// Stdio mode
    Stdio,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Error> {
    let args = Args::parse();

    // Initialize logger only if not in stdio mode
    if !matches!(args.command, Mode::Stdio) {
        if env::var("RUST_LOG").is_err() {
            env::set_var("RUST_LOG", "taper=info");
        }
        env_logger::init();
    }

    debug!("Creating tap interface");
    let (tap, tap_name) = tap_interface().context("failed to create tap interface")?;
    let mut tap = tap.into_framed();

    // Create/find bridge and attach tap interface and other devices
    if let Some(bridge) = args.bridge {
        let devices = once(tap_name.as_str()).chain(
            args.attach
                .iter()
                .flat_map(|s| s.iter().map(|s| s.as_str())),
        );
        attach_create_bridge(&bridge, devices)
            .await
            .context("failed to attach tap interface to bridge")?;
    }

    match args.command {
        Mode::Server { bind } => server(bind, &mut tap, &tap_name).await?,
        Mode::Client { connect } => client(connect, &mut tap, &tap_name).await?,
        Mode::Stdio => stdio(&mut tap, &tap_name).await?,
    }

    Ok(())
}

async fn stdio<T>(tap: &mut T, tap_name: &str) -> Result<(), Error>
where
    T: Stream<Item = io::Result<TunPacket>> + Sink<TunPacket, Error = io::Error> + Unpin,
{
    let stream = join(stdin(), stdout());
    let stream = Framed::new(stream, stream_codec());

    info!("Forwarding stdio <-> {tap_name}");
    forward(stream, tap).await?;

    Ok(())
}

/// Run a server that listens for incoming connections and forwards packets to a tap interface
async fn server<S, T>(bind: S, tap: &mut T, tap_name: &str) -> Result<(), Error>
where
    S: ToSocketAddrs + fmt::Display,
    T: Stream<Item = io::Result<TunPacket>> + Sink<TunPacket, Error = io::Error> + Unpin,
{
    info!("Listening on {bind}");

    let bind = bind
        .to_socket_addrs()?
        .next()
        .ok_or(anyhow!("failed to resolve {bind}"))?;
    let listen = TcpListener::bind(bind).await?;

    loop {
        let (stream, peer) = listen.accept().await?;
        info!("Accepted connection from {peer}");

        debug!("Setting TCP_NODELAY on {peer}");
        stream.set_nodelay(true)?;

        let mut stream = Framed::new(stream, stream_codec());

        info!("Forwarding {peer} <-> {tap_name}");
        match forward(&mut stream, tap).await {
            Ok(()) => info!("Connection closed"),
            Err(e) => warn!("Connection closed: {e}"),
        }
    }
}

/// Connect to a server and forward packets between a stream and a tap interface
async fn client<S, T>(addr: S, tap: &mut T, tap_name: &str) -> Result<(), Error>
where
    S: ToSocketAddrs + fmt::Display,
    T: Stream<Item = io::Result<TunPacket>> + Sink<TunPacket, Error = io::Error> + Unpin,
{
    info!("Connecting to {addr}");
    let addr = addr
        .to_socket_addrs()?
        .next()
        .ok_or(anyhow!("failed to resolve {addr}"))?;
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;

    let stream = Framed::new(stream, stream_codec());

    info!("Forwarding {addr} <-> {tap_name}");
    forward(stream, tap).await?;

    Ok(())
}

/// Forward packets between a stream and a tap interface bidirectionally
async fn forward<S, T>(mut stream: S, tap: &mut T) -> Result<(), Error>
where
    S: Stream<Item = io::Result<BytesMut>> + Sink<Bytes, Error = io::Error> + Unpin,
    T: Stream<Item = io::Result<TunPacket>> + Sink<TunPacket, Error = io::Error> + Unpin,
{
    loop {
        select! {
            Some(frame) = stream.next() => {
                let frame = frame?;
                trace!("Forwarding {} bytes to tap", frame.len());
                tap.send(TunPacket::new(frame.into())).await?;
            }
            Some(frame) = tap.next() => {
                let frame = frame?.into_bytes();
                trace!("Forwarding {} bytes to peer", frame.len());
                stream.send(frame).await?;
            }
            else => break Ok(()),
        }
    }
}

/// Create a tap interface
fn tap_interface() -> Result<(AsyncDevice, String), Error> {
    let mut config = tun::Configuration::default();
    config.layer(tun::Layer::L2);
    config.up();
    let tap = tun::create_as_async(&config).context("failed to create tun device")?;
    let tap_name = tap.get_ref().name()?;
    info!("Created {tap_name}");
    Ok((tap, tap_name))
}

/// Attach a tap interface to a bridge
/// aka `ip link set tap0 master bridge`
async fn attach_create_bridge<'a, I>(bridge: &str, devices: I) -> Result<(), Error>
where
    I: Iterator<Item = &'a str>,
{
    let (connection, handle, _) = rtnetlink::new_connection()?;

    // Ensure the netlink connection is closed after use
    let connection = {
        let token = CancellationToken::new();
        let guard = token.clone().drop_guard();
        let cancelled = Box::pin(async move {
            token.cancelled().await;
        });
        tokio::spawn(future::select(connection, cancelled));
        guard
    };

    // Find bridge link
    let bridge_index = if let Some(bridge) = find_link_index(&handle, bridge).await {
        bridge
    } else {
        // Bridge not found. Create it.
        info!("Creating {bridge}");
        handle
            .link()
            .add()
            .bridge(bridge.to_string())
            .execute()
            .await
            .context("failed to create bridge interface")?;
        let index = find_link_index(&handle, bridge)
            .await
            .expect("failed to find created bridge interface");
        handle.link().set(index).up().execute().await?;
        index
    };

    // Find tap link
    for device in devices {
        let index = find_link_index(&handle, device)
            .await
            .ok_or(anyhow!("failed to find tap interface"))?;

        info!("Attaching {device} to {bridge}");
        handle
            .link()
            .set(index)
            .controller(bridge_index)
            .execute()
            .await
            .context("failed to attach {device} to {bridge}")?;
    }

    drop(connection);

    Ok(())
}

/// Find link index by name
async fn find_link_index(handle: &rtnetlink::Handle, name: &str) -> Option<u32> {
    handle
        .link()
        .get()
        .match_name(name.to_string())
        .execute()
        .try_next()
        .await
        .ok()
        .flatten()
        .map(|link| link.header.index)
}

/// Codec definition for length-delimited framing with peer
fn stream_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .big_endian()
        .length_field_type::<u32>()
        .max_frame_length(2000)
        .new_codec()
}
