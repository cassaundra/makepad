use crate::{makepad_derive_widget::*, makepad_draw::*, widget::*, VideoColorFormat};
use std::{cell::RefCell, collections::VecDeque, rc::Rc, time::Instant};

const DEFAULT_FPS_INTERVAL: f64 = 33.0;

live_design! {
    import makepad_draw::shader::std::*;
    import makepad_widgets::theme::*;

    Video = {{Video}} {
        walk:{
            width: 500
            height: 500
        }
        draw_bg: {
            texture y_image: texture2d
            texture uv_image: texture2d
            instance image_scale: vec2(1.0, 1.0)
            instance image_pan: vec2(0.0, 0.0)
            uniform image_alpha: 1.0

            fn yuv_to_rgb(y: float, u: float, v: float) -> vec4 {
                let c = y - 16.0;
                let d = u - 128.0;
                let e = v - 128.0;

                let r = clamp((298.0 * c + 409.0 * e + 128.0) / 65536.0, 0.0, 1.0);
                let g = clamp((298.0 * c - 100.0 * d - 208.0 * e + 128.0) / 65536.0, 0.0, 1.0);
                let b = clamp((298.0 * c + 516.0 * d + 128.0) / 65536.0, 0.0, 1.0);

                return vec4(r, g, b, 1.0);
            }

            fn get_color(self) -> vec4 {
                let y_sample = sample2d(self.y_image, self.pos * self.image_scale + self.image_pan).z;
                let uv_coords = (self.pos * self.image_scale + self.image_pan);
                let uv_sample = sample2d(self.uv_image, uv_coords);

                let u = uv_sample.x;
                let v = uv_sample.y;

                return yuv_to_rgb(y_sample * 255., u * 255., v * 255.);
            }

            fn pixel(self) -> vec4 {
                let color = self.get_color();
                return Pal::premul(vec4(color.xyz, color.w * self.image_alpha))
            }

            shape: Solid,
            fill: Image
        }
    }
}

#[derive(Live)]
pub struct Video {
    // Drawing
    #[live]
    draw_bg: DrawColor,
    #[live]
    walk: Walk,
    #[live]
    layout: Layout,
    #[live]
    scale: f64,

    #[live]
    source: LiveDependency,
    #[rust]
    y_texture: Option<Texture>,
    #[rust]
    uv_texture: Option<Texture>,

    // Playback options
    #[live]
    is_looping: bool,

    // Original video metadata
    #[rust]
    width: usize,
    #[rust]
    height: usize,
    #[rust]
    total_duration: u128,
    #[rust]
    original_frame_rate: usize,
    #[rust]
    color_format: VideoColorFormat,

    // Buffering
    #[rust]
    frames_buffer: RingBuffer,

    // Frame
    #[rust]
    current_frame_index: usize,
    #[rust]
    current_frame_ts: u128,
    #[rust]
    frame_ts_interval: f64,
    #[rust]
    last_update: MyInstant,
    #[rust]
    tick: Timer,
    #[rust]
    accumulated_time: u128,
    #[rust]
    playback_finished: bool,

    // Decoding
    #[rust]
    decoding_threshold: f64,
    #[rust]
    decoding_state: DecodingState,
    #[rust]
    latest_chunk: Option<(u128, u128)>,
    #[rust]
    vec_pool: VecPool,

    #[rust]
    id: LiveId,
}

#[derive(Clone)]
struct VideoFrame {
    y_data: Rc<RefCell<Vec<u32>>>,
    uv_data: Rc<RefCell<Vec<u32>>>,
    timestamp_us: u128,
}

#[derive(Clone, Default, PartialEq, WidgetRef)]
pub struct VideoRef(WidgetRef);

#[derive(Default, PartialEq)]
enum DecodingState {
    #[default]
    NotStarted,
    Idle,
    Decoding,
    Finished,
}

struct MyInstant(Instant);

impl Default for MyInstant {
    fn default() -> Self {
        MyInstant(Instant::now())
    }
}

