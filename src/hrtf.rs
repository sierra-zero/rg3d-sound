//! Head-Related Transfer Function (HRTF) module. Provides all needed types and methods for HRTF rendering.
//!
//! # Overview
//!
//! HRTF stands for [Head-Related Transfer Function](https://en.wikipedia.org/wiki/Head-related_transfer_function)
//! and can work only with spatial sounds. For each of such sound source after it was processed by HRTF you can
//! defininitely tell from which locationsound came from. In other words HRTF improves perception of sound to
//! the level of real life.
//!
//! # HRIR Spheres
//!
//! This library uses Head-Related Impulse Response (HRIR) spheres to create HRTF spheres. HRTF sphere is a set of
//! points in 3D space which are connected into a mesh forming triangulated sphere. Each point contains spectrum
//! for left and right ears which will be used to modify samples from each spatial sound source to create binaural
//! sound. HRIR spheres can be found [here](https://github.com/mrDIMAS/hrir_sphere_builder/tree/master/hrtf_base/IRCAM)
//!
//! # Usage
//!
//! To use HRTF you need to change default renderer to HRTF renderer like so:
//!
//! ```no_run
//! use rg3d_sound::context::Context;
//! use rg3d_sound::hrtf::{HrtfSphere, HrtfRenderer};
//! use rg3d_sound::renderer::Renderer;
//! use std::path::Path;
//!
//! fn use_hrtf(context: &mut Context) {
//!     // IRC_1002_C.bin is HRIR sphere in binary format, can be any valid HRIR sphere
//!     // from base mentioned above.
//!     let hrtf = HrtfSphere::new(Path::new("IRC_1002_C.bin")).unwrap();
//!
//!     context.set_renderer(Renderer::HrtfRenderer(HrtfRenderer::new(hrtf)));
//! }
//! ```
//!
//! # Performance
//!
//! HRTF is `heavy`. Usually it 4-5 slower than default renderer, this is essential because HRTF requires some heavy
//! math (fast Fourier transform, convolution, etc.). On Ryzen 1700 it takes 400-450 μs (0.4 - 0.45 ms) per source.
//! In most cases this is ok, engine works in separate thread and it has around 100 ms to prepare new portion of
//! samples for output device.
//!
//! # Known problems
//!
//! This renderer still suffers from small audible clicks in very fast moving sounds, clicks sounds more like
//! "buzzing" - it is due the fact that hrtf is different from frame to frame which gives "bumps" in amplitude
//! of signal because of phase shift each impulse response have. This can be fixed by short cross fade between
//! small amount of samples from previous frame with same amount of frames of current as proposed in
//! [here](http://csoundjournal.com/issue9/newHRTFOpcodes.html)
//!
//! Clicks can be reproduced by using clean sine wave of 440 Hz on some source moving around listener.

use rustfft::{
    num_complex::Complex,
    num_traits::Zero,
    FFTplanner,
};
use std::{
    fs::File,
    path::Path,
    io::{
        BufReader,
        Read,
        Error,
    },
};
use rg3d_core::math::{
    get_barycentric_coords,
    vec3::Vec3,
    ray::Ray,
};
use byteorder::{
    ReadBytesExt,
    LittleEndian,
};
use crate::{
    context::{
        DistanceModel,
        Context,
    },
    listener::Listener,
    renderer::render_source_default,
    device,
    source::{
        spatial::SpatialSource,
        SoundSource,
    },
    math::{
        self,
        mat4::Mat4,
    },
};

/// Single point of HRTF sphere. See module docs for more info.
pub struct HrtfPoint {
    /// Position of point in cartesian coordinate space.
    pub pos: Vec3,
    left_hrtf: Vec<Complex<f32>>,
    right_hrtf: Vec<Complex<f32>>,
}

impl HrtfPoint {
    /// Returns shared reference to spectrum for left ear.
    pub fn left_hrtf(&self) -> &[Complex<f32>] {
        &self.left_hrtf
    }

    /// Returns shared reference to spectrum for right ear.
    pub fn right_hrtf(&self) -> &[Complex<f32>] {
        &self.right_hrtf
    }
}

struct Face {
    a: usize,
    b: usize,
    c: usize,
}

/// See module docs.
pub struct HrtfSphere {
    length: usize,
    points: Vec<HrtfPoint>,
    faces: Vec<Face>,
}

/// All possible error that can occur during HRIR sphere loading.
#[derive(Debug)]
pub enum HrtfError {
    /// Io error has occurred (file does not exists, etc.)
    IoError(std::io::Error),

    /// HRIR has sample rate that differs from device sample rate.
    /// Tuple holds pair (current_sample_rate, device_sample_rate)
    /// You should resample HRIR's first and regenerate sphere.
    InvalidSampleRate(u32, u32),

