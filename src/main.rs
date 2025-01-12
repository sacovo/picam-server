use std::{
    io::{self, Write},
    iter::zip,
    path::Path,
    sync::Arc,
    thread,
    time::Duration,
};
use thiserror::Error;

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt, TryStreamExt};
use libcamera::{
    camera::{CameraConfiguration, CameraConfigurationStatus},
    camera_manager::CameraManager,
    framebuffer::AsFrameBuffer,
    framebuffer_allocator::{FrameBuffer, FrameBufferAllocator},
    framebuffer_map::MemoryMappedFrameBuffer,
    pixel_format::PixelFormat,
    request::{Request, ReuseFlag},
    stream::{StreamConfigurationRef, StreamRole},
};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use serde::{Deserialize, Serialize};
use v4l2r::{
    controls::{
        codec::{VideoBitrate, VideoH264Level, VideoH264Profile},
        SafeExtControl,
    },
    device::queue::{
        direction::Capture,
        dqbuf::DqBuffer,
        generic::{GenericBufferHandles, GenericQBuffer, GenericSupportedMemoryType},
        handles_provider::MmapProvider,
    },
    encoder::{CompletedOutputBuffer, Encoder, ReadyToEncode},
    ioctl::{s_ext_ctrls, CtrlWhich},
    memory::MmapHandle,
    Format,
};

const PF: &[u8; 4] = b"NV12";
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

const PIXEL_FORMAT: PixelFormat = PixelFormat::new(u32::from_le_bytes(*PF), 0);