impl LiveHook for Video {
    fn before_live_design(cx: &mut Cx) {
        register_widget!(cx, Video)
    }

    fn after_new_from_doc(&mut self, cx: &mut Cx) {
        self.id = LiveId::new(cx);
        self.initialize_decoding(cx);
    }
}

#[derive(Clone, WidgetAction)]
pub enum VideoAction {
    None,
}

// TODO:
// - add audio playback
// - determine buffer size based on memory usage: minimal amount of frames to keep in memory for smooth playback considering their size
// - implement a pause/play
// - cleanup resources after playback is finished

impl Widget for Video {
    fn redraw(&mut self, cx: &mut Cx) {
        self.draw_bg
            .draw_vars
            .set_texture(0, self.y_texture.as_ref().unwrap());

        self.draw_bg
            .draw_vars
            .set_texture(1, self.uv_texture.as_ref().unwrap());
        self.draw_bg.redraw(cx);
    }

    fn get_walk(&self) -> Walk {
        self.walk
    }

    fn draw_walk_widget(&mut self, cx: &mut Cx2d, walk: Walk) -> WidgetDraw {
        self.draw_bg.draw_walk(cx, walk);
        WidgetDraw::done()
    }

    fn handle_widget_event_with(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        dispatch_action: &mut dyn FnMut(&mut Cx, WidgetActionItem),
    ) {
        let uid = self.widget_uid();
        self.handle_event_with(cx, event, &mut |cx, action| {
            dispatch_action(cx, WidgetActionItem::new(action.into(), uid));
        });
    }
}

impl Video {
    pub fn handle_event_with(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        _dispatch_action: &mut dyn FnMut(&mut Cx, VideoAction),
    ) {
        // TODO: Check for video id
        if self.tick.is_event(event) {
            self.tick = cx.start_timeout((1.0 / self.original_frame_rate as f64 / 2.0) * 1000.0);

            if self.decoding_state == DecodingState::Finished
                || self.decoding_state == DecodingState::Decoding
                    && self.frames_buffer.data.len() > 5
            {
                self.process_tick(cx);
            }

            if self.should_request_decoding() {
                cx.decode_next_video_chunk(self.id, 30);
                self.decoding_state = DecodingState::Decoding;
            }
        }

        if let Event::VideoDecodingInitialized(event) = event {
            self.width = event.video_width as usize;
            self.height = event.video_height as usize;
            self.original_frame_rate = event.frame_rate;
            self.total_duration = event.duration;
            self.color_format = event.color_format;
            self.frame_ts_interval = 1000000.0 / self.original_frame_rate as f64;

            makepad_error_log::log!(
                "<<<<<<<<<<<<<<< Decoding initialized: \n {}x{}px | {} FPS | Color format: {:?} | Timestamp interval: {:?}",
                self.width,
                self.height,
                self.original_frame_rate,
                self.color_format,
                self.frame_ts_interval
            );

            self.resize_frames_buffer();

            cx.decode_next_video_chunk(self.id, 45);
            self.decoding_state = DecodingState::Decoding;

            self.tick = cx.start_timeout((1.0 / self.original_frame_rate as f64 / 2.0) * 1000.0);
        }

        if let Event::VideoChunkDecoded(_id) = event {
            // makepad_error_log::log!("<<<<<<<<<<<<<<< VideoChunkDecoded Event");
            self.decoding_state = DecodingState::Finished;

            cx.fetch_next_video_frames(self.id, 30);
        }

        // let start = Instant::now();
        if let Event::VideoStream(event) = event {
            makepad_error_log::log!("<<<<<<<<<<<<<<< VideoStream Event");
            let mut cursor = 0;
            let frame_group = &event.frame_group;

            // | Timestamp (8B)  | Y Stride (4B) | UV Stride (4B) | Frame data length (4b) | Pixel Data |
            let metadata_size = 20;

            while cursor < frame_group.len() {
                // might have to update for different endinaess on other platforms
                let timestamp =
                    u64::from_be_bytes(frame_group[cursor..cursor + 8].try_into().unwrap()) as u128;
                let y_stride =
                    u32::from_be_bytes(frame_group[cursor + 8..cursor + 12].try_into().unwrap());
                let uv_stride =
                    u32::from_be_bytes(frame_group[cursor + 12..cursor + 16].try_into().unwrap());
                let frame_length =
                    u32::from_be_bytes(frame_group[cursor + 16..cursor + 20].try_into().unwrap())
                        as usize;

                let frame_data_start = cursor + metadata_size;
                let frame_data_end = frame_data_start + frame_length;

                let pixel_data = &frame_group[frame_data_start..frame_data_end];

                let mut y_data = self.vec_pool.acquire(self.width * self.height);
                let mut uv_data = self.vec_pool.acquire((self.width / 2) * (self.height / 2));

                split_nv12_data(
                    pixel_data,
                    self.width,
                    self.height,
                    y_stride as usize,
                    uv_stride as usize,
                    y_data.as_mut_slice(),
                    uv_data.as_mut_slice(),
                );

                self.frames_buffer.push(VideoFrame {
                    y_data: Rc::new(RefCell::new(y_data)),
                    uv_data: Rc::new(RefCell::new(uv_data)),
                    timestamp_us: timestamp,
                });

                cursor = frame_data_end;
            }

            // let elapsed = start.elapsed();

            // let elapsed_ms = elapsed.as_secs() * 1000 + elapsed.subsec_millis() as u64;
            // makepad_error_log::log!("STREAM EVENT TOOK: {}", elapsed_ms);
        }
    }

