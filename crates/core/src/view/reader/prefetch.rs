//! Background page rendering for fixed-layout documents.
//!
//! A worker thread with its own instance of the document renders the pages
//! the reader is likely to show next, so that a page turn only has to blit
//! an already rendered pixmap. If the requested page is being rendered when
//! the reader needs it, the reader waits for that render instead of doing
//! the same work a second time on the UI thread.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use crate::document::{open, Document, Location};
use crate::framebuffer::{Framebuffer, Pixmap};
use crate::geom::Rectangle;
use crate::metadata::{Margin, ZoomMode};
use crate::device::CURRENT_DEVICE;
use super::{scaling_factor, Resource};

// Rendered pages waiting to be picked up (each one is ~2.6 MB on an rM2).
const MAX_READY: usize = 4;

// Everything a page's pixmap depends on. Results rendered with different
// parameters (after a zoom, crop or rotation change) are discarded.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderParams {
    pub rect: Rectangle,
    pub cropping_margin: Margin,
    pub margin_width: i32,
    pub zoom_mode: ZoomMode,
}

pub fn render_resource(doc: &mut dyn Document, location: usize, params: &RenderParams) -> Resource {
    let RenderParams { rect, cropping_margin, margin_width, zoom_mode } = params;
    let dims = doc.dims(location).unwrap_or((3.0, 4.0));
    let scale = scaling_factor(rect, cropping_margin, *margin_width, dims, *zoom_mode);
    if let Some((pixmap, _)) = doc.pixmap(Location::Exact(location), scale, CURRENT_DEVICE.color_samples()) {
        let frame = rect![(cropping_margin.left * pixmap.width as f32).ceil() as i32,
                          (cropping_margin.top * pixmap.height as f32).ceil() as i32,
                          ((1.0 - cropping_margin.right) * pixmap.width as f32).floor() as i32,
                          ((1.0 - cropping_margin.bottom) * pixmap.height as f32).floor() as i32];
        Resource { pixmap, frame, scale }
    } else {
        let width = (dims.0 * scale).max(1.0) as u32;
        let height = (dims.1 * scale).max(1.0) as u32;
        let pixmap = Pixmap::empty(width, height, CURRENT_DEVICE.color_samples());
        let frame = pixmap.rect();
        Resource { pixmap, frame, scale }
    }
}

struct Job {
    location: usize,
    params: RenderParams,
}

#[derive(Default)]
struct State {
    queue: Vec<Job>,  // Highest priority first.
    in_flight: Option<(usize, RenderParams)>,
    ready: Vec<(usize, RenderParams, Resource)>,
    quit: bool,
}

pub struct Prefetcher {
    shared: Arc<(Mutex<State>, Condvar)>,
}

impl Prefetcher {
    pub fn new(path: PathBuf) -> Prefetcher {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let shared2 = Arc::clone(&shared);

        thread::Builder::new().name("prefetch".to_string()).spawn(move || {
            // Stay out of the way of the UI thread on this dual core device.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 5); }

            let (lock, cvar) = &*shared2;
            let Some(mut doc) = open(&path) else {
                eprintln!("Prefetch: can't open {}.", path.display());
                lock.lock().unwrap().quit = true;
                cvar.notify_all();
                return;
            };

            loop {
                let job = {
                    let mut state = lock.lock().unwrap();
                    while state.queue.is_empty() && !state.quit {
                        state = cvar.wait(state).unwrap();
                    }
                    if state.quit {
                        break;
                    }
                    let job = state.queue.remove(0);
                    state.in_flight = Some((job.location, job.params.clone()));
                    job
                };

                let resource = timed!(format!("pixmap p{} (prefetch)", job.location),
                                      render_resource(doc.as_mut(), job.location, &job.params));

                let mut state = lock.lock().unwrap();
                state.in_flight = None;
                state.ready.push((job.location, job.params, resource));
                if state.ready.len() > MAX_READY {
                    state.ready.remove(0);
                }
                cvar.notify_all();
            }
        }).ok();

        Prefetcher { shared }
    }

    // Replaces the pending work with `locations` (highest priority first).
    pub fn request(&self, locations: &[usize], params_for: impl Fn(usize) -> RenderParams) {
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap();
        if state.quit {
            return;
        }
        let jobs: Vec<Job> = locations.iter().filter_map(|&location| {
            let params = params_for(location);
            let done = state.ready.iter().any(|(l, p, _)| *l == location && *p == params) ||
                       state.in_flight.as_ref().is_some_and(|(l, p)| *l == location && *p == params);
            if done { None } else { Some(Job { location, params }) }
        }).collect();
        state.queue = jobs;
        // Results nobody asked for any more only hold on to memory.
        state.ready.retain(|(l, _, _)| locations.contains(l));
        cvar.notify_all();
    }

    // Returns the prefetched page, waiting if it is being rendered right now.
    pub fn take(&self, location: usize, params: &RenderParams) -> Option<Resource> {
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap();
        state.queue.retain(|job| job.location != location);
        loop {
            if let Some(i) = state.ready.iter().position(|(l, p, _)| *l == location && p == params) {
                return Some(state.ready.remove(i).2);
            }
            let rendering = state.in_flight.as_ref().is_some_and(|(l, p)| *l == location && p == params);
            if !rendering || state.quit {
                return None;
            }
            state = cvar.wait(state).unwrap();
        }
    }
}

impl Drop for Prefetcher {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap();
        state.quit = true;
        state.queue.clear();
        cvar.notify_all();
    }
}
