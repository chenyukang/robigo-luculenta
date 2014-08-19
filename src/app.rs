// Robigo Luculenta -- Proof of concept spectral path tracer in Rust
// Copyright (C) 2014 Ruud van Asseldonk
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::comm::{Handle, Select, Sender, Receiver, channel};
use std::io::timer::sleep;
use std::os::num_cpus;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::vec::unzip;
use camera::Camera;
use gather_unit::GatherUnit;
use geometry::{Circle, Plane, Sphere};
use material::{BlackBodyMaterial, DiffuseGreyMaterial, DiffuseColouredMaterial};
use object::{Emissive, Object, Reflective};
use plot_unit::PlotUnit;
use quaternion::Quaternion;
use scene::Scene;
use task_scheduler::{Task, Sleep, Trace, Plot, Gather, Tonemap, TaskScheduler};
use tonemap_unit::TonemapUnit;
use trace_unit::TraceUnit;
use vector3::Vector3;

pub type Image = Vec<u8>;

/// Width of the canvas.
pub static image_width: uint = 1280;

/// Height of the canvas.
pub static image_height: uint = 720;

/// Canvas aspect ratio.
static aspect_ratio: f32 = image_width as f32 / image_height as f32;

pub struct App {
    /// Channel that can be used to signal the application to stop.
    pub stop: Sender<()>,

    /// Channel that produces a rendered image periodically.
    pub images: Receiver<Image>
}

impl App {
    pub fn new() -> App {
        let concurrency = num_cpus();
        let ts = TaskScheduler::new(concurrency, image_width, image_height);
        let task_scheduler = Arc::new(Mutex::new(ts));

        // Channels for communicating back to the main task.
        let (stop_tx, stop_rx) = channel::<()>();
        let (img_tx, img_rx) = channel();

        // Then spawn a supervisor task that will start the workers.
        spawn(proc() {
            // Spawn as many workers as cores.
            let (stop_workers, images) = unzip(
            range(0u, concurrency)
            .map(|_| { App::start_worker(task_scheduler.clone()) }));
            
            // Combine values so we can recv one at a time.
            let select = Select::new();
            let mut worker_handles: Vec<Handle<Image>> = images
            .iter().map(|worker_rx| { select.handle(worker_rx) }).collect();
            for handle in worker_handles.mut_iter() {
                unsafe { handle.add(); }
            }
            let mut stop_handle = select.handle(&stop_rx);
            unsafe { stop_handle.add(); }

            // Then go into the supervising loop: broadcast a stop signal to
            // all workers, or route a rendered image to the main task.
            loop {
                let id = select.wait();

                // Was the source a worker?
                for handle in worker_handles.mut_iter() {
                    // When a new image arrives, route it to the main task.
                    if id == handle.id() {
                        let img = handle.recv();
                        img_tx.send(img);
                    }
                }

                // Or the stop channel perhaps?
                if id == stop_handle.id() {
                    // Broadcast to all workers that they should stop.
                    for stop in stop_workers.iter() {
                        stop.send(());
                    }
                    // Then also stop the supervising loop.
                    break;
                }
            }
        });

        App {
            stop: stop_tx,
            images: img_rx
        }
    }

    fn start_worker(task_scheduler: Arc<Mutex<TaskScheduler>>)
                    -> (Sender<()>, Receiver<Image>) {
        let (stop_tx, stop_rx) = channel::<()>();
        let (img_tx, img_rx) = channel::<Image>();

        spawn(proc() {
            // TODO: there should be one scene for the entire program,
            // not one per worker thread. However, I can't get sharing
            // the scene working properly :(
            let scene = App::set_up_scene();

            // Move img_tx into the proc.
            let mut owned_img_tx = img_tx;

            // There is no task yet, but the task scheduler expects
            // a completed task. Therefore, this worker is done sleeping.
            let mut task = Sleep;

            // Until something signals this worker to stop,
            // continue executing tasks.
            loop {
                // Ask the task scheduler for a new task, complete the old one.
                // Then execute it.
                task = task_scheduler.lock().get_new_task(task);
                App::execute_task(&mut task, &scene, &mut owned_img_tx);

                // Stop only if a stop signal has been sent.
                match stop_rx.try_recv() {
                    Ok(()) => break,
                    _ => { }
                }
            }
        });

        // TODO: spawn proc.
        (stop_tx, img_rx)
    }

    fn execute_task(task: &mut Task, scene: &Scene, img_tx: &mut Sender<Image>) {
        match *task {
            Sleep =>
                App::execute_sleep_task(),
            Trace(ref mut trace_unit) =>
                App::execute_trace_task(scene, &mut **trace_unit),
            Plot(ref mut plot_unit, ref mut units) =>
                App::execute_plot_task(&mut **plot_unit, units.as_mut_slice()),
            Gather(ref mut gather_unit, ref mut units) =>
                App::execute_gather_task(&mut **gather_unit, units.as_mut_slice()),
            Tonemap(ref mut tonemap_unit, ref mut gather_unit) =>
                App::execute_tonemap_task(img_tx, &mut **tonemap_unit, &mut **gather_unit)
        }
    }

