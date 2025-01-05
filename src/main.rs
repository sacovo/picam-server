use std::{
    io::{self, Write},
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, RwLock,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result};
use libcamera::{
    camera::CameraConfigurationStatus,
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
    io::AsyncWriteExt,
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
    encoder::{CompletedOutputBuffer, Encoder},
    memory::MmapHandle,
    Format,
};

mod demo;

const PF: &[u8; 4] = b"NV12";
const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

const PIXEL_FORMAT: PixelFormat = PixelFormat::new(u32::from_le_bytes(*PF), 0);

fn camera_loop(idx: usize, tx: watch::Sender<Arc<Vec<u8>>>) -> Result<()> {
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
    cfgs.get_mut(0).unwrap().set_size(Size {
        width: WIDTH,
        height: HEIGHT,
    });
    cfgs.get_mut(0).unwrap().set_pixel_format(PIXEL_FORMAT);

    let pixel_formats = cfgs.get(0).unwrap().formats().pixel_formats();
    for i in 0..pixel_formats.len() {
        let pf = pixel_formats.get(i).unwrap();
        println!("Pixel format: {:?}, {:x}", pf, pf.fourcc());
    }

    match cfgs.validate() {
        CameraConfigurationStatus::Valid => println!("Configuration is valid"),
        CameraConfigurationStatus::Invalid => panic!("Error while validating configuration"),
        CameraConfigurationStatus::Adjusted => println!("Configuration was adjusted: {:#?}", cfgs),
    }

    println!("Using configuration: {:#?}", cfgs);
    println!(
        "Fourcc: {}",
        cfgs.get(0).unwrap().get_pixel_format().fourcc()
    );

    let mut camera = camera.acquire()?;
    camera.configure(&mut cfgs)?;

    let cfg = cfgs
        .get(0)
        .context("Could not get config for first stream")?;
    let stream = cfg.stream().context("Could not get stream")?;

    let mut alloc = FrameBufferAllocator::new(&camera);
    let buffers = alloc.alloc(&stream).unwrap();
    println!("Allocated {} buffers", buffers.len());

    // Convert FrameBuffer to MemoryMappedFrameBuffer, which allows reading &[u8]
    let buffers = buffers
        .into_iter()
        .map(|buf| MemoryMappedFrameBuffer::new(buf).unwrap())
        .collect::<Vec<_>>();

    // Create capture requests and attach buffers
    let reqs = buffers
        .into_iter()
        .enumerate()
        .map(|(i, buf)| {
            let mut req = camera.create_request(Some(i as u64)).unwrap();
            req.add_buffer(&stream, buf).unwrap();
            req
        })
        .collect::<Vec<_>>();

    let (txr, rxr) = std::sync::mpsc::channel();
    camera.on_request_completed(move |req| {
        txr.send(req).expect("Could not send request");
    });

    camera.start(None)?;
    for req in reqs {
        camera.queue_request(req)?;
    }
    let frame_size = cfgs.get(0).unwrap().get_frame_size() as usize;

    let mut t0 = SystemTime::now();
    loop {
        let mut req = rxr.recv_timeout(Duration::from_secs(2)).unwrap();
        let t1 = SystemTime::now();
        let duration = t1.duration_since(t0).unwrap().as_millis();
        t0 = t1;

        let framebuffer: &MemoryMappedFrameBuffer<FrameBuffer> = req.buffer(&stream).unwrap();
        let planes = framebuffer.data();
        let mut data: Vec<u8> = Vec::with_capacity(frame_size);
        data.resize(frame_size, 0);
        let plane_info = framebuffer.metadata().unwrap().planes();
        let mut pos = 0;
        for i in 0..plane_info.len() {
            let bytes_used = plane_info.get(i).unwrap().bytes_used as usize;
            let plane = &planes.get(i).unwrap()[0..bytes_used];
            data[pos..(pos + bytes_used)].copy_from_slice(plane);
            pos = pos + bytes_used;
        }
        assert_eq!(pos, frame_size);
        tx.send(Arc::new(data))?;
        req.reuse(ReuseFlag::REUSE_BUFFERS);
        camera.queue_request(req)?;
    }
}

