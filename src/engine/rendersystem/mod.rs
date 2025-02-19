use log::{error, info};
use nalgebra::*;
use std::{cell::SyncUnsafeCell, collections::HashMap, fs, io, mem, sync::Arc};

#[cfg(not(any(target_os = "macos", target_os = "ios", xbox)))]
mod vulkan;

mod render_impl {
    #[cfg(not(any(target_os = "macos", target_os = "ios", xbox)))]
    pub use crate::engine::rendersystem::vulkan::*;
}

pub type ThingHolder<T> = Arc<SyncUnsafeCell<T>>;

pub struct State {
    backend: render_impl::State,
    shaders: HashMap<String, ThingHolder<Shader>>,
    models: HashMap<String, ThingHolder<Model>>,
    materials: HashMap<String, ThingHolder<Material>>,
}

impl State {
    pub fn init(video: &crate::platform::video::State) -> Self {
        info!("Render system initialization started");
        let backend = render_impl::State::init(video);
        info!("Render system initialization succeeded");

        Self {
            backend,
            shaders: HashMap::new(),
            models: HashMap::new(),
            materials: HashMap::new(),
        }
    }

    pub fn load_resources(&mut self) {
        if self.backend.is_initialized() && !self.backend.is_loaded() {
            info!("Loading resources");
            self.backend.load_resources(&mut self.models);
            info!("Done loading resources");
        }
    }

    pub fn begin_cmds(&mut self, video: &crate::platform::video::State) {
        self.backend.begin_cmds(video)
    }

    pub fn present(&mut self) {
        self.backend.present()
    }

    pub fn unload_resources(&mut self) {
        if self.backend.is_initialized() && self.backend.is_loaded() {
            info!("Unloading resources");
            self.backend.unload_resources();
            info!("Done unloading resources");
        }
    }

    pub fn shutdown(mut self) {
        info!("Render system shutdown started");
        self.unload_resources();
        self.backend.shutdown();
        info!("Render system shutdown succeeded");
    }
}

#[derive(Debug)]
pub enum ShaderError {
    Io(io::Error),
    Backend(render_impl::ShaderErrorType),
}

pub struct Shader {
    name: String,
    handle: render_impl::ShaderData,
}

impl Shader {
    pub fn new(
        state: &mut crate::engine::State,
        name: &str,
    ) -> Result<ThingHolder<Self>, ShaderError> {
        info!("Creating shader {name}");

        let vertex_path = format!(
            "{}{name}{}",
            crate::engine::GameDirs::shaders(state),
            render_impl::ShaderData::vertex_extension()
        );
        let fragment_path = format!(
            "{}{name}{}",
            crate::engine::GameDirs::shaders(state),
            render_impl::ShaderData::fragment_extension()
        );
        let vertex_binary = match fs::read(&vertex_path) {
            Ok(data) => data,
            Err(err) => {
                error!("Failed to read vertex binary {vertex_path} for shader {name}: {err}");
                return Err(ShaderError::Io(err));
            }
        };
        let fragment_binary = match fs::read(&fragment_path) {
            Ok(data) => data,
            Err(err) => {
                error!("Failed to read fragment binary {fragment_path} for shader {name}: {err}");
                return Err(ShaderError::Io(err));
            }
        };
        let handle = match render_impl::ShaderData::new(
            &state.render().backend,
            name,
            vertex_binary,
            fragment_binary,
        ) {
            Ok(handle) => handle,
            Err(err) => {
                error!("Failed to create shader {name}: {err:?}");
                return Err(err);
            }
        };

        let shader = Arc::new(SyncUnsafeCell::new(Self {
            name: String::from(name),
            handle,
        }));
        state
            .render()
            .shaders
            .insert(String::from(name), shader.clone());

        info!("Shader {name} created successfully");

        Ok(shader)
    }

    pub fn destroy(&self, state: &State) {
        self.handle.destroy(&state.backend);
    }

    pub fn name(&self) -> &String {
        &self.name
    }
}

