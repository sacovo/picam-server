use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::{cell::RefCell, collections::VecDeque, time::Instant};

use v4l2r::{
    device::{
        poller::PollError,
        queue::{
            direction::Capture,
            dqbuf::DqBuffer,
            generic::{GenericBufferHandles, GenericQBuffer, GenericSupportedMemoryType},
            handles_provider::MmapProvider,
            OutputQueueable,
        },
    },
    encoder::*,
    memory::{MmapHandle, UserPtrHandle},
    Format,
};

use anyhow::ensure;
use clap::{App, Arg};

fn main() {
    env_logger::init();

    let matches = App::new("V4L2 stateful encoder")
        .arg(
            Arg::with_name("num_frames")
                .long("stop_after")
                .takes_value(true)
                .help("Stop after encoding a given number of buffers"),
        )
        .arg(
            Arg::with_name("device")
                .required(true)
                .help("Path to the vicodec device file"),
        )
        .arg(
            Arg::with_name("frame_size")
                .long("frame_size")
                .required(false)
                .takes_value(true)
                .default_value("640x480")
                .help("Size of the frames to encode (e.g. \"640x480\")"),
        )
        .arg(
            Arg::with_name("output_file")
                .long("save")
                .required(false)
                .takes_value(true)
                .help("Save the encoded stream to a file"),
        )
        .arg(
            Arg::with_name("output_mem")
                .long("output_mem")
                .required(false)
                .takes_value(true)
                .default_value("mmap")
                .help("Type of memory to use for the OUTPUT queue (mmap, user or dmabuf)"),
        )
        .get_matches();

    let device_path = matches.value_of("device").unwrap_or("/dev/video0");

    let mut stop_after = match clap::value_t!(matches.value_of("num_frames"), usize) {
        Ok(v) => Some(v),
        Err(e) if e.kind == clap::ErrorKind::ArgumentNotFound => None,
        Err(e) => panic!("Invalid value for stop_after: {}", e),
    };

    let frame_size = matches
        .value_of("frame_size")
        .map(|s| {
            const ERROR_MSG: &str = "Invalid parameter for frame_size";
            let split: Vec<&str> = s.split('x').collect();
            if split.len() != 2 {
                panic!("{}", ERROR_MSG);
            }
            let width: usize = split[0].parse().expect(ERROR_MSG);
            let height: usize = split[1].parse().expect(ERROR_MSG);

            (width, height)
        })
        .unwrap();

    let mut output_file = matches
        .value_of("output_file")
        .map(|s| File::create(s).expect("Invalid output file specified."));

    let output_mem = match matches.value_of("output_mem") {
        Some("mmap") => GenericSupportedMemoryType::Mmap,
        Some("user") => GenericSupportedMemoryType::UserPtr,
        Some("dmabuf") => GenericSupportedMemoryType::DmaBuf,
        _ => panic!("Invalid value for output_mem"),
    };

    let lets_quit = Arc::new(AtomicBool::new(false));
    // Setup the Ctrl+c handler.
    {
        let lets_quit_handler = lets_quit.clone();
        ctrlc::set_handler(move || {
            lets_quit_handler.store(true, Ordering::SeqCst);
        })
        .expect("Failed to set Ctrl-C handler.");
    }

    let encoder = Encoder::open(Path::new(&device_path))
        .expect("Failed to open device")
        .set_capture_format(|f| {
            let format: Format = f
                .set_pixelformat(b"H264")
                .set_size(frame_size.0, frame_size.1)
                .apply()?;

            // ensure!(
            //     format.pixelformat == b"H264".into(),
            //     "FWHT format not supported ({0})",
            //     format.pixelformat
            // );
            println!("Format used: {0}", format.pixelformat);

            Ok(())
        })
        .expect("Failed to set capture format")
        .set_output_format(|f| {
            let format: Format = f
                .set_pixelformat(b"RGB3")
                .set_size(frame_size.0, frame_size.1)
                .apply()?;

            ensure!(
                format.pixelformat == b"RGB3".into(),
                "RGB3 format not supported {0}",
                format.pixelformat,
            );
            ensure!(
                format.width as usize == frame_size.0 && format.height as usize == frame_size.1,
                "Output frame resolution not supported"
            );

            Ok(())
        })
        .expect("Failed to set output format");

    let output_format = encoder
        .get_output_format()
        .expect("Failed to get output format");
    println!("Adjusted output format: {:?}", output_format);

    let capture_format = encoder
        .get_capture_format()
        .expect("Failed to get capture format");
    println!("Adjusted capture format: {:?}", capture_format);

    println!(
        "Configured encoder for {}x{} ({} bytes per line)",
        output_format.width, output_format.height, output_format.plane_fmt[0].bytesperline
    );

    let mut frame_gen = FrameGenerator::new(
        output_format.width as usize,
        output_format.height as usize,
        output_format.plane_fmt[0].bytesperline as usize,
    )
    .expect("Failed to create frame generator");

    println!("Generated frame generator");

    const NUM_BUFFERS: usize = 2;

    let free_buffers: Option<VecDeque<_>> = match output_mem {
        GenericSupportedMemoryType::Mmap | GenericSupportedMemoryType::DmaBuf => None,
        GenericSupportedMemoryType::UserPtr => Some(
            std::iter::repeat(vec![0u8; output_format.plane_fmt[0].sizeimage as usize])
                .take(NUM_BUFFERS)
                .collect(),
        ),
    };
    println!("allocated buffers");
    let free_buffers = RefCell::new(free_buffers);

    let dmabufs: Option<VecDeque<_>> = match output_mem {
        GenericSupportedMemoryType::Mmap | GenericSupportedMemoryType::UserPtr => None,
        GenericSupportedMemoryType::DmaBuf => Some(
            export_dmabufs(&output_format, NUM_BUFFERS)
                .unwrap()
                .into_iter()
                .collect(),
        ),
    };
    let dmabufs = RefCell::new(dmabufs);

    let input_done_cb = |buffer: CompletedOutputBuffer<GenericBufferHandles>| {
        let handles = match buffer {
            CompletedOutputBuffer::Dequeued(mut buf) => buf.take_handles().unwrap(),
            CompletedOutputBuffer::Canceled(buf) => buf.plane_handles,
        };
        match handles {
            // We have nothing to do for MMAP buffers.
            GenericBufferHandles::Mmap(_) => {}
            // For user-allocated memory, return the buffer to the free list.
            GenericBufferHandles::User(mut u) => {
                free_buffers
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .push_back(u.remove(0).0);
            }
            GenericBufferHandles::DmaBuf(d) => {
                dmabufs.borrow_mut().as_mut().unwrap().push_back(d);
            }
        };
    };

    println!("Create buffers");

    let mut total_size = 0usize;
    let start_time = Instant::now();
    let poll_count_reader = Arc::new(AtomicUsize::new(0));
    let poll_count_writer = Arc::clone(&poll_count_reader);
    let mut frame_counter = 0usize;
    let output_ready_cb = move |cap_dqbuf: DqBuffer<Capture, Vec<MmapHandle>>| {
        println!("output ready cb");
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
        print!(
            "\rEncoded buffer {:#5}, index: {:#2}), bytes used:{:#6} total encoded size:{:#8} fps: {:#5.2} ppf: {:#4.2}" ,
            cap_dqbuf.data.sequence(),
            cap_dqbuf.data.index(),
            bytes_used,
            total_size,
            fps,
            ppf,
        );
        io::stdout().flush().unwrap();

        if let Some(ref mut output) = output_file {
            let mapping = cap_dqbuf
                .get_plane_mapping(0)
                .expect("Failed to map capture buffer");
            output
                .write_all(mapping.as_ref())
                .expect("Error while writing output data");
        }
    };

    println!("Starting encoder");

    let mut encoder = encoder
        .allocate_output_buffers_generic::<GenericBufferHandles>(output_mem, NUM_BUFFERS)
        .expect("Failed to allocate OUTPUT buffers")
        .allocate_capture_buffers(NUM_BUFFERS, MmapProvider::new(&capture_format))
        .expect("Failed to allocate CAPTURE buffers")
        .set_poll_counter(poll_count_writer)
        .start(input_done_cb, output_ready_cb)
        .expect("Failed to start encoder");

    println!("Started encoder");

    while !lets_quit.load(Ordering::SeqCst) {
        println!("Frame");
        if let Some(max_cpt) = &mut stop_after {
            if *max_cpt == 0 {
                break;
            }
            *max_cpt -= 1;
        }

        let v4l2_buffer = match encoder.get_buffer() {
            Ok(buffer) => buffer,
            // If we got interrupted while waiting for a buffer, just exit normally.
            Err(GetBufferError::PollError(PollError::EPollWait(nix::errno::Errno::EINTR))) => break,
            Err(e) => panic!("{}", e),
        };
        println!("Read buffer");
        let bytes_used = frame_gen.frame_size();
        println!("Bytes used: {}", bytes_used);
        match v4l2_buffer {
            GenericQBuffer::Mmap(buf) => {
                let mut mapping = buf
                    .get_plane_mapping(0)
                    .expect("Failed to get MMAP mapping");
                frame_gen
                    .next_frame(&mut mapping)
                    .expect("Failed to generate frame");
                buf.queue(&[bytes_used])
                    .expect("Failed to queue input frame");
            }
            GenericQBuffer::User(buf) => {
                let mut buffer = free_buffers
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .pop_front()
                    .expect("No backing buffer to bind");
                frame_gen
                    .next_frame(&mut buffer)
                    .expect("Failed to generate frame");
                buf.queue_with_handles(
                    GenericBufferHandles::from(vec![UserPtrHandle::from(buffer)]),
                    &[bytes_used],
                )
                .expect("Failed to queue input frame");
            }
            GenericQBuffer::DmaBuf(buf) => {
                let buffer = dmabufs
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .pop_front()
                    .expect("No backing dmabuf to bind");
                let mut mapping = buffer[0].map().unwrap();
                frame_gen
                    .next_frame(&mut mapping)
                    .expect("Failed to generate frame");
                buf.queue_with_handles(GenericBufferHandles::from(buffer), &[bytes_used])
                    .expect("Failed to queue input frame");
            }
        }
    }

    encoder.stop().unwrap();

    // Insert new line since we were overwriting the same one
    println!();

    if output_mem == GenericSupportedMemoryType::UserPtr {
        // All the OUTPUT buffers should have been returned
        assert_eq!(free_buffers.borrow().as_ref().unwrap().len(), NUM_BUFFERS);
    }
}
use thiserror::Error;
#[derive(Debug, Error)]
pub enum NewFrameGeneratorError {
    #[error("invalid stride")]
    InvalidStride,
}

