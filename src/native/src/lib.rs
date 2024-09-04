use core::num;
use std::borrow::Cow;
use std::fs::File;
use std::mem::size_of;
use std::io::BufReader;
use std::os::raw::c_char;
use std::mem;
use std::ffi::{c_void, CStr};
use std::path::Path;
use std::slice::from_raw_parts;

use exr::error::UnitResult;
use exr::prelude::*;
use itertools::{izip, multizip};

macro_rules! unwrap_or_return_err {
    ($e: expr) => {
        match $e {
            Ok(e) => e,
            Err(err) => {
                println!("{err}");
                return 1;
            }
        }
    };
}

#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum ExrEncoding {
    Uncompressed = 0,
    RLE = 1,
    ZIP1 = 2,
    ZIP16 = 3,
    PIZ = 4,
}

#[derive(Clone, Copy, Debug)]
#[repr(i32)]
pub enum ExrPixelFormat
{
    Unknown = -1,
    U32 = 0,
    F16 = 1,
    F32 = 2,
    RGBF32 = 3
}

impl From<SampleType> for ExrPixelFormat {
    fn from(value: SampleType) -> Self {
        match value {
            SampleType::F16 => ExrPixelFormat::F16,
            SampleType::F32 => ExrPixelFormat::F32,
            SampleType::U32 => ExrPixelFormat::U32,
        }
    }
}

#[no_mangle]
pub unsafe extern fn write_texture(path: *const c_char, width: i32, height: i32, format: ExrPixelFormat, encoding: ExrEncoding, data: *const Sample) -> i32 {
    let path = Path::new(unwrap_or_return_err!(CStr::from_ptr(path).to_str()));

    let result = match format {
        ExrPixelFormat::U32 => {
            let ptr = data as *const u32;
            let array = from_raw_parts(ptr, (width * height * 4) as usize);
            write_exr(path, array, width as usize, height as usize, encoding)
        },
        ExrPixelFormat::F16 => {
            let ptr = data as *const f16;
            let array = from_raw_parts(ptr, (width * height * 4) as usize);
            write_exr(path, array, width as usize, height as usize, encoding)
        },
        ExrPixelFormat::F32 => {
            let ptr = data as *const f32;
            let array = from_raw_parts(ptr, (width * height * 4) as usize);
            write_exr(path, array, width as usize, height as usize, encoding)
        }
        _ => {
            // Unknown
            Err(Error::NotSupported(Cow::Owned(format!("Encoding {encoding:?} not supported"))))
        }
    };

    match result {
        Ok(()) => 0,
        Err(err) => {
            println!("{err}");
            1
        },
    }
}

fn write_exr<T: IntoSample>(path: impl AsRef<Path>, array: &[T], width: usize, height: usize, encoding: ExrEncoding) -> UnitResult {
    let channels = SpecificChannels::rgba(|Vec2(x,y)| (
        array[(y * width + x) * 4 + 0],
        array[(y * width + x) * 4 + 1],
        array[(y * width + x) * 4 + 2],
        array[(y * width + x) * 4 + 3]
    ));
    let encoding = match encoding  {
        // See encoding presets but expanded here to make clearer the
        // encoding compression
        ExrEncoding::Uncompressed => Encoding {
            compression: Compression::Uncompressed,
            blocks: Blocks::ScanLines, // longest lines, faster memcpy
            line_order: LineOrder::Increasing // presumably fastest?
        },
        ExrEncoding::RLE => Encoding {
            compression: Compression::RLE,
            blocks: Blocks::Tiles(Vec2(64, 64)), // optimize for RLE compression
            line_order: LineOrder::Unspecified
        },
        ExrEncoding::ZIP16 => Encoding {
            compression: Compression::ZIP16,
            blocks: Blocks::ScanLines, // largest possible, but also with high probability of parallel workers
            line_order: LineOrder::Increasing
        },
        ExrEncoding::PIZ => Encoding {
            compression: Compression::PIZ,
            blocks: Blocks::Tiles(Vec2(256, 256)),
            line_order: LineOrder::Unspecified
        },
        ExrEncoding::ZIP1 => Encoding {
            compression: Compression::ZIP1,
            blocks: Blocks::ScanLines,
            line_order: LineOrder::Increasing
        }
    };
    let layer = Layer::new(
        Vec2(width, height),
        LayerAttributes::named("first layer"),
        encoding,
        channels
    );
    Image::from_layer(layer).write().to_file(path)
}

#[no_mangle]
pub unsafe extern fn load_from_path(path: *const c_char, width: *mut u32, height: *mut u32, num_channels: *mut u32, format: *mut ExrPixelFormat, data: *mut *mut c_void) -> i32 {
    let path = Path::new(unwrap_or_return_err!(CStr::from_ptr(path).to_str()));

    *data = unwrap_or_return_err!(load(path, &mut *width, &mut *height, &mut *num_channels, &mut *format));

    0
}