    fn execute_sleep_task() {
        sleep(Duration::milliseconds(100));
    }

    fn execute_trace_task(scene: &Scene, trace_unit: &mut TraceUnit) {
        trace_unit.render(scene);
    }

    fn execute_plot_task(plot_unit: &mut PlotUnit,
                         units: &mut[Box<TraceUnit>]) {
        for unit in units.mut_iter() {
            plot_unit.plot(unit.mapped_photons);
        }
    }

    fn execute_gather_task(gather_unit: &mut GatherUnit,
                           units: &mut[Box<PlotUnit>]) {
        for unit in units.mut_iter() {
            gather_unit.accumulate(unit.tristimulus_buffer.as_slice());
            unit.clear();
        }
    }

    fn execute_tonemap_task(img_tx: &mut Sender<Image>,
                            tonemap_unit: &mut TonemapUnit,
                            gather_unit: &mut GatherUnit) {
        tonemap_unit.tonemap(gather_unit.tristimulus_buffer.as_slice());

        // Copy the rendered image.
        let img = tonemap_unit.rgb_buffer.clone();

        // And send it to the UI / main task.
        img_tx.send(img);
    }

    fn set_up_scene() -> Scene {
        let mut objects = Vec::new();

        let grey = DiffuseGreyMaterial::new(0.8);
        let red = DiffuseColouredMaterial::new(0.9, 660.0, 60.0);
        let green = DiffuseColouredMaterial::new(0.9, 550.0, 40.0);
        let blue = DiffuseColouredMaterial::new(0.5, 470.0, 25.0);

        let sun_emissive = BlackBodyMaterial::new(6504.0, 1.0);
        let sky1_emissive = BlackBodyMaterial::new(7600.0, 0.6);
        let sky2_emissive = BlackBodyMaterial::new(5000.0, 0.6);

        // Sphere in the centre.
        let sun_radius: f32 = 5.0;
        let sun_position = Vector3::zero();
        let sun_sphere = Sphere::new(sun_position, sun_radius);
        let sun = Object::new(box sun_sphere, Emissive(box sun_emissive));
        objects.push(sun);

        let floor_normal = Vector3::new(0.0, 0.0, -1.0);

        // Sky light 1.
        let sky_height: f32 = 30.0;
        let sky1_radius: f32 = 5.0;
        let sky1_position = Vector3::new(-sun_radius, 0.0, sky_height);
        let sky1_circle = Circle::new(floor_normal, sky1_position, sky1_radius);
        let sky1 = Object::new(box sky1_circle, Emissive(box sky1_emissive));
        objects.push(sky1);

        let sky2_radius: f32 = 15.0;
        let sky2_position = Vector3 {
            x: -sun_radius * 0.5, y: sun_radius * 2.0 + sky2_radius, z: sky_height
        };
        let sky2_circle = Circle::new(floor_normal, sky2_position, sky2_radius);
        let sky2 = Object::new(box sky2_circle, Emissive(box sky2_emissive));
        objects.push(sky2);

        // Ceiling plane (for more interesting light).
        let ceiling_position = Vector3::new(0.0, 0.0, sky_height * 2.0);
        let ceiling_plane = Plane::new(floor_normal, ceiling_position);
        let ceiling = Object::new(box ceiling_plane, Reflective(box blue));
        objects.push(ceiling);

        fn make_camera(t: f32) -> Camera {
            // Orbit around (0, 0, 0) based on the time.
            let pi: f32 = Float::pi();
            let phi = pi * (1.0 + 0.01 * t);
            let alpha = pi * (0.3 - 0.01 * t);

            // Also zoom in a bit. (Or actually, it is a dolly roll.)
            let distance = 50.0 - 0.5 * t;

            let position = Vector3 {
                x: alpha.cos() * phi.sin() * distance,
                y: alpha.cos() * phi.cos() * distance,
                z: alpha.sin() * distance
            };

            // Compensate for the displacement of the camera by rotating
            // such that (0, 0, 0) remains fixed. The camera is aimed
            // downward with angle alpha.
            let orientation = Quaternion::rotation(0.0, 0.0, -1.0, phi + Float::pi())
                * Quaternion::rotation(1.0, 0.0, 0.0, -alpha);

            Camera {
                position: position,
                field_of_view: pi * 0.35,
                focal_distance: distance * 0.9,
                // A slight blur, not too much, but enough to demonstrate the effect.
                depth_of_field: 2.0,
                // A subtle amount of chromatic abberation.
                chromatic_abberation: 0.012,
                orientation: orientation
            }
        }

        let red = DiffuseColouredMaterial::new(0.9, 700.0, 120.0);
        let plane = Plane::new(Vector3::new(0.0, 1.0, 0.0), Vector3::zero());
        let sphere = Sphere::new(Vector3::zero(), 2.0);
        let black_body = BlackBodyMaterial::new(6504.0, 1.0);
        let reflective = Object::new(box plane, Reflective(box red));
        let emissive = Object::new(box sphere, Emissive(box black_body));
        Scene {
            objects: objects,
            get_camera_at_time: make_camera
        }
    }
}