#[derive(Debug, Error)]
pub enum GenerateFrameError {
    #[error("provided buffer is too small")]
    BufferTooSmall,
}

pub struct FrameGenerator {
    width: usize,
    height: usize,
    stride: usize,
    step: u32,
}

impl FrameGenerator {
    pub fn new(width: usize, height: usize, stride: usize) -> Result<Self, NewFrameGeneratorError> {
        if stride < width * 3 {
            return Err(NewFrameGeneratorError::InvalidStride);
        }

        Ok(FrameGenerator {
            width,
            height,
            stride,
            step: 0,
        })
    }

    pub fn frame_size(&self) -> usize {
        self.stride * self.height
    }

    pub fn next_frame<S: AsMut<[u8]>>(&mut self, frame: &mut S) -> Result<(), GenerateFrameError> {
        let frame = frame.as_mut();

        if frame.len() < self.frame_size() {
            return Err(GenerateFrameError::BufferTooSmall);
        }

        self.gen_pattern(frame);
        self.step = self.step.wrapping_add(1);

        Ok(())
    }

    fn gen_pattern(&mut self, frame: &mut [u8]) {
        frame
            .chunks_exact_mut(self.stride)
            .map(|l| &mut l[0..self.width * 3])
            .enumerate()
            .for_each(|(y, line)| {
                line.chunks_exact_mut(3).enumerate().for_each(|(x, pixel)| {
                    let rgba = self.step.wrapping_add((x ^ y) as u32).to_le_bytes();
                    pixel[0] = rgba[0];
                    pixel[1] = rgba[1];
                    pixel[2] = rgba[2];
                });
            });
    }
}

