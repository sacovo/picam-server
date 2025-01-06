use std::{
    io::{self, Write},
    path::Path,
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use libcamera::{
    camera::{CameraConfiguration, CameraConfigurationStatus},
    camera_manager::CameraManager,
    framebuffer::AsFrameBuffer,
    framebuffer_allocator::{FrameBuffer, FrameBufferAllocator},
    framebuffer_map::MemoryMappedFrameBuffer,
    geometry::Size,
    pixel_format::PixelFormat,
    request::ReuseFlag,
    stream::StreamRole,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{broadcast, mpsc, watch},
};
use v4l2r::{
    device::queue::{
        direction::Capture,
        dqbuf::DqBuffer,
        generic::{GenericBufferHandles, GenericQBuffer, GenericSupportedMemoryType},
        handles_provider::MmapProvider,
    },
    encoder::{CompletedOutputBuffer, Encoder, ReadyToEncode},
    memory::MmapHandle,
    Format,
};

const PF: &[u8; 4] = b"NV12";
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

const PIXEL_FORMAT: PixelFormat = PixelFormat::new(u32::from_le_bytes(*PF), 0);

enum Command {
    NextFrame,
    Resize(u32, u32),
    ResizeCamera(u32, u32, u32, u32),
    RestartEncoder,
}

fn configure_camera(cfgs: &mut CameraConfiguration, width: u32, height: u32, stride: Option<u32>) {
    cfgs.get_mut(0).unwrap().set_pixel_format(PIXEL_FORMAT);
    cfgs.get_mut(0).unwrap().set_size(Size { width, height });
    if let Some(stride) = stride {
        cfgs.get_mut(0).unwrap().set_stride(stride);
    }

    match cfgs.validate() {
        CameraConfigurationStatus::Valid => println!("Configuration is valid"),
        CameraConfigurationStatus::Invalid => panic!("Error while validating configuration"),
        CameraConfigurationStatus::Adjusted => println!("Configuration was adjusted: {:#?}", cfgs),
    }
}

fn camera_loop(
    idx: usize,
    tx: std::sync::mpsc::Sender<Arc<Vec<u8>>>,
    mut rx: tokio::sync::watch::Receiver<Command>,
) -> Result<()> {
    let manager = CameraManager::new().unwrap();
    let camera_list = manager.cameras();
    let camera = camera_list
        .get(idx)
        .context(format!("Could not open camera at index {idx}"))
        .unwrap();
    let roles = vec![StreamRole::VideoRecording];
    let mut cfgs = camera
        .generate_configuration(&roles)
        .context("Could not read configuration")?;

    configure_camera(&mut cfgs, WIDTH, HEIGHT, None);

    println!("Using configuration: {:#?}", cfgs);

    let mut camera = camera.acquire()?;
    camera.configure(&mut cfgs)?;

    let (mut stream, mut frame_size) = fun_name(&cfgs, &mut camera)?;

    let (txr, rxr) = std::sync::mpsc::channel();
    camera.on_request_completed(move |req| {
        txr.send(req).expect("Could not send request");
    });

    'main: loop {
        if rx.has_changed()? {
            let cmd = rx.borrow_and_update();
            match *cmd {
                Command::NextFrame => {}
                Command::RestartEncoder => {}
                Command::Resize(_, _) => {}
                Command::ResizeCamera(width, height, stride, size) => {
                    println!("[CAM] Received new size: {width}x{height} with stride {stride}");
                    camera.stop()?;
                    configure_camera(&mut cfgs, width, height, Some(stride));
                    println!(
                        "Requested image size is {}, configured size is {}",
                        size,
                        cfgs.get(0).unwrap().get_frame_size()
                    );
                    println!("Using configuration: {:#?}", cfgs);
                    camera.configure(&mut cfgs)?;
                    (stream, frame_size) = fun_name(&cfgs, &mut camera)?;
                }
            }
        }
        let mut req = rxr.recv_timeout(Duration::from_secs(2)).unwrap();

        let framebuffer: &MemoryMappedFrameBuffer<FrameBuffer> = req.buffer(&stream).unwrap();
        let planes = framebuffer.data();
        let mut data: Vec<u8> = vec![0; frame_size];
        let plane_info = framebuffer.metadata().unwrap().planes();
        let mut pos = 0;
        for i in 0..plane_info.len() {
            let bytes_used = plane_info.get(i).unwrap().bytes_used as usize;

            if pos + bytes_used > data.len() {
                println!("Buffer is to large, skipping this image");
                continue 'main;
            }

            let plane = &planes.get(i).unwrap()[0..bytes_used];
            data[pos..(pos + bytes_used)].copy_from_slice(plane);
            pos += bytes_used;
        }
        if pos != frame_size {
            println!("Buffer was not filled, skipping this image");
            continue;
        }
        tx.send(Arc::new(data))?;
        req.reuse(ReuseFlag::REUSE_BUFFERS);
        camera.queue_request(req)?;
    }
}

