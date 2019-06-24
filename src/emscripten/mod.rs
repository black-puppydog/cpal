use std::mem;
use std::os::raw::c_void;
use std::slice::from_raw_parts;
use std::sync::Mutex;
use stdweb;
use stdweb::Reference;
use stdweb::unstable::TryInto;
use stdweb::web::TypedArray;
use stdweb::web::set_timeout;

use BuildStreamError;
use DefaultFormatError;
use DeviceNameError;
use DevicesError;
use Format;
use PauseStreamError;
use PlayStreamError;
use SupportedFormatsError;
use StreamData;
use StreamDataResult;
use SupportedFormat;
use UnknownTypeOutputBuffer;

// The emscripten backend works by having a global variable named `_cpal_audio_contexts`, which
// is an array of `AudioContext` objects. A stream ID corresponds to an entry in this array.
//
// Creating a stream creates a new `AudioContext`. Destroying a stream destroys it.

// TODO: handle latency better ; right now we just use setInterval with the amount of sound data
// that is in each buffer ; this is obviously bad, and also the schedule is too tight and there may
// be underflows

pub struct EventLoop {
    streams: Mutex<Vec<Option<Reference>>>,
}

impl EventLoop {
    #[inline]
    pub fn new() -> EventLoop {
        stdweb::initialize();
        EventLoop {
            streams: Mutex::new(Vec::new()),
        }
    }

    #[inline]
    pub fn run<F>(&self, callback: F) -> !
        where F: FnMut(StreamId, StreamDataResult) + Send,
    {
        // The `run` function uses `set_timeout` to invoke a Rust callback repeatidely. The job
        // of this callback is to fill the content of the audio buffers.

        // The first argument of the callback function (a `void*`) is a casted pointer to `self`
        // and to the `callback` parameter that was passed to `run`.

        fn callback_fn<F>(user_data_ptr: *mut c_void)
            where F: FnMut(StreamId, StreamDataResult)
        {
            unsafe {
                let user_data_ptr2 = user_data_ptr as *mut (&EventLoop, F);
                let user_data = &mut *user_data_ptr2;
                let user_cb = &mut user_data.1;

                let streams = user_data.0.streams.lock().unwrap().clone();
                for (stream_id, stream) in streams.iter().enumerate() {
                    let stream = match stream.as_ref() {
                        Some(v) => v,
                        None => continue,
                    };

                    let mut temporary_buffer = vec![0.0; 44100 * 2 / 3];

                    {
                        let buffer = UnknownTypeOutputBuffer::F32(::OutputBuffer { buffer: &mut temporary_buffer });
                        let data = StreamData::Output { buffer: buffer };
                        user_cb(StreamId(stream_id), Ok(data));
                        // TODO: directly use a TypedArray<f32> once this is supported by stdweb
                    }

                    let typed_array = {
                        let f32_slice = temporary_buffer.as_slice();
                        let u8_slice: &[u8] = from_raw_parts(
                            f32_slice.as_ptr() as *const _,
                            f32_slice.len() * mem::size_of::<f32>(),
                        );
                        let typed_array: TypedArray<u8> = u8_slice.into();
                        typed_array
                    };

                    let num_channels = 2u32; // TODO: correct value
                    debug_assert_eq!(temporary_buffer.len() % num_channels as usize, 0);

                    js!(
                        var src_buffer = new Float32Array(@{typed_array}.buffer);
                        var context = @{stream};
                        var buf_len = @{temporary_buffer.len() as u32};
                        var num_channels = @{num_channels};

                        var buffer = context.createBuffer(num_channels, buf_len / num_channels, 44100);
                        for (var channel = 0; channel < num_channels; ++channel) {
                            var buffer_content = buffer.getChannelData(channel);
                            for (var i = 0; i < buf_len / num_channels; ++i) {
                                buffer_content[i] = src_buffer[i * num_channels + channel];
                            }
                        }

                        var node = context.createBufferSource();
                        node.buffer = buffer;
                        node.connect(context.destination);
                        node.start();
                    );
                }

                set_timeout(|| callback_fn::<F>(user_data_ptr), 330);
            }
        }

        let mut user_data = (self, callback);
        let user_data_ptr = &mut user_data as *mut (_, _);

        set_timeout(|| callback_fn::<F>(user_data_ptr as *mut _), 10);

        stdweb::event_loop();
    }