use nix::libc::off_t;

use nix::{
    sys::memfd::{memfd_create, MemFdCreateFlag},
    unistd::ftruncate,
};
use v4l2r::memory::DmaBufHandle;

pub fn export_dmabufs(
    format: &Format,
    nb_buffers: usize,
) -> nix::Result<Vec<Vec<DmaBufHandle<File>>>> {
    let fds: Vec<Vec<DmaBufHandle<File>>> = (0..nb_buffers)
        .map(|_| {
            format
                .plane_fmt
                .iter()
                .map(|plane| {
                    memfd_create(c"memfd buffer", MemFdCreateFlag::MFD_ALLOW_SEALING)
                        .and_then(|fd| ftruncate(&fd, plane.sizeimage as off_t).map(|_| fd))
                        .map(|fd| DmaBufHandle::from(File::from(fd)))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(fds)
}

#[cfg(test)]
mod tests {
    use v4l2r::{memory::DmaBufSource, Format, PixelFormat, PlaneLayout};

    use super::export_dmabufs;

    #[test]
    fn test_export_dmabufs() {
        const WIDTH: u32 = 640;
        const HEIGHT: u32 = 480;
        const BYTES_PER_LINE: u32 = WIDTH;
        const SIZE_IMAGE_Y: u32 = BYTES_PER_LINE * HEIGHT;
        const SIZE_IMAGE_UV: u32 = BYTES_PER_LINE * HEIGHT / 2;

        let format = Format {
            width: WIDTH,
            height: HEIGHT,
            pixelformat: PixelFormat::from_fourcc(b"NV12"),
            plane_fmt: vec![
                PlaneLayout {
                    sizeimage: SIZE_IMAGE_Y,
                    bytesperline: BYTES_PER_LINE,
                },
                PlaneLayout {
                    sizeimage: SIZE_IMAGE_UV,
                    bytesperline: BYTES_PER_LINE,
                },
            ],
        };

        let dmabufs = export_dmabufs(&format, 4).unwrap();
        assert_eq!(dmabufs.len(), 4);
        for buf in dmabufs {
            assert_eq!(buf.len(), 2);
            assert!(buf[0].0.len() >= SIZE_IMAGE_Y as u64);
            assert!(buf[1].0.len() >= SIZE_IMAGE_UV as u64);
        }
    }
}