#[repr(C)]
pub struct UniformData {
    model: Matrix4<f64>,
    view: Matrix4<f64>,
    projection: Matrix4<f64>,
}

pub struct RenderTexture {
    name: String,
    //texture: crate::texture::Texture,
    //handle: render_impl::TextureData
}

pub struct Material {
    name: String,
    shader: ThingHolder<Shader>,
    //   texture: Arc<RenderTexture>,
}

impl Material {
    pub fn new(state: &mut State, name: &str, shader: &str) -> Result<ThingHolder<Self>, ()> {
        let material = Arc::new(SyncUnsafeCell::new(Self {
            name: String::from(name),
            shader: match state.shaders.get(&String::from(shader)) {
                Some(thing) => thing,
                None => {
                    return Err(());
                }
            }
            .clone(),
        }));
        state.materials.insert(String::from(name), material.clone());
        Ok(material)
    }

    pub fn name(&self) -> &String {
        &self.name
    }
}

pub trait Renderable {
    fn render(&self, state: &mut State);
}

#[derive(PartialEq)]
pub struct Vertex {
    position: Vector3<f32>,
    texture_coordinate: Vector2<f32>,
    normal: Vector3<f32>,
}

pub struct Model {
    name: String,
    data: Vec<u8>,
    material: ThingHolder<Material>,
    handle: render_impl::ModelData,
}

impl Model {
    pub fn new(
        state: &mut State,
        name: &str,
        models: Vec<tobj::Model>,
        material: &str,
    ) -> Result<ThingHolder<Self>, ()> {
        info!("Creating model {name}");

        // largely based on https://github.com/bwasty/learn-opengl-rs/blob/master/src/model.rs
        let mut all_vertices = Vec::new();
        let mut all_indices: Vec<u32> = Vec::new();
        for model in models {
            let mut mesh = model.mesh;

            assert!(!mesh.normals.is_empty() && !mesh.texcoords.is_empty());

            let vertex_count = mesh.positions.len() / 3;
            let mut vertices = Vec::with_capacity(vertex_count);
            let (p, t, n) = (&mesh.positions, &mesh.texcoords, &mesh.normals);
            for i in 0..vertex_count {
                let position = Vector3::new(p[i * 3], p[i * 3 + 1], p[i * 3 + 2]);
                let texture_coordinate = Vector2::new(t[i * 2], t[i * 2 + 1]);
                let normal = Vector3::new(n[i * 3], n[i * 3 + 1], n[i * 3 + 2]);
                vertices.push(Vertex {
                    position,
                    texture_coordinate,
                    normal,
                })
            }

            all_vertices.append(&mut vertices);
            all_indices.append(&mut mesh.indices);
        }

        let vertices_size = all_vertices.len() * mem::size_of::<Vertex>();
        let indices_size = all_indices.len() * mem::size_of::<u32>();

        let mut data = Vec::new();
        let vertices = all_vertices.into_raw_parts();
        data.append(&mut unsafe {
            Vec::from_raw_parts(vertices.0 as *mut u8, vertices_size, vertices_size)
        });
        let indices = all_indices.into_raw_parts();
        data.append(&mut unsafe {
            Vec::from_raw_parts(indices.0 as *mut u8, indices_size, indices_size)
        });

        let handle = render_impl::ModelData::new(&state.backend, name, vertices_size, indices_size);

        let model = Arc::new(SyncUnsafeCell::new(Self {
            name: String::from(name),
            material: match state.materials.get(&String::from(material)) {
                Some(thing) => thing,
                None => {
                    return Err(());
                }
            }
            .clone(),
            data,
            handle,
        }));
        state.models.insert(String::from(name), model.clone());

        info!("Created model {name} successfully");

        Ok(model)
    }

    pub fn name(&self) -> &String {
        &self.name
    }

    pub fn data(&self) -> &Vec<u8> {
        &self.data
    }

    pub fn size(&self) -> u64 {
        self.data.len() as u64
    }
}

impl Renderable for Model {
    fn render(&self, state: &mut State) {
        if state.backend.is_in_frame() {
            state.backend.render_model(self);
        }
    }
}