    #[inline]
    pub fn build_input_stream(&self, _: &Device, _format: &Format) -> Result<StreamId, BuildStreamError> {
        unimplemented!();
    }

    #[inline]
    pub fn build_output_stream(&self, _: &Device, _format: &Format) -> Result<StreamId, BuildStreamError> {
        let stream = js!(return new AudioContext()).into_reference().unwrap();

        let mut streams = self.streams.lock().unwrap();
        let stream_id = if let Some(pos) = streams.iter().position(|v| v.is_none()) {
            streams[pos] = Some(stream);
            pos
        } else {
            let l = streams.len();
            streams.push(Some(stream));
            l
        };

        Ok(StreamId(stream_id))
    }

    #[inline]
    pub fn destroy_stream(&self, stream_id: StreamId) {
        self.streams.lock().unwrap()[stream_id.0] = None;
    }

    #[inline]
    pub fn play_stream(&self, stream_id: StreamId) -> Result<(), PlayStreamError> {
        let streams = self.streams.lock().unwrap();
        let stream = streams
            .get(stream_id.0)
            .and_then(|v| v.as_ref())
            .expect("invalid stream ID");
        js!(@{stream}.resume());
        Ok(())
    }

    #[inline]
    pub fn pause_stream(&self, stream_id: StreamId) -> Result<(), PauseStreamError> {
        let streams = self.streams.lock().unwrap();
        let stream = streams
            .get(stream_id.0)
            .and_then(|v| v.as_ref())
            .expect("invalid stream ID");
        js!(@{stream}.suspend());
        Ok(())
    }
}

// Index within the `streams` array of the events loop.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamId(usize);

// Detects whether the `AudioContext` global variable is available.
fn is_webaudio_available() -> bool {
    stdweb::initialize();

    js!(if (!AudioContext) {
            return false;
        } else {
            return true;
        }).try_into()
        .unwrap()
}

// Content is false if the iterator is empty.
pub struct Devices(bool);

impl Devices {
    pub fn new() -> Result<Self, DevicesError> {
        Ok(Self::default())
    }
}

impl Default for Devices {
    fn default() -> Devices {
        // We produce an empty iterator if the WebAudio API isn't available.
        Devices(is_webaudio_available())
    }
}
impl Iterator for Devices {
    type Item = Device;
    #[inline]
    fn next(&mut self) -> Option<Device> {
        if self.0 {
            self.0 = false;
            Some(Device)
        } else {
            None
        }
    }
}

#[inline]
pub fn default_input_device() -> Option<Device> {
    unimplemented!();
}

#[inline]
pub fn default_output_device() -> Option<Device> {
    if is_webaudio_available() {
        Some(Device)
    } else {
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device;

impl Device {
    #[inline]
    pub fn name(&self) -> Result<String, DeviceNameError> {
        Ok("Default Device".to_owned())
    }

    #[inline]
    pub fn supported_input_formats(&self) -> Result<SupportedInputFormats, SupportedFormatsError> {
        unimplemented!();
    }

    #[inline]
    pub fn supported_output_formats(&self) -> Result<SupportedOutputFormats, SupportedFormatsError> {
        // TODO: right now cpal's API doesn't allow flexibility here
        //       "44100" and "2" (channels) have also been hard-coded in the rest of the code ; if
        //       this ever becomes more flexible, don't forget to change that
        //       According to https://developer.mozilla.org/en-US/docs/Web/API/BaseAudioContext/createBuffer
        //       browsers must support 1 to 32 channels at leats and 8,000 Hz to 96,000 Hz.
        Ok(
            vec![
                SupportedFormat {
                    channels: 2,
                    min_sample_rate: ::SampleRate(44100),
                    max_sample_rate: ::SampleRate(44100),
                    data_type: ::SampleFormat::F32,
                },
            ].into_iter(),
        )
    }

    pub fn default_input_format(&self) -> Result<Format, DefaultFormatError> {
        unimplemented!();
    }

    pub fn default_output_format(&self) -> Result<Format, DefaultFormatError> {
        // TODO: because it is hard coded, see supported_output_formats.
        Ok(
            Format {
                channels: 2,
                sample_rate: ::SampleRate(44100),
                data_type: ::SampleFormat::F32,
            },
        )
    }
}

pub type SupportedInputFormats = ::std::vec::IntoIter<SupportedFormat>;
pub type SupportedOutputFormats = ::std::vec::IntoIter<SupportedFormat>;