fn fun_name(
    cfgs: &CameraConfiguration,
    camera: &mut libcamera::camera::ActiveCamera<'_>,
) -> Result<(libcamera::stream::Stream, usize), anyhow::Error> {
    let cfg = cfgs
        .get(0)
        .context("Could not get config for first stream")?;
    let stream = cfg.stream().context("Could not get stream")?;
    let mut alloc = FrameBufferAllocator::new(&*camera);
    let buffers = alloc.alloc(&stream).unwrap();

    let buffers = buffers
        .into_iter()
        .map(|buf| MemoryMappedFrameBuffer::new(buf).unwrap())
        .collect::<Vec<_>>();
    let reqs = buffers
        .into_iter()
        .enumerate()
        .map(|(i, buf)| {
            let mut req = camera.create_request(Some(i as u64)).unwrap();
            req.add_buffer(&stream, buf).unwrap();
            req
        })
        .collect::<Vec<_>>();
    camera.start(None)?;
    for req in reqs {
        camera.queue_request(req)?;
    }
    let frame_size = cfgs.get(0).unwrap().get_frame_size() as usize;
    Ok((stream, frame_size))
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

fn encoder_thread(
    frame_in: std::sync::mpsc::Receiver<Arc<Vec<u8>>>,
    encoder_out: broadcast::Sender<Arc<Vec<u8>>>,
    mut next_frame: mpsc::Receiver<Command>,
    tx_cam: watch::Sender<Command>,
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
            Ok(Command::NextFrame) => {
                // This means we should send this frame to the encoder
                mapping.as_mut().copy_from_slice(&frame);
                buf.queue(&[frame.len()])?;
            }
            Ok(Command::ResizeCamera(_, _, _, _)) => (),
            Ok(Command::RestartEncoder) => {
                println!("[ENC] Restart requested");
                encoder = encoder
                    .stop()
                    .unwrap()
                    .start(input_done_cb, output_ready_cb.clone())
                    .unwrap();
            }
            Ok(Command::Resize(width, height)) => {
                println!("[ENC] Received new size: {width}x{height}");
                let old = encoder.stop();
                drop(old);
                let (tmp, cap) = create_encoder(width as usize, height as usize).unwrap();

                let planeformat = cap.plane_fmt.first().unwrap();
                encoder = tmp.start(input_done_cb, output_ready_cb.clone()).unwrap();
                tx_cam
                    .send(Command::ResizeCamera(
                        width,
                        height,
                        planeformat.bytesperline,
                        planeformat.sizeimage,
                    ))
                    .unwrap();
            }
        };
    }

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cam_idx = 0;

    let (tx, rx) = std::sync::mpsc::channel();
    let (tx_cam, rx_cam) = tokio::sync::watch::channel(Command::NextFrame);
    thread::spawn(move || {
        camera_loop(cam_idx, tx, rx_cam).expect("Error in thread");
    });

    let (tx_enc, _rx_enc) = tokio::sync::broadcast::channel(1);
    let (tx_next, rx_next) = tokio::sync::mpsc::channel(10);
    let tx_tmp = tx_enc.clone();
    thread::spawn(move || {
        encoder_thread(rx, tx_tmp, rx_next, tx_cam).expect("Error in thread");
    });

    let addr = "0.0.0.0:8111";
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (socket, addr) = listener.accept().await?;
        let mut rx_enc = tx_enc.subscribe();

        let (mut srx, mut swx) = socket.into_split();

        let tx = tx_next.clone();

        tokio::spawn(async move {
            loop {
                let mut buf = [0u8; 8];
                srx.read_exact(&mut buf)
                    .await
                    .expect("Could not read two values");
                let width = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let height = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                println!("[TCP] Received new size: {width}x{height}");
                tx.send(Command::Resize(width, height))
                    .await
                    .expect("Could not send resize command");
            }
        });

        let tx = tx_next.clone();
        println!("New connection from {addr}");

        tokio::spawn(async move {
            tx.send(Command::RestartEncoder)
                .await
                .expect("Could not send restart cmd");
            loop {
                // Tell the encoder that we are ready for a new frame
                tx.send(Command::NextFrame).await.unwrap();

                // Get the next frame from the buffer
                let buf = match rx_enc.recv().await {
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(v)) => {
                        println!("Lagged: {v}");
                        continue;
                    }
                    Ok(buf) => buf,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };

                let bytes = &(buf.len() as u32).to_be_bytes();

                swx.write_all(bytes)
                    .await
                    .expect("Could not send size of buffer");

                swx.flush().await.unwrap();
                if let Err(e) = swx.write_all(&buf).await {
                    println!("Error detected, disconnecting: {e}");
                    break;
                }
                swx.flush().await.unwrap();
            }
        });
    }
}