    /// It is not valid HRIR sphere file.
    InvalidFileFormat,

    /// HRIR has invalid length (zero)
    InvalidLength(usize),
}

impl From<std::io::Error> for HrtfError {
    fn from(io_err: Error) -> Self {
        HrtfError::IoError(io_err)
    }
}

fn make_hrtf(mut hrir: Vec<Complex<f32>>, pad_length: usize, planner: &mut FFTplanner<f32>) -> Vec<Complex<f32>> {
    for _ in hrir.len()..pad_length {
        // Pad with zeros to length of context's output buffer.
        hrir.push(Complex::zero());
    }
    let mut hrtf = vec![Complex::zero(); pad_length];
    planner.plan_fft(pad_length).process(hrir.as_mut(), hrtf.as_mut());
    // Smooth
    hrtf
}

fn read_hrir(reader: &mut dyn Read, len: usize) -> Result<Vec<Complex<f32>>, HrtfError> {
    let mut hrir = Vec::with_capacity(len);
    for _ in 0..len {
        hrir.push(Complex::new(reader.read_f32::<LittleEndian>()?, 0.0));
    }
    Ok(hrir)
}

fn read_faces(reader: &mut dyn Read, index_count: usize) -> Result<Vec<Face>, HrtfError> {
    let mut indices = Vec::with_capacity(index_count);
    for _ in 0..index_count {
        indices.push(reader.read_u32::<LittleEndian>()?);
    }
    let faces = indices.chunks(3)
        .map(|f| Face { a: f[0] as usize, b: f[1] as usize, c: f[2] as usize })
        .collect();
    Ok(faces)
}

impl HrtfSphere {
    /// Loads HRIR sphere and creates HRTF sphere from it.
    ///
    /// # Coordinate system
    ///
    /// Hrtf spheres made in `right-handed` coordinate system. This fact can give
    /// weird positioning issues if your application uses `left-handed` coordinate
    /// system. However this can be fixed very easily: just invert z-axis of
    /// listener basis, or use appropriate `set_orientation_XXX` method.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<HrtfSphere, HrtfError> {
        let mut reader = BufReader::new(File::open(path)?);

        let mut magic = [0; 4];
        reader.read_exact(&mut magic)?;
        if magic[0] != b'H' && magic[1] != b'R' && magic[2] != b'I' && magic[3] != b'R' {
            return Err(HrtfError::InvalidFileFormat);
        }

        let sample_rate = reader.read_u32::<LittleEndian>()?;
        if sample_rate != device::SAMPLE_RATE {
            return Err(HrtfError::InvalidSampleRate(sample_rate, device::SAMPLE_RATE));
        }
        let length = reader.read_u32::<LittleEndian>()? as usize;
        if length == 0 {
            return Err(HrtfError::InvalidLength(length));
        }
        let vertex_count = reader.read_u32::<LittleEndian>()? as usize;
        let index_count = reader.read_u32::<LittleEndian>()? as usize;

        let faces = read_faces(&mut reader, index_count)?;

        let mut planner = FFTplanner::new(false);
        let pad_length = Context::HRTF_BLOCK_LEN + length - 1;

        let mut points = Vec::with_capacity(vertex_count);
        for _ in 0..vertex_count {
            let x = reader.read_f32::<LittleEndian>()?;
            let y = reader.read_f32::<LittleEndian>()?;
            let z = reader.read_f32::<LittleEndian>()?;

            let left_hrtf = make_hrtf(read_hrir(&mut reader, length)?, pad_length, &mut planner);
            let right_hrtf = make_hrtf(read_hrir(&mut reader, length)?, pad_length, &mut planner);

            points.push(HrtfPoint {
                pos: Vec3::new(x, y, z),
                left_hrtf,
                right_hrtf,
            });
        }