fn load(path: &Path, width: &mut u32, height: &mut u32, num_channels: &mut u32, format: &mut ExrPixelFormat) -> anyhow::Result<*mut c_void> {
    let extension = match path
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some(extension) => extension,
        None => ""
    };

    match extension {
        "hdr" => {
            let f = File::open(path)?;
            let r = BufReader::new(f);
            let mut image = radiant::load(r)?;

            *width = image.width as u32;
            *height = image.height as u32;
            *num_channels = 3;
            *format = ExrPixelFormat::RGBF32;

            let ptr = image.data.as_mut_ptr();
            mem::forget(image);

            Ok(ptr as *mut c_void)
        },
        _ => {
            match MetaData::read_from_file(path, false) {
                Ok(meta) => {
                    let size = meta.headers[0].layer_size;
                    *width = size.0 as u32;
                    *height = size.1 as u32;

                    let sample_type = meta.headers[0].channels.uniform_sample_type;

                    match sample_type {
                        Some(sample_type) => {
                            *format = sample_type.into();
                            Ok(match sample_type {
                                SampleType::F16 => {
                                    let (mut image, channels) = load_exr_f16(path, &meta)?;
                                    *num_channels = channels as u32;
                                    let ret = image.as_mut_ptr() as *mut c_void;
                                    mem::forget(image);
                                    ret
                                },
                                SampleType::F32 => {
                                    let (mut image, channels) = load_exr_f32(path, &meta)?;
                                    *num_channels = channels as u32;
                                    let ret = image.as_mut_ptr() as *mut c_void;
                                    mem::forget(image);
                                    ret
                                },
                                SampleType::U32 => {
                                    let (mut image, channels) = load_exr_u32(path, &meta)?;
                                    *num_channels = channels as u32;
                                    let ret = image.as_mut_ptr() as *mut c_void;
                                    mem::forget(image);
                                    ret
                                },
                            })
                        },
                        None => {
                            *format = ExrPixelFormat::Unknown;
                            *num_channels = 0;
                            Err(Error::NotSupported("Sample type".into()).into())
                        }
                    }
                },
                Err(err) => {
                    *width = 0;
                    *height = 0;
                    *num_channels = 0;
                    *format = ExrPixelFormat::Unknown;
                    Err(err.into())
                }
            }
        }
    }
}

fn load_exr_f16(path: &Path, meta: &MetaData) -> Result<(Vec<f16>, usize)> {
    let image = read_first_flat_layer_from_file(path)?;
    let w = meta.headers[0].layer_size.0;
    let h = meta.headers[0].layer_size.1;
    let num_channels = image.layer_data.channel_data.list.len();
    let mut flat_data = vec![
        f16::from_f32(0.); 
        w * h * num_channels
    ];

    for i in 0 .. w*h {
        for (channel_index, channel) in image.layer_data.channel_data.list.iter().enumerate() {
            if let FlatSamples::F16(samples) = &channel.sample_data {
                    flat_data[i * num_channels + channel_index] = samples[i]
            }else{
                unreachable!()
            }
        }
    }

    Ok((flat_data, num_channels))
}

fn load_exr_f32(path: &Path, meta: &MetaData) -> Result<(Vec<f32>, usize)> {
    let image = read_first_flat_layer_from_file(path)?;
    let w = meta.headers[0].layer_size.0;
    let h = meta.headers[0].layer_size.1;
    let num_channels = image.layer_data.channel_data.list.len();
    let mut flat_data = vec![0.;  w * h * num_channels];

    for i in 0 .. w*h {
        for (channel_index, channel) in image.layer_data.channel_data.list.iter().enumerate() {
            if let FlatSamples::F32(samples) = &channel.sample_data {
                    flat_data[i * num_channels + channel_index] = samples[i]
            }else{
                unreachable!()
            }
        }
    }

    Ok((flat_data, num_channels))
}

fn load_exr_u32(path: &Path, meta: &MetaData) -> Result<(Vec<u32>, usize)> {
    let image = read_first_flat_layer_from_file(path)?;
    let w = meta.headers[0].layer_size.0;
    let h = meta.headers[0].layer_size.1;
    let num_channels = image.layer_data.channel_data.list.len();
    let mut flat_data = vec![0;  w * h * num_channels];

    for i in 0 .. w*h {
        for (channel_index, channel) in image.layer_data.channel_data.list.iter().enumerate() {
            if let FlatSamples::U32(samples) = &channel.sample_data {
                    flat_data[i * num_channels + channel_index] = samples[i]
            }else{
                unreachable!()
            }
        }
    }

    Ok((flat_data, num_channels))
}

// The use of exr::Sample is stored in memory at compile time according to the largest element, f32

// fn load_exr(path: &str) -> usize {
//     let image = read_first_rgba_layer_from_file(
//         path,
//         |resolution, _| {
//             let default_pixel: [Sample;4] = [Sample::default(), Sample::default(), Sample::default(), Sample::default()];
//             let empty_line = vec![ default_pixel; resolution.width() ];
//             let empty_image = vec![ empty_line; resolution.height() ];
//             empty_image
//         },
//         |pixel_vector, position, (r,g,b, a): (Sample, Sample, Sample, Sample)| {
//             pixel_vector[position.y()][position.x()] = [r, g, b, a]
//         },

//     ).unwrap();

//     let mut pixel = image.layer_data.channel_data.pixels.into_iter().flatten().collect::<Vec<[Sample;4]>>();
//     let ptr = pixel.as_mut_ptr();
//     mem::forget(pixel);

//     return ptr as usize;
// }

#[test]
fn test_depth_image() {
    let path = Path::new("0270_Ocean_Commission_Canyon_NLD_11.Depth.0001.exr");
    let mut width = 0;
    let mut height = 0;
    let mut num_channels = 0;
    let mut format = ExrPixelFormat::Unknown;
    let data = load(path, &mut width, &mut height, &mut num_channels, &mut format).unwrap();
    assert_eq!(num_channels, 1);
}