use std::{
    path::Path,
    time::{
        self,
        Duration,
    },
    thread,
};
use rg3d_sound::{
    hrtf::{
        HrtfRenderer,
        HrtfSphere,
    },
    context::Context,
    buffer::{
        DataSource,
        SoundBuffer
    },
    renderer::Renderer,
    effects::{
        reverb::Reverb,
        Effect
    },
    source::{
        SoundSource,
        spatial::SpatialSourceBuilder,
        generic::GenericSourceBuilder,
        Status
    },
    math::{
        mat4::Mat4,
        vec3::Vec3,
        quat::Quat,
    },
};

fn main() {
    let hrtf = HrtfSphere::new(Path::new("examples/data/IRC_1002_C.bin")).unwrap();

    // Initialize new sound context with default output device.
    let context = Context::new().unwrap();

    // Set HRTF renderer instead of default for binaural sound.
    context.lock()
        .unwrap()
        .set_renderer(Renderer::HrtfRenderer(HrtfRenderer::new(hrtf)));

    // Create reverb effect and set its decay time.
    let mut reverb = Reverb::new();
    reverb.set_decay_time(Duration::from_secs_f32(10.0));
    context.lock()
        .unwrap()
        .add_effect(Effect::Reverb(reverb));

    // Create some sounds.
    let sound_buffer = SoundBuffer::new_generic(DataSource::from_file("examples/data/door_open.wav").unwrap()).unwrap();
    let source = SpatialSourceBuilder::new(
        GenericSourceBuilder::new(sound_buffer)
            .with_status(Status::Playing)
            .build()
            .unwrap())
        .build_source();
    context.lock()
        .unwrap()
        .add_source(source);

    let sound_buffer = SoundBuffer::new_generic(DataSource::from_file("examples/data/drop.wav").unwrap()).unwrap();
    let source = SpatialSourceBuilder::new(
        GenericSourceBuilder::new(sound_buffer)
            .with_status(Status::Playing)
            .with_looping(true)
            .build()
            .unwrap())
        .build_source();
    let source_handle = context.lock()
        .unwrap()
        .add_source(source);

    // Move sound around listener for some time.
    let start_time = time::Instant::now();
    let mut angle = 0.0f32;
    while (time::Instant::now() - start_time).as_secs() < 360 {
        {
            let mut context = context.lock().unwrap();

            if let SoundSource::Spatial(sound) = context.get_source_mut(source_handle) {
                let axis = Vec3::new(0.0, 1.0, 0.0);
                let rotation_matrix = Mat4::from_quat(Quat::from_axis_angle(axis, angle.to_radians()));
                sound.set_position(&rotation_matrix.transform_vector(Vec3::new(0.0, 0.0, 3.0)));
            }

            angle += 1.6;

            println!("sound render time {} ms", context.full_render_time() * 1000.0);
        }

        // Limit rate of context updates.
        thread::sleep(Duration::from_millis(100));
    }
}