    fn should_request_decoding(&self) -> bool {
        match self.decoding_state {
            DecodingState::Decoding => false,
            DecodingState::Finished => self.frames_buffer.data.len() < 10,
            _ => todo!(),
        }
    }

    fn process_tick(&mut self, cx: &mut Cx) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update.0).as_micros();
        self.accumulated_time += elapsed;

        match self.frames_buffer.get() {
            Some(current_frame) => {
                if self.accumulated_time >= current_frame.timestamp_us {
                    self.update_textures(cx, current_frame.y_data, current_frame.uv_data);

                    self.redraw(cx);

                    // if at latest frame, restart
                    if self.current_frame_ts >= self.total_duration {
                        if self.is_looping {
                            self.current_frame_ts = 0;
                        } else {
                            self.playback_finished = true;
                            self.cleanup_decoding(cx);
                        }
                        self.accumulated_time -= current_frame.timestamp_us;
                    } else {
                        self.current_frame_ts =
                            (self.current_frame_ts as f64 + self.frame_ts_interval).ceil() as u128;
                    }
                }

                self.last_update = MyInstant(now);
            }
            None => {
                makepad_error_log::log!("Empty Buffer");
            }
        }
    }

    fn update_textures(
        &mut self,
        cx: &mut Cx,
        y_data: Rc<RefCell<Vec<u32>>>,
        uv_data: Rc<RefCell<Vec<u32>>>,
    ) {
        if let None = self.y_texture {
            self.y_texture = Some(Texture::new(cx));
        }
        if let None = self.uv_texture {
            self.uv_texture = Some(Texture::new(cx));
        }

        let y_texture = self.y_texture.as_mut().unwrap();
        let uv_texture = self.uv_texture.as_mut().unwrap();

        y_texture.set_desc(
            cx,
            TextureDesc {
                format: TextureFormat::ImageBGRA,
                width: Some(self.width),
                height: Some(self.height),
            },
        );

        uv_texture.set_desc(
            cx,
            TextureDesc {
                format: TextureFormat::ImageBGRA,
                width: Some(self.width / 2),
                height: Some(self.height / 2),
            },
        );

        y_texture.swap_image_u32(cx, &mut y_data.borrow_mut());
        uv_texture.swap_image_u32(cx, &mut uv_data.borrow_mut());

        // TODO: simplify and probably remove Rc

        self.vec_pool.release(y_data.replace(Vec::new()));
        self.vec_pool.release(uv_data.replace(Vec::new()));        
    }

    fn initialize_decoding(&self, cx: &mut Cx) {
        match cx.get_dependency(self.source.as_str()) {
            Ok(data) => {
                cx.initialize_video_decoding(self.id, data, 100);
            }
            Err(_e) => {
                todo!()
            }
        }
    }

    fn resize_frames_buffer(&mut self) {
        let chunk_duration_seconds = CHUNK_DURATION_US as f64 / 1_000_000.0;
        let estimated_frames_per_chunk =
            (self.original_frame_rate as f64 * chunk_duration_seconds).ceil() as usize;

        self.frames_buffer.capacity = (estimated_frames_per_chunk as f64 * 1.2).ceil() as usize;
    }

    fn cleanup_decoding(&mut self, cx: &mut Cx) {
        //cx.cleanup_video_decoding(self.id);
        //cx.cancel_timeout
    }
}