fn encoder_thread(
    mut frame_in: watch::Receiver<Arc<Vec<u8>>>,
    encoder_out: broadcast::Sender<Arc<Vec<u8>>>,
    mut next_frame: mpsc::Receiver<()>,
) -> Result<()> {
    let encoder_path = "/dev/video11";

    let encoder = Encoder::open(Path::new(&encoder_path))?
        .set_capture_format(|f| {
            let format: Format = f
                .set_pixelformat(b"H264")
                .set_size(WIDTH as usize, HEIGHT as usize)
                .apply()?;
            println!("Format used: {0}", format.pixelformat);

            Ok(())
        })?
        .set_output_format(|f| {
            let format: Format = f
                .set_pixelformat(PF)
                .set_size(WIDTH as usize, HEIGHT as usize)
                .apply()?;
            println!("Format used: {0}", format.pixelformat);
            Ok(())
        })?;

    const NUM_BUFFERS: usize = 10;

    let output_mem = GenericSupportedMemoryType::Mmap;

    let input_done_cb = |buffer: CompletedOutputBuffer<GenericBufferHandles>| {
        let handles = match buffer {
            CompletedOutputBuffer::Dequeued(mut buf) => buf.take_handles().unwrap(),
            CompletedOutputBuffer::Canceled(buf) => buf.plane_handles,
        };
        match handles {
            // We have nothing to do for MMAP buffers.
            GenericBufferHandles::Mmap(_) => {}
            // For user-allocated memory, return the buffer to the free list.
            _ => {}
        };
    };

    let mut total_size = 0usize;
    let start_time = Instant::now();
    let poll_count_reader = Arc::new(AtomicUsize::new(0));
    let poll_count_writer = Arc::clone(&poll_count_reader);
    let mut frame_counter = 0usize;

    let output_ready_cb = move |cap_dqbuf: DqBuffer<Capture, Vec<MmapHandle>>| {
        let bytes_used = *cap_dqbuf.data.get_first_plane().bytesused as usize;
        // Ignore zero-sized buffers.
        if bytes_used == 0 {
            return;
        }

        total_size = total_size.wrapping_add(bytes_used);
        let elapsed = start_time.elapsed();
        frame_counter += 1;
        let fps = frame_counter as f32 / elapsed.as_millis() as f32 * 1000.0;
        let ppf = poll_count_reader.load(Ordering::SeqCst) as f32 / frame_counter as f32;
        println!(
            "\rEncoded buffer {:#5}, index: {:#2}), bytes used:{:#6} total encoded size:{:#8} fps: {:#5.2} ppf: {:#4.2}" ,
            cap_dqbuf.data.sequence(),
            cap_dqbuf.data.index(),
            bytes_used,
            total_size,
            fps,
            ppf,
        );
        io::stdout().flush().unwrap();
        let data = cap_dqbuf.get_plane_mapping(0).unwrap().as_ref().to_vec();
        encoder_out.send(Arc::new(data)).unwrap();
    };

    let output_format = encoder
        .get_output_format()
        .expect("Failed to get output format");
    println!("Adjusted output format: {:?}", output_format);

    let capture_format = encoder
        .get_capture_format()
        .expect("Failed to get capture format");

    let mut encoder = encoder
        .allocate_output_buffers_generic::<GenericBufferHandles>(output_mem, NUM_BUFFERS)
        .expect("Failed to allocate OUTPUT buffers")
        .allocate_capture_buffers(NUM_BUFFERS, MmapProvider::new(&capture_format))
        .expect("Failed to allocate CAPTURE buffers")
        .set_poll_counter(poll_count_writer)
        .start(input_done_cb, output_ready_cb)
        .expect("Failed to start encoder");

    loop {
        match next_frame.blocking_recv() {
            None => break,
            _ => (),
        };
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
        let frame = frame_in.borrow_and_update().to_vec();
        assert_eq!(frame.len(), mapping.len());

        mapping.as_mut().copy_from_slice(&frame);
        buf.queue(&[frame.len()])?;
    }

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cam_idx = 0;

    let (tx, rx) = tokio::sync::watch::channel(Arc::new(vec![]));
    thread::spawn(move || {
        camera_loop(cam_idx, tx).expect("Error in thread");
    });

    let (tx_enc, rx_enc) = tokio::sync::broadcast::channel(1);
    let (tx_next, rx_next) = tokio::sync::mpsc::channel(1);
    let tx_tmp = tx_enc.clone();
    thread::spawn(move || {
        encoder_thread(rx, tx_tmp, rx_next).expect("Error in thread");
    });

    let addr = "0.0.0.0:8111";
    let listener = TcpListener::bind(addr).await?;

    let initial_buffer: Arc<RwLock<Option<Arc<Vec<u8>>>>> = Arc::new(RwLock::new(None));

    loop {
        let (mut socket, addr) = listener.accept().await?;
        let mut rx_enc = tx_enc.subscribe();
        let initial_buffer = initial_buffer.clone();
        let b = initial_buffer.read().unwrap().is_some();
        if b {
            let b = initial_buffer.read().unwrap();

            let bytes = &(b.as_ref().unwrap().len() as u32).to_be_bytes();
            println!("Sending (initial) {bytes:?}");
            socket
                .write_all(bytes)
                .await
                .expect("Could not send size of buffer");

            socket
                .write_all(&b.as_ref().unwrap())
                .await
                .expect("Could not send initial frame");
        }

        let tx = tx_next.clone();
        println!("New connection from {addr}");
        tokio::spawn(async move {
            loop {
                // Tell the encoder that we are ready for a new frame
                tx.send(()).await.unwrap();

                // Get the next frame from the buffer
                let buf = match rx_enc.recv().await {
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(v)) => {
                        println!("Lagged: {v}");
                        continue;
                    }
                    Ok(buf) => buf,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };

                // Send the frame to the tcp listener
                if initial_buffer.read().unwrap().is_none() {
                    let mut b = initial_buffer.write().unwrap();
                    *b = Some(buf.clone());
                }
                let bytes = &(buf.len() as u32).to_be_bytes();
                println!("Sending {bytes:?}");
                socket
                    .write_all(bytes)
                    .await
                    .expect("Could not send size of buffer");

                socket.flush().await.unwrap();
                if let Err(e) = socket.write_all(&buf).await {
                    println!("Error detected, disconnecting: {e}");
                    break;
                }
                socket.flush().await.unwrap();
            }
        });
    }
}