        Ok(Self {
            points,
            length,
            faces,
        })
    }

    /// Applies specified transform to each point in sphere. Can be used
    /// to rotate or scale sphere. Transform shouldn't have translation
    /// part, otherwise result of bilinear sampling is undefined.
    pub fn transform(&mut self, matrix: Mat4) {
        for pt in self.points.iter_mut() {
            pt.pos = matrix.transform_vector(pt.pos);
        }
    }

    /// Returns shared reference to sphere points array.
    pub fn points(&self) -> &[HrtfPoint] {
        &self.points
    }

    /// Returns mutable reference to sphere points array.
    pub fn points_mut(&mut self) -> &mut [HrtfPoint] {
        &mut self.points
    }

    /// Sampling with bilinear interpolation
    /// http://www02.smt.ufrj.br/~diniz/conf/confi117.pdf
    pub fn sample_bilinear(&self, left_hrtf: &mut Vec<Complex<f32>>, right_hrtf: &mut Vec<Complex<f32>>, dir: Vec3) {
        if let Some(ray) = Ray::from_two_points(&Vec3::ZERO, &dir.scale(10.0)) {
            for face in self.faces.iter() {
                let a = self.points.get(face.a).unwrap();
                let b = self.points.get(face.b).unwrap();
                let c = self.points.get(face.c).unwrap();

                if let Some(p) = ray.triangle_intersection(&[a.pos, b.pos, c.pos]) {
                    let (ka, kb, kc) = get_barycentric_coords(&p, &a.pos, &b.pos, &c.pos);

                    let len = a.left_hrtf.len();

                    left_hrtf.clear();
                    for i in 0..len {
                        left_hrtf.push(
                            a.left_hrtf[i] * ka +
                                b.left_hrtf[i] * kb +
                                c.left_hrtf[i] * kc);
                    }

                    right_hrtf.clear();
                    for i in 0..len {
                        right_hrtf.push(
                            a.right_hrtf[i] * ka +
                                b.right_hrtf[i] * kb +
                                c.right_hrtf[i] * kc);
                    }
                }
            }
        } else {
            // In case if we have degenerated dir vector use first available point as HRTF.
            let pt = self.points.first().unwrap();

            let len = pt.left_hrtf.len();

            left_hrtf.clear();
            for i in 0..len {
                left_hrtf.push(pt.left_hrtf[i])
            }

            right_hrtf.clear();
            for i in 0..len {
                right_hrtf.push(pt.right_hrtf[i])
            }
        }
    }
}

fn copy_replace(prev_samples: &mut Vec<f32>, raw_buffer: &mut [Complex<f32>], segment_len: usize) {
    if prev_samples.len() != segment_len {
        *prev_samples = vec![0.0; segment_len];
    }

    // Copy samples from previous iteration in the beginning of the buffer.
    for (prev_sample, raw_sample) in prev_samples.iter().zip(&mut raw_buffer[..segment_len]) {
        *raw_sample = Complex::new(*prev_sample, 0.0);
    }

    // Replace last samples by samples from end of the buffer for next iteration.
    let last_start = raw_buffer.len() - segment_len;
    for (prev_sample, raw_sample) in prev_samples.iter_mut().zip(&mut raw_buffer[last_start..]) {
        *prev_sample = raw_sample.re;
    }
}

/// Overlap-save convolution. See more info here:
/// https://dsp-nbsphinx.readthedocs.io/en/nbsphinx-experiment/nonrecursive_filters/segmented_convolution.html
///
/// # Notes
///
/// It is much faster that direct convolution (in case for long impulse responses
/// and signals). Check table here:
/// https://ccrma.stanford.edu/~jos/ReviewFourier/FFT_Convolution_vs_Direct.html
///
/// I measured performance and direct convolution was 8-10 times slower than
/// overlap-save convolution with impulse response length of 512 and signal length
/// of 3545 samples.
fn convolve_overlap_save(in_buffer: &mut [Complex<f32>],
                         out_buffer: &mut [Complex<f32>],
                         hrtf: &[Complex<f32>],
                         hrtf_len: usize,
                         prev_samples: &mut Vec<f32>,
                         fft: &mut FFTplanner<f32>,
                         ifft: &mut FFTplanner<f32>)
{
    assert_eq!(hrtf.len(), in_buffer.len());

    copy_replace(prev_samples, in_buffer, hrtf_len);

    fft.plan_fft(in_buffer.len()).process(in_buffer, out_buffer);

    // Multiply HRIR and input signal in frequency domain.
    for (s, h) in out_buffer.iter_mut().zip(hrtf.iter()) {
        *s *= *h;
    }

    ifft.plan_fft(in_buffer.len()).process(out_buffer, in_buffer);
}

fn get_pad_len(hrtf_len: usize) -> usize {
    // Total length for each temporary buffer.
    // The value defined by overlap-add convolution method:
    //
    // pad_length = M + N - 1,
    //
    // where M - signal length, N - hrtf length
    Context::HRTF_BLOCK_LEN + hrtf_len - 1
}

/// See module docs.
pub struct HrtfRenderer {
    hrtf_sphere: HrtfSphere,
    left_in_buffer: Vec<Complex<f32>>,
    right_in_buffer: Vec<Complex<f32>>,
    left_out_buffer: Vec<Complex<f32>>,
    right_out_buffer: Vec<Complex<f32>>,
    fft: FFTplanner<f32>,
    ifft: FFTplanner<f32>,
    left_hrtf: Vec<Complex<f32>>,
    right_hrtf: Vec<Complex<f32>>,
}