struct CameraResizeRequest {
    width: u32,
    height: u32,
    stride: u32,
    size: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

enum CameraCommand {
    Resize(CameraResizeRequest),
    Empty,
}

enum EncoderCommand {
    NextFrame,
    RestartEncoder,
    Resize(Size),
    SetBitrate(i32),
    SetLevel(VideoH264Level),
    SetProfile(VideoH264Profile),
}

#[derive(Debug, Serialize, Deserialize)]
enum ClientCommands {
    RestartEncoder,
    Resize(Size),
    SetBitrate(i32),
    SetLevel(String),
    SetProfile(String),
    NextFrame,
}

fn configure_camera(
    cfgs: &mut CameraConfiguration,
    idx: usize,
    width: u32,
    height: u32,
    stride: Option<u32>,
) {
    cfgs.get_mut(idx).unwrap().set_pixel_format(PIXEL_FORMAT);
    cfgs.get_mut(idx)
        .unwrap()
        .set_size(libcamera::geometry::Size { width, height });
    if let Some(stride) = stride {
        cfgs.get_mut(idx).unwrap().set_stride(stride);
    }
}

fn camera_loop(
    idx: usize,
    txs: [std::sync::mpsc::Sender<Arc<Vec<u8>>>; 2],
    mut rx: tokio::sync::watch::Receiver<CameraCommand>,
) -> Result<()> {
    let manager = CameraManager::new().unwrap();
    let camera_list = manager.cameras();
    let camera = camera_list
        .get(idx)
        .context(format!("Could not open camera at index {idx}"))
        .unwrap();
    let roles = vec![StreamRole::VideoRecording, StreamRole::VideoRecording];
    let mut cfgs = camera
        .generate_configuration(&roles)
        .context("Could not read configuration")?;

    configure_camera(&mut cfgs, 0, 1080, 720, None);
    configure_camera(&mut cfgs, 1, WIDTH, HEIGHT, None);

    match cfgs.validate() {
        CameraConfigurationStatus::Valid => println!("Configuration is valid"),
        CameraConfigurationStatus::Invalid => panic!("Error while validating configuration"),
        CameraConfigurationStatus::Adjusted => println!("Configuration was adjusted: {:#?}", cfgs),
    }

    println!("Using configuration: {:#?}", cfgs);

    let mut camera = camera.acquire()?;
    camera.configure(&mut cfgs)?;

    let mut streams = prepare_camera(&cfgs, &mut camera)?;

    let (txr, rxr) = std::sync::mpsc::channel();
    camera.on_request_completed(move |req| {
        txr.send(req).expect("Could not send request");
    });

    loop {
        if rx.has_changed()? {
            let cmd = rx.borrow_and_update();
            match *cmd {
                CameraCommand::Resize(CameraResizeRequest {
                    width,
                    height,
                    stride,
                    size,
                }) => {
                    println!("[CAM] Received new size: {width}x{height} with stride {stride}");
                    camera.stop()?;
                    configure_camera(&mut cfgs, 0, width, height, Some(stride));
                    println!(
                        "Requested image size is {}, configured size is {}",
                        size,
                        cfgs.get(0).unwrap().get_frame_size()
                    );
                    println!("Using configuration: {:#?}", cfgs);
                    camera.configure(&mut cfgs)?;
                    streams = prepare_camera(&cfgs, &mut camera)?;
                }
                CameraCommand::Empty => {}
            }
        }
        let mut req = rxr.recv_timeout(Duration::from_secs(2)).unwrap();

        for i in 0..streams.len() {
            match read_camera_buffer(cfgs.get(i).unwrap(), &streams[i], &req) {
                Ok(value) => {
                    txs[i].send(Arc::new(value))?;
                }
                Err(_) => continue,
            }
        }
        req.reuse(ReuseFlag::REUSE_BUFFERS);
        camera.queue_request(req)?;
    }
}

#[derive(Debug, Error)]
pub enum CameraError {
    #[error("Buffer is too large")]
    BufferTooLarge,
    #[error("Buffer is too small")]
    BufferTooSmall,
    #[error("Could not find buffer for the given stream")]
    NoBufferForStream,
}

fn read_camera_buffer(
    cfg: libcamera::utils::Immutable<StreamConfigurationRef>,
    stream: &libcamera::stream::Stream,
    req: &libcamera::request::Request,
) -> Result<Vec<u8>> {
    let framebuffer: &MemoryMappedFrameBuffer<FrameBuffer> = req
        .buffer(stream)
        .context("No buffer for this stream in this request.")?;
    let planes = framebuffer.data();
    let frame_size = cfg.get_frame_size() as usize;
    let mut data: Vec<u8> = vec![0; frame_size];
    let plane_info = framebuffer.metadata().unwrap().planes();
    let mut pos = 0;

    for i in 0..plane_info.len() {
        let bytes_used = plane_info.get(i).unwrap().bytes_used as usize;

        if pos + bytes_used > data.len() {
            bail!("Buffer is to large, skipping this image");
        }

        let plane = &planes.get(i).unwrap()[0..bytes_used];
        data[pos..(pos + bytes_used)].copy_from_slice(plane);
        pos += bytes_used;
    }
    if pos != frame_size {
        bail!("Buffer was not filled, skipping this image");
    }
    Ok(data)
}

fn prepare_camera(
    cfgs: &CameraConfiguration,
    camera: &mut libcamera::camera::ActiveCamera<'_>,
) -> Result<[libcamera::stream::Stream; 2], anyhow::Error> {
    let mut alloc = FrameBufferAllocator::new(&*camera);

    let (stream0, stream1, reqs0) = create_stream(cfgs, camera, &mut alloc)?;

    camera.start(None)?;

    for req in reqs0 {
        camera.queue_request(req)?;
    }

    Ok([stream0, stream1])
}

fn create_stream(
    cfgs: &CameraConfiguration,
    camera: &mut libcamera::camera::ActiveCamera<'_>,
    alloc: &mut FrameBufferAllocator,
) -> Result<
    (
        libcamera::stream::Stream,
        libcamera::stream::Stream,
        Vec<Request>,
    ),
    anyhow::Error,
> {
    let cfg = cfgs.get(0).context("Could not get config for stream 1")?;
    let stream = cfg
        .stream()
        .context("Could not get stream from configuration")?;
    let buffers = create_buffers(alloc, stream);
    let cfg = cfgs.get(1).context("Could not get config for stream 1")?;
    let stream1 = cfg.stream().context("Could not get stream 1")?;
    let buffers1 = create_buffers(alloc, stream1);

    let reqs = zip(buffers, buffers1)
        .enumerate()
        .map(|(i, buf)| {
            let mut req = camera
                .create_request(Some(i as u64))
                .expect("Could not create request");
            req.add_buffer(&stream, buf.0)
                .expect("Could not add buffer to request");
            req.add_buffer(&stream1, buf.1)
                .expect("Could not add buffer to request");
            req
        })
        .collect::<Vec<_>>();
    Ok((stream, stream1, reqs))
}

fn create_buffers(
    alloc: &mut FrameBufferAllocator,
    stream: libcamera::stream::Stream,
) -> Vec<MemoryMappedFrameBuffer<FrameBuffer>> {
    let buffers = alloc
        .alloc(&stream)
        .expect("Failed to allocate buffers for stream {idx}");

    buffers
        .into_iter()
        .map(|buf| MemoryMappedFrameBuffer::new(buf).expect("Could not create mmap buffer"))
        .collect::<Vec<_>>()
}

fn create_encoder(
    width: usize,
    height: usize,
) -> Result<(
    Encoder<ReadyToEncode<GenericBufferHandles, MmapProvider>>,
    Format,
)> {
    let encoder_path = "/dev/video11";

    let encoder = Encoder::open(Path::new(&encoder_path))?
        .set_capture_format(|f| {
            let _format: Format = f.set_pixelformat(b"H264").set_size(width, height).apply()?;

            Ok(())
        })?
        .set_output_format(|f| {
            let _format: Format = f.set_pixelformat(PF).set_size(width, height).apply()?;
            Ok(())
        })?;

    const NUM_BUFFERS: usize = 10;

    let output_mem = GenericSupportedMemoryType::Mmap;

    let output_format = encoder
        .get_output_format()
        .expect("Failed to get output format");
    println!("Encoder output format: {:?}", output_format);

    let capture_format = encoder
        .get_capture_format()
        .expect("Failed to get capture format");

    println!("Encoder capture format: {:?}", capture_format);

    let encoder = encoder
        .allocate_output_buffers_generic::<GenericBufferHandles>(output_mem, NUM_BUFFERS)
        .expect("Failed to allocate OUTPUT buffers")
        .allocate_capture_buffers(NUM_BUFFERS, MmapProvider::new(&capture_format))
        .expect("Failed to allocate CAPTURE buffers");
    Ok((encoder, output_format))
}

fn fixed_encoder_thread(
    img_in: std::sync::mpsc::Receiver<Arc<Vec<u8>>>,
    encoder_out: broadcast::Sender<Arc<Vec<u8>>>,
) -> Result<()> {
    let input_done_cb = |buffer: CompletedOutputBuffer<GenericBufferHandles>| {
        let handles = match buffer {
            CompletedOutputBuffer::Dequeued(mut buf) => buf.take_handles().unwrap(),
            CompletedOutputBuffer::Canceled(buf) => buf.plane_handles,
        };
        if let GenericBufferHandles::Mmap(_) = handles {};
    };

    let output_ready_cb = move |cap_dqbuf: DqBuffer<Capture, Vec<MmapHandle>>| {
        let bytes_used = *cap_dqbuf.data.get_first_plane().bytesused as usize;
        // Ignore zero-sized buffers.
        if bytes_used == 0 {
            return;
        }
        io::stdout().flush().unwrap();
        let data = cap_dqbuf.get_plane_mapping(0).unwrap().as_ref().to_vec();
        encoder_out.send(Arc::new(data)).unwrap();
    };

    let (encoder, _) = create_encoder(WIDTH as usize, HEIGHT as usize)?;

    let mut encoder = encoder.start(input_done_cb, output_ready_cb)?;

    loop {
        let buf = encoder.get_buffer().unwrap();
        if let GenericQBuffer::Mmap(buf) = buf {
            let mut mapping = buf.get_plane_mapping(0).unwrap();
            let frame = img_in.recv_timeout(Duration::from_secs(10)).unwrap();

            mapping.as_mut().copy_from_slice(&frame);
            buf.queue(&[frame.len()])?;
        }
    }
}

fn encoder_thread(
    frame_in: std::sync::mpsc::Receiver<Arc<Vec<u8>>>,
    encoder_out: broadcast::Sender<Arc<Vec<u8>>>,
    mut next_frame: mpsc::Receiver<EncoderCommand>,
    tx_cam: watch::Sender<CameraCommand>,
) -> Result<()> {
    let input_done_cb = |buffer: CompletedOutputBuffer<GenericBufferHandles>| {
        let handles = match buffer {
            CompletedOutputBuffer::Dequeued(mut buf) => buf.take_handles().unwrap(),
            CompletedOutputBuffer::Canceled(buf) => buf.plane_handles,
        };
        if let GenericBufferHandles::Mmap(_) = handles {};
    };

    let output_ready_cb = move |cap_dqbuf: DqBuffer<Capture, Vec<MmapHandle>>| {
        let bytes_used = *cap_dqbuf.data.get_first_plane().bytesused as usize;
        // Ignore zero-sized buffers.
        if bytes_used == 0 {
            return;
        }
        io::stdout().flush().unwrap();
        let data = cap_dqbuf.get_plane_mapping(0).unwrap().as_ref().to_vec();
        encoder_out.send(Arc::new(data)).unwrap();
    };

    let (encoder, _) = create_encoder(WIDTH as usize, HEIGHT as usize)?;

    let mut encoder = encoder
        .start(input_done_cb, output_ready_cb.clone())
        .expect("Failed to start encoder");

    loop {
        let v4l2_buffer = match encoder.get_buffer() {
            Ok(buffer) => buffer,
            Err(e) => panic!("{}", e),
        };

        let buf = match v4l2_buffer {
            GenericQBuffer::Mmap(buf) => buf,
            _ => panic!("Not the right buffer format!"),
        };
        let mut mapping = match buf.get_plane_mapping(0) {
            Some(m) => m,
            _ => panic!("Mapping has no planes"),
        };
        let mut frame = frame_in.recv_timeout(Duration::from_secs(10)).unwrap();
        // assert_eq!(frame.len(), mapping.len());
        while frame.len() != mapping.len() {
            frame = frame_in.recv_timeout(Duration::from_secs(10)).unwrap();
        }

        match next_frame.try_recv() {
            Err(mpsc::error::TryRecvError::Empty) => continue, // Discard this frame
            Err(mpsc::error::TryRecvError::Disconnected) => break,
            Ok(EncoderCommand::NextFrame) => {
                // This means we should send this frame to the encoder
                mapping.as_mut().copy_from_slice(&frame);
                buf.queue(&[frame.len()])?;
            }
            Ok(EncoderCommand::RestartEncoder) => {
                encoder = encoder
                    .stop()
                    .unwrap()
                    .start(input_done_cb, output_ready_cb.clone())
                    .unwrap();
            }
            Ok(EncoderCommand::Resize(Size { width, height })) => {
                println!("[ENC] Received new size: {width}x{height}");
                let old = encoder.stop();
                drop(old);
                let (tmp, cap) = create_encoder(width as usize, height as usize).unwrap();

                let planeformat = cap.plane_fmt.first().unwrap();
                encoder = tmp.start(input_done_cb, output_ready_cb.clone()).unwrap();
                tx_cam
                    .send(CameraCommand::Resize(CameraResizeRequest {
                        width,
                        height,
                        stride: planeformat.bytesperline,
                        size: planeformat.sizeimage,
                    }))
                    .unwrap();
            }
            Ok(EncoderCommand::SetProfile(profile)) => {
                let mut profile = SafeExtControl::<VideoH264Profile>::from_value(profile.into());
                println!("[ENC] Setting new Profile: {}", profile.value());
                s_ext_ctrls(&encoder, CtrlWhich::Current, &mut profile)?;
            }
            Ok(EncoderCommand::SetBitrate(bitrate)) => {
                let mut bitrate = SafeExtControl::<VideoBitrate>::from_value(bitrate);
                s_ext_ctrls(&encoder, CtrlWhich::Current, &mut bitrate)?;
            }
            Ok(EncoderCommand::SetLevel(level)) => {
                let mut level = SafeExtControl::<VideoH264Level>::from_value(level.into());
                s_ext_ctrls(&encoder, CtrlWhich::Current, &mut level)?;
            }
        };
    }

    Ok(())
}

use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct Args {
    // Name of the camera that should be used
    #[arg(short, long)]
    device_name: String,

    // TCP listen port of the fixed encoder frames
    #[arg(short, long)]
    port_fixed: usize,

    // TCP isten port of the variable encoder frames
    #[arg(short, long)]
    port_variable: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cam_idx = 0;

    let (tx, rx) = std::sync::mpsc::channel();
    let (tx1, rx1) = std::sync::mpsc::channel();
    let (tx_cam, rx_cam) = tokio::sync::watch::channel(CameraCommand::Empty);
    thread::spawn(move || {
        camera_loop(cam_idx, [tx, tx1], rx_cam).expect("Error in variable encoder thread");
    });

    let (tx_enc, _rx_enc) = tokio::sync::broadcast::channel(10);
    let (tx_next, rx_next) = tokio::sync::mpsc::channel(10);
    let tx_tmp = tx_enc.clone();
    thread::spawn(move || {
        encoder_thread(rx, tx_tmp, rx_next, tx_cam).expect("Error in thread");
    });

    let (tx_fix_enc, _rx_enc) = tokio::sync::broadcast::channel(10);
    thread::spawn(move || {
        fixed_encoder_thread(rx1, tx_fix_enc).expect("Error in fixed encoder thread");
    });

    let addr = "0.0.0.0:8111";
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (socket, addr) = listener.accept().await?;

        let framed = Framed::new(socket, LengthDelimitedCodec::new());
        let (mut swx, srx) = framed.split();
        let mut deserializer = tokio_serde::SymmetricallyFramed::new(
            srx,
            SymmetricalBincode::<ClientCommands>::default(),
        );
        let mut rx_enc = tx_enc.subscribe();

        let tx = tx_next.clone();

        tokio::spawn(async move {
            while let Some(cmd) = deserializer.try_next().await.unwrap() {
                match cmd {
                    ClientCommands::Resize(Size { width, height }) => {
                        println!("[TCP] Received new size: {width}x{height}");
                        tx.send(EncoderCommand::Resize(Size { width, height }))
                            .await
                            .expect("Could not send resize command");
                        tx.send(EncoderCommand::RestartEncoder).await.expect("Send");
                    }
                    ClientCommands::SetBitrate(bitrate) => {
                        tx.send(EncoderCommand::SetBitrate(bitrate))
                            .await
                            .expect("Could not send bitrate cmd");
                    }
                    ClientCommands::SetProfile(profile) => {
                        let profile = match profile.as_str() {
                            "baseline" => VideoH264Profile::Baseline,
                            "main" => VideoH264Profile::Main,
                            "high" => VideoH264Profile::High,
                            _ => VideoH264Profile::Baseline,
                        };

                        println!("[TCP] Received new profile: {profile:?}");
                        tx.send(EncoderCommand::SetProfile(profile))
                            .await
                            .expect("Could not send bitrate cmd");
                    }
                    ClientCommands::SetLevel(level) => {
                        let level = match level.as_str() {
                            "11" => VideoH264Level::L1_1,
                            "21" => VideoH264Level::L2_1,
                            "22" => VideoH264Level::L2_2,
                            "3" => VideoH264Level::L3_1,
                            "32" => VideoH264Level::L3_2,
                            "4" => VideoH264Level::L4_0,
                            "41" => VideoH264Level::L4_1,
                            _ => VideoH264Level::L4_2,
                        };
                        println!("[TCP] Received new profile: {level:?}");
                        tx.send(EncoderCommand::SetLevel(level))
                            .await
                            .expect("Could not send bitrate cmd");
                    }
                    ClientCommands::RestartEncoder => {
                        tx.send(EncoderCommand::RestartEncoder)
                            .await
                            .expect("Could not send cmd");
                    }
                    ClientCommands::NextFrame => {
                        tx.send(EncoderCommand::NextFrame)
                            .await
                            .expect("Could not send cmd");
                    }
                }
            }
            println!("Invalid command received, closing connection");
        });

        let tx = tx_next.clone();
        println!("New connection from {addr}");

        tokio::spawn(async move {
            tx.send(EncoderCommand::RestartEncoder)
                .await
                .expect("Could not send restart cmd");
            loop {
                // Get the next frame from the buffer
                let buf = match rx_enc.recv().await {
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Ok(buf) => buf,
                };

                swx.send(tokio_util::bytes::Bytes::copy_from_slice(buf.as_slice()))
                    .await
                    .expect("Could not send bytes");
                swx.flush().await.unwrap();
            }
        });
    }
}
