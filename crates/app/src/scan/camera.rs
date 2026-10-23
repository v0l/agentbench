//! The back camera through the NDK's camera2 API, into an image reader that hands over the grey
//! (luma) plane of each frame, which is all a QR code needs.

use super::Scan;
use ndk_sys::*;
use std::ffi::{CStr, c_void};
use std::sync::{Arc, Mutex};

const PERMISSION: &str = "android.permission.CAMERA";
const WIDTH: i32 = 1280;
const HEIGHT: i32 = 720;

/// What the camera callbacks write and the window reads.
struct Shared {
    state: Scan,
    preview: Option<(Vec<u8>, usize, usize)>,
    frames: u32,
    wake: Arc<dyn Fn() + Send + Sync>,
}

pub struct Camera {
    shared: Arc<Mutex<Shared>>,
    /// Everything opened for the camera, closed in `Drop`. None until the permission is given.
    open: Arc<Mutex<Option<Open>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

struct Open {
    manager: *mut ACameraManager,
    device: *mut ACameraDevice,
    reader: *mut AImageReader,
    outputs: *mut ACaptureSessionOutputContainer,
    output: *mut ACaptureSessionOutput,
    request: *mut ACaptureRequest,
    target: *mut ACameraOutputTarget,
    session: *mut ACameraCaptureSession,
    /// The context the reader's callback was given, freed with the rest.
    listener: *mut Arc<Mutex<Shared>>,
}

// The raw handles are only touched under the mutex, from this module.
unsafe impl Send for Open {}

impl Camera {
    pub fn start(wake: Arc<dyn Fn() + Send + Sync>) -> Camera {
        let shared = Arc::new(Mutex::new(Shared {
            state: Scan::Starting,
            preview: None,
            frames: 0,
            wake: wake.clone(),
        }));
        let open = Arc::new(Mutex::new(None));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cam = Camera { shared: shared.clone(), open: open.clone(), stop: stop.clone() };
        std::thread::spawn(move || {
            // The permission dialog answers on its own time, so wait for it here.
            if !crate::voice::android::allowed(PERMISSION) {
                if let Err(e) = crate::voice::android::ask(PERMISSION) {
                    return fail(&shared, e);
                }
                let mut waited = 0;
                while !crate::voice::android::allowed(PERMISSION) {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    waited += 1;
                    if waited > 600 {
                        return fail(&shared, "the camera was not allowed".into());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            match unsafe { open_back_camera(&shared) } {
                Ok(o) => {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        unsafe { close(o) };
                    } else {
                        shared.lock().unwrap().state = Scan::Looking;
                        *open.lock().unwrap() = Some(o);
                    }
                }
                Err(e) => fail(&shared, e),
            }
            wake();
        });
        cam
    }

    pub fn state(&self) -> Scan {
        self.shared.lock().unwrap().state.clone()
    }

    pub fn preview(&self) -> Option<(Vec<u8>, usize, usize)> {
        self.shared.lock().unwrap().preview.clone()
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(o) = self.open.lock().unwrap().take() {
            unsafe { close(o) };
        }
    }
}

fn fail(shared: &Arc<Mutex<Shared>>, why: String) {
    let mut s = shared.lock().unwrap();
    s.state = Scan::Failed(why);
    (s.wake)();
}

unsafe extern "C" fn disconnected(_: *mut c_void, _: *mut ACameraDevice) {}
unsafe extern "C" fn errored(_: *mut c_void, _: *mut ACameraDevice, _: i32) {}
unsafe extern "C" fn session_state(_: *mut c_void, _: *mut ACameraCaptureSession) {}

/// A frame is ready: read its grey plane, look for a code in it, and keep it for the preview.
unsafe extern "C" fn frame(context: *mut c_void, reader: *mut AImageReader) {
    let shared = unsafe { &*(context as *const Arc<Mutex<Shared>>) };
    let mut image: *mut AImage = std::ptr::null_mut();
    if unsafe { AImageReader_acquireLatestImage(reader, &mut image) } != media_status_t::AMEDIA_OK
        || image.is_null()
    {
        return;
    }
    let (mut w, mut h, mut stride, mut len) = (0i32, 0i32, 0i32, 0i32);
    let mut data: *mut u8 = std::ptr::null_mut();
    unsafe {
        AImage_getWidth(image, &mut w);
        AImage_getHeight(image, &mut h);
        AImage_getPlaneRowStride(image, 0, &mut stride);
        AImage_getPlaneData(image, 0, &mut data, &mut len);
    }
    if !data.is_null() && w > 0 && h > 0 && stride >= w {
        let (w, h, stride) = (w as usize, h as usize, stride as usize);
        let plane = unsafe { std::slice::from_raw_parts(data, len as usize) };
        let mut grey = Vec::with_capacity(w * h);
        for row in 0..h {
            let start = row * stride;
            if start + w > plane.len() {
                break;
            }
            grey.extend_from_slice(&plane[start..start + w]);
        }
        if grey.len() == w * h {
            let mut s = shared.lock().unwrap();
            s.frames += 1;
            // Reading is the costly part, so every other frame is enough.
            let found = if matches!(s.state, Scan::Looking) && s.frames % 2 == 0 {
                super::decode(&grey, w, h)
            } else {
                None
            };
            s.preview = Some((grey, w, h));
            if let Some(text) = found {
                s.state = Scan::Found(text);
            }
            (s.wake)();
        }
    }
    unsafe { AImage_delete(image) };
}

unsafe fn open_back_camera(shared: &Arc<Mutex<Shared>>) -> Result<Open, String> {
    unsafe {
        let manager = ACameraManager_create();
        if manager.is_null() {
            return Err("no camera service".into());
        }
        let mut list: *mut ACameraIdList = std::ptr::null_mut();
        if ACameraManager_getCameraIdList(manager, &mut list) != camera_status_t::ACAMERA_OK
            || list.is_null()
        {
            ACameraManager_delete(manager);
            return Err("could not list the cameras".into());
        }
        // The first camera facing back, else whichever is first.
        let ids: Vec<*const std::ffi::c_char> =
            (0..(*list).numCameras).map(|i| *(*list).cameraIds.add(i as usize)).collect();
        let mut chosen = ids.first().copied();
        for id in &ids {
            let mut meta: *mut ACameraMetadata = std::ptr::null_mut();
            if ACameraManager_getCameraCharacteristics(manager, *id, &mut meta)
                != camera_status_t::ACAMERA_OK
            {
                continue;
            }
            let mut entry: ACameraMetadata_const_entry = std::mem::zeroed();
            let back = ACameraMetadata_getConstEntry(
                meta,
                acamera_metadata_tag::ACAMERA_LENS_FACING.0,
                &mut entry,
            ) == camera_status_t::ACAMERA_OK
                && entry.count > 0
                && *entry.data.u8_ == acamera_metadata_enum_acamera_lens_facing::ACAMERA_LENS_FACING_BACK.0 as u8;
            ACameraMetadata_free(meta);
            if back {
                chosen = Some(*id);
                break;
            }
        }
        let Some(id) = chosen else {
            ACameraManager_deleteCameraIdList(list);
            ACameraManager_delete(manager);
            return Err("this device has no camera".into());
        };
        let name = CStr::from_ptr(id).to_string_lossy().to_string();

        let mut callbacks = ACameraDevice_StateCallbacks {
            context: std::ptr::null_mut(),
            onDisconnected: Some(disconnected),
            onError: Some(errored),
        };
        let mut device: *mut ACameraDevice = std::ptr::null_mut();
        let opened = ACameraManager_openCamera(manager, id, &mut callbacks, &mut device);
        ACameraManager_deleteCameraIdList(list);
        if opened != camera_status_t::ACAMERA_OK || device.is_null() {
            ACameraManager_delete(manager);
            return Err(format!("could not open camera {name} ({})", opened.0));
        }

        let mut reader: *mut AImageReader = std::ptr::null_mut();
        if AImageReader_new(WIDTH, HEIGHT, AIMAGE_FORMATS::AIMAGE_FORMAT_YUV_420_888.0 as i32, 3, &mut reader)
            != media_status_t::AMEDIA_OK
        {
            ACameraDevice_close(device);
            ACameraManager_delete(manager);
            return Err("could not read the camera's pictures".into());
        }
        let listener = Box::into_raw(Box::new(shared.clone()));
        let mut on_image = AImageReader_ImageListener {
            context: listener.cast(),
            onImageAvailable: Some(frame),
        };
        AImageReader_setImageListener(reader, &mut on_image);

        let mut window: *mut ANativeWindow = std::ptr::null_mut();
        AImageReader_getWindow(reader, &mut window);
        let mut outputs: *mut ACaptureSessionOutputContainer = std::ptr::null_mut();
        let mut output: *mut ACaptureSessionOutput = std::ptr::null_mut();
        ACaptureSessionOutputContainer_create(&mut outputs);
        ACaptureSessionOutput_create(window, &mut output);
        ACaptureSessionOutputContainer_add(outputs, output);

        let mut request: *mut ACaptureRequest = std::ptr::null_mut();
        ACameraDevice_createCaptureRequest(
            device,
            ACameraDevice_request_template::TEMPLATE_PREVIEW,
            &mut request,
        );
        let mut target: *mut ACameraOutputTarget = std::ptr::null_mut();
        ACameraOutputTarget_create(window, &mut target);
        ACaptureRequest_addTarget(request, target);

        let mut states = ACameraCaptureSession_stateCallbacks {
            context: std::ptr::null_mut(),
            onClosed: Some(session_state),
            onReady: Some(session_state),
            onActive: Some(session_state),
        };
        let mut session: *mut ACameraCaptureSession = std::ptr::null_mut();
        let made = ACameraDevice_createCaptureSession(device, outputs, &mut states, &mut session);
        let o = Open { manager, device, reader, outputs, output, request, target, session, listener };
        if made != camera_status_t::ACAMERA_OK || session.is_null() {
            close(o);
            return Err(format!("could not start the camera ({})", made.0));
        }
        let mut requests = [request];
        let started = ACameraCaptureSession_setRepeatingRequest(
            session,
            std::ptr::null_mut(),
            1,
            requests.as_mut_ptr(),
            std::ptr::null_mut(),
        );
        if started != camera_status_t::ACAMERA_OK {
            close(o);
            return Err(format!("could not start the camera ({})", started.0));
        }
        Ok(o)
    }
}

unsafe fn close(o: Open) {
    unsafe {
        if !o.session.is_null() {
            ACameraCaptureSession_stopRepeating(o.session);
            ACameraCaptureSession_close(o.session);
        }
        if !o.request.is_null() {
            ACaptureRequest_removeTarget(o.request, o.target);
            ACaptureRequest_free(o.request);
        }
        if !o.target.is_null() {
            ACameraOutputTarget_free(o.target);
        }
        ACameraDevice_close(o.device);
        if !o.output.is_null() {
            ACaptureSessionOutputContainer_remove(o.outputs, o.output);
            ACaptureSessionOutput_free(o.output);
        }
        if !o.outputs.is_null() {
            ACaptureSessionOutputContainer_free(o.outputs);
        }
        AImageReader_delete(o.reader);
        drop(Box::from_raw(o.listener));
        ACameraManager_delete(o.manager);
    }
}