// TODO: dynamically calculate this based on frame rate and size
const CHUNK_DURATION_US: u128 = 1_000_000 / 2;

struct RingBuffer {
    data: VecDeque<VideoFrame>,
    last_added_index: Option<usize>,
    capacity: usize,
}

impl RingBuffer {
    fn get(&mut self) -> Option<VideoFrame> {
        self.data.pop_front()
    }

    fn push(&mut self, frame: VideoFrame) {
        self.data.push_back(frame);

        match self.last_added_index {
            None => {
                self.last_added_index = Some(0);
            }
            Some(index) => {
                self.last_added_index = Some(index + 1);
            }
        }
    }
}

impl Default for RingBuffer {
    fn default() -> Self {
        Self {
            capacity: 0,
            data: VecDeque::new(),
            last_added_index: None,
        }
    }
}

#[derive(Default)]
pub struct VecPool {
    pool: RefCell<Vec<Vec<u32>>>,
}

impl VecPool {
    pub fn new() -> Self {
        Self {
            pool: RefCell::new(Vec::new()),
        }
    }

    // TODO: rework this to avoid zeroing out the vec
    pub fn acquire(&self, capacity: usize) -> Vec<u32> {
        let mut pool = self.pool.borrow_mut();
        match pool.pop() {
            Some(mut vec) => {
                vec.clear();
                vec.resize(capacity, 0);
                vec
            }
            None => vec![0u32; capacity],
        }
    }

    pub fn release(&self, vec: Vec<u32>) {
        let mut pool = self.pool.borrow_mut();
        pool.push(vec);
    }
}

fn split_nv12_data(
    data: &[u8],
    width: usize,
    height: usize,
    y_stride: usize,
    uv_stride: usize,
    y_data: &mut [u32],
    uv_data: &mut [u32],
) {
    let mut y_idx = 0;
    let mut uv_idx = 0;

    if y_data.len() < width * height || uv_data.len() < (width / 2) * (height / 2) {
        makepad_error_log::log!("y_data len: {}, uv_data len: {}, width: {}, height: {}", y_data.len(), uv_data.len(), width, height);
        return; 
    }

    // Extract and convert Y data
    for row in 0..height {
        let start = row * y_stride;
        let end = start + width;
        for &y in &data[start..end] {
            y_data[y_idx] = 0xFFFFFF00u32 | (y as u32);
            y_idx += 1;
        }
    }

    // Extract and convert UV data
    let uv_start = y_stride * height;
    for row in 0..(height / 2) {
        let start = uv_start + row * uv_stride;
        let end = start + width;
        for chunk in data[start..end].chunks(2) {
            let u = chunk[0];
            let v = chunk[1];
            uv_data[uv_idx] = (u as u32) << 16 | (v as u32) << 8 | 0xFF000000u32;
            uv_idx += 1;
        }
    }
}