pub(in crate) fn get_raw_samples(source: &mut SpatialSource, left: &mut [Complex<f32>], right: &mut [Complex<f32>], offset: usize) {
    assert_eq!(left.len(), right.len());

    for ((left, right), &(raw_left, _)) in left.iter_mut().zip(right.iter_mut()).zip(&source.generic().frame_samples()[offset..]) {
        // Ignore all channels except left. Only mono sounds can be processed by HRTF.
        let sample = Complex::new(raw_left, 0.0);
        *left = sample;
        *right = sample;
    }
}

fn is_pow2(x: usize) -> bool {
    (x & (x - 1)) == 0
}

impl HrtfRenderer {
    /// Creates new HRTF renderer using specified HRTF sphere. See module docs for more info.
    pub fn new(hrtf_sphere: HrtfSphere) -> Self {
        let pad_length = get_pad_len(hrtf_sphere.length);

        // Acquire default hrtf's for left and right channels.
        let pt = hrtf_sphere.points.first().unwrap();
        let left_hrtf = pt.left_hrtf.clone();
        let right_hrtf = pt.right_hrtf.clone();

        Self {
            hrtf_sphere,
            left_in_buffer: vec![Complex::zero(); pad_length],
            right_in_buffer: vec![Complex::zero(); pad_length],
            left_out_buffer: vec![Complex::zero(); pad_length],
            right_out_buffer: vec![Complex::zero(); pad_length],
            fft: FFTplanner::new(false),
            ifft: FFTplanner::new(true),
            left_hrtf,
            right_hrtf,
        }
    }

    pub(in crate) fn render_source(&mut self,
                                   source: &mut SoundSource,
                                   listener: &Listener,
                                   distance_model: DistanceModel,
                                   out_buf: &mut [(f32, f32)],
    ) {
        match source {
            SoundSource::Generic(_) => {
                render_source_default(source, listener, distance_model, out_buf)
            }
            SoundSource::Spatial(spatial) => {
                // Still very unoptimal and heavy. TODO: Optimize.
                let pad_length = get_pad_len(self.hrtf_sphere.length);

                // TODO: Remove this warning when there will be ability to control output buffer length
                //       from context.
                if !is_pow2(pad_length) {
                    println!("rg3d-sound PERFORMANCE WARNING: Hrtf pad length is not power of two, performance will be ~2 times worse.")
                }

                // Overlap-save convolution with HRTF interpolation.
                // It divides given output buffer into N parts, fetches samples from source
                // performs convolution and writes processed samples to output buffer. Output
                // buffer divided into parts because of HRTF interpolation which significantly
                // reduces distortion in output signal.
                let new_sampling_vector = spatial.get_sampling_vector(listener);
                let new_distance_gain = spatial.get_distance_gain(listener, distance_model);
                for step in 0..Context::HRTF_INTERPOLATION_STEPS {
                    let next = step + 1;
                    let out = &mut out_buf[(step * Context::HRTF_BLOCK_LEN)..(next * Context::HRTF_BLOCK_LEN)];

                    let t = next as f32 / Context::HRTF_INTERPOLATION_STEPS as f32;
                    let sampling_vector = spatial.prev_sampling_vector.lerp(&new_sampling_vector, t);
                    self.hrtf_sphere.sample_bilinear(&mut self.left_hrtf, &mut self.right_hrtf, sampling_vector);

                    let hrtf_len = self.hrtf_sphere.length - 1;

                    get_raw_samples(spatial, &mut self.left_in_buffer[hrtf_len..],
                                    &mut self.right_in_buffer[hrtf_len..], step * Context::HRTF_BLOCK_LEN);

                    convolve_overlap_save(&mut self.left_in_buffer, &mut self.left_out_buffer,
                                          &self.left_hrtf, hrtf_len, &mut spatial.prev_left_samples,
                                          &mut self.fft, &mut self.ifft);

                    convolve_overlap_save(&mut self.right_in_buffer, &mut self.right_out_buffer,
                                          &self.right_hrtf, hrtf_len, &mut spatial.prev_right_samples,
                                          &mut self.fft, &mut self.ifft);

                    // Mix samples into output buffer with rescaling and apply distance gain.
                    let distance_gain = math::lerpf(spatial.prev_distance_gain.unwrap_or(new_distance_gain), new_distance_gain, t);
                    let k = distance_gain / (pad_length as f32);

                    let left_payload = &self.left_in_buffer[hrtf_len..];
                    let right_payload = &self.right_in_buffer[hrtf_len..];
                    for ((out_left, out_right), (processed_left, processed_right))
                    in out.iter_mut().zip(left_payload.iter().zip(right_payload)) {
                        *out_left += processed_left.re * k;
                        *out_right += processed_right.re * k;
                    }
                }
                spatial.prev_sampling_vector = new_sampling_vector;
                spatial.prev_distance_gain = Some(new_distance_gain);
            }
        }
    }
}