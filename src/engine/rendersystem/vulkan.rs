use ash::{extensions, vk};
use log::{debug, error, log};
use std::rc::Rc;
use std::{alloc, ffi, mem, ptr};
use vk_mem::*;

macro_rules! vulkan_check {
    ($call: expr) => {
        $call.unwrap_or_else(|err| panic!("Vulkan call {} failed: {}", stringify!($call), err))
    };
}

extern "system" fn vulkan_alloc(
    _p_user_data: *mut ffi::c_void,
    size: usize,
    alignment: usize,
    _allocation_scope: vk::SystemAllocationScope,
) -> *mut ffi::c_void {
    unsafe {
        alloc::alloc(alloc::Layout::from_size_align(size, alignment).unwrap()) as *mut ffi::c_void
    }
}

extern "system" fn vulkan_realloc(
    _p_user_data: *mut ffi::c_void,
    p_original: *mut ffi::c_void,
    size: usize,
    alignment: usize,
    _allocation_scope: vk::SystemAllocationScope,
) -> *mut ffi::c_void {
    unsafe {
        alloc::realloc(
            p_original as *mut u8,
            alloc::Layout::from_size_align(size, alignment).unwrap(),
            size,
        ) as *mut ffi::c_void
    }
}

extern "system" fn vulkan_dealloc(_p_user_data: *mut ffi::c_void, p_memory: *mut ffi::c_void) {
    unsafe {
        alloc::dealloc(
            p_memory as *mut u8,
            alloc::Layout::from_size_align(0, 1).unwrap(),
        )
    }
}

const ALLOCATION_CALLBACKS: vk::AllocationCallbacks = vk::AllocationCallbacks {
    pfn_allocation: Some(vulkan_alloc),
    pfn_reallocation: Some(vulkan_realloc),
    pfn_free: Some(vulkan_dealloc),
    p_user_data: ptr::null_mut(),
    pfn_internal_allocation: None,
    pfn_internal_free: None,
};

const FRAME_COUNT: usize = 3;

struct GpuInfo {
    device: vk::PhysicalDevice,

    memory_properties: vk::PhysicalDeviceMemoryProperties,
    props: vk::PhysicalDeviceProperties,

    surface_capabilities: vk::SurfaceCapabilitiesKHR,
    surface_formats: Vec<vk::SurfaceFormatKHR>,
    present_modes: Vec<vk::PresentModeKHR>,

    graphics_family_index: u32,
    compute_family_index: u32,

    // Vague guess at how powerful the GPU is
    performance_score: u32,
}

struct Image {
    handle: vk::Image,
    allocation: vk_mem::Allocation,
    view: vk::ImageView,
    format: vk::Format,
}

impl Image {
    pub fn new(
        device: &ash::Device,
        allocator: &vk_mem::Allocator,
        format: vk::Format,
        create_info: &mut vk::ImageCreateInfo,
        view_info: &mut vk::ImageViewCreateInfo,
        allocation_info: &vk_mem::AllocationCreateInfo,
    ) -> Result<Self, vk::Result> {
        create_info.format = format;
        let result = unsafe { allocator.create_image(create_info, allocation_info) };
        if result.is_err() {
            return Err(result.unwrap_err());
        }

        let (handle, allocation) = result.unwrap();
        view_info.image = handle;
        view_info.format = format;

        let view = unsafe { device.create_image_view(view_info, Some(&ALLOCATION_CALLBACKS)) };
        if view.is_err() {
            return Err(view.unwrap_err());
        }
        let view = view.unwrap();

        Ok(Self {
            handle,
            allocation,
            view,
            format,
        })
    }

    pub fn destroy(self, device: &ash::Device, allocator: &vk_mem::Allocator) {
        unsafe {
            device.destroy_image_view(self.view, Some(&ALLOCATION_CALLBACKS));
            allocator.destroy_image(self.handle, self.allocation);
        }
    }

    pub fn choose_fmt(
        instance: &ash::Instance,
        gpu: &GpuInfo,
        formats: &Vec<vk::Format>,
        tiling: vk::ImageTiling,
        required_features: vk::FormatFeatureFlags,
    ) -> vk::Format {
        debug!("Finding format with feature flags {:#?}", required_features);

        let formats: Vec<&vk::Format> = formats
            .iter()
            .filter(|format| {
                let props =
                    unsafe { instance.get_physical_device_format_properties(gpu.device, **format) };
                match tiling {
                    vk::ImageTiling::LINEAR => {
                        props.linear_tiling_features.contains(required_features)
                    }
                    vk::ImageTiling::OPTIMAL => {
                        props.optimal_tiling_features.contains(required_features)
                    }
                    _ => false,
                }
            })
            .collect();

        if !formats.is_empty() {
            *formats[0]
        } else {
            vk::Format::UNDEFINED
        }
    }

    pub fn handle(&self) -> &vk::Image {
        &self.handle
    }

    pub fn allocation(&self) -> &vk_mem::Allocation {
        &self.allocation
    }

    pub fn allocation_mut(&mut self) -> &mut vk_mem::Allocation {
        &mut self.allocation
    }

    pub fn view(&self) -> &vk::ImageView {
        &self.view
    }

    pub fn format(&self) -> vk::Format {
        self.format
    }
}

struct Buffer {
    handle: vk::Buffer,
    allocation: vk_mem::Allocation,
    size: vk::DeviceSize,
}

impl Buffer {
    pub fn new(
        allocator: &vk_mem::Allocator,
        size: vk::DeviceSize,
        usage: vk::BufferUsageFlags,
        flags: vk::MemoryPropertyFlags,
    ) -> Result<Self, vk::Result> {
        let result = unsafe {
            allocator.create_buffer(
                &vk::BufferCreateInfo {
                    size,
                    usage,
                    sharing_mode: vk::SharingMode::EXCLUSIVE,
                    ..Default::default()
                },
                &vk_mem::AllocationCreateInfo {
                    required_flags: flags,
                    ..Default::default()
                },
            )
        };

        if result.is_err() {
            return Err(result.unwrap_err());
        }

        let (handle, allocation) = result.unwrap();
        Ok(Self {
            handle,
            allocation,
            size,
        })
    }

    pub fn copy(
        &self,
        device: &ash::Device,
        queue: &vk::Queue,
        transfer_pool: &vk::CommandPool,
        fence: &vk::Fence,
        destination: &Self,
    ) {
        let transfer_buffer = unsafe {
            vulkan_check!(
                device.allocate_command_buffers(&vk::CommandBufferAllocateInfo {
                    level: vk::CommandBufferLevel::PRIMARY,
                    command_pool: transfer_pool.clone(),
                    command_buffer_count: FRAME_COUNT as u32,
                    ..Default::default()
                })
            )
        }[0];

        unsafe {
            vulkan_check!(device.begin_command_buffer(
                transfer_buffer,
                &vk::CommandBufferBeginInfo {
                    flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT,
                    ..Default::default()
                }
            ));

            device.cmd_copy_buffer(
                transfer_buffer,
                self.handle,
                destination.handle,
                &[vk::BufferCopy {
                    size: self.size,
                    ..Default::default()
                }],
            );

            vulkan_check!(device.end_command_buffer(transfer_buffer));
            vulkan_check!(device.queue_submit(
                queue.clone(),
                &[vk::SubmitInfo {
                    command_buffer_count: 1,
                    p_command_buffers: ptr::addr_of!(transfer_buffer),
                    ..Default::default()
                }],
                fence.clone()
            ));
            vulkan_check!(device.queue_wait_idle(queue.clone()));

            device.free_command_buffers(transfer_pool.clone(), &[transfer_buffer]);
        }
    }

    pub fn destroy(self, allocator: &vk_mem::Allocator) {
        unsafe { allocator.destroy_buffer(self.handle, self.allocation) };
    }

    pub fn handle(&self) -> &vk::Buffer {
        &self.handle
    }

    pub fn allocation(&self) -> &vk_mem::Allocation {
        &self.allocation
    }

    pub fn allocation_mut(&mut self) -> &mut vk_mem::Allocation {
        &mut self.allocation
    }

    pub fn size(&self) -> vk::DeviceSize {
        self.size
    }
}

struct HostBuffer {
    buffer: Buffer,
    address: u64,
}

impl HostBuffer {
    pub fn new(
        allocator: &vk_mem::Allocator,
        size: vk::DeviceSize,
        usage: vk::BufferUsageFlags,
        flags: vk::MemoryPropertyFlags,
    ) -> Result<Self, vk::Result> {
        Self::from_buffer(
            allocator,
            match Buffer::new(allocator, size, usage, flags) {
                Ok(buffer) => buffer,
                Err(err) => return Err(err)
            },
        )
    }

    pub fn from_buffer(allocator: &vk_mem::Allocator, buffer: Buffer) -> Result<Self, vk::Result> {
        let mut self_ =  Self {
            buffer,
            address: 0
        };
        self_.address = unsafe { match allocator.map_memory(&mut self_.buffer.allocation) {
                Ok(address) => address as u64,
                Err(err) => return Err(err)
        } };
        Ok(self_)
    }

    pub fn destroy(mut self, allocator: &vk_mem::Allocator) {
        unsafe { allocator.unmap_memory(&mut self.buffer.allocation) };
        self.buffer.destroy(allocator);
    }
}

pub struct State {
    entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    surface_loader: extensions::khr::Surface,
    swapchain_loader: extensions::khr::Swapchain,
    surface: vk::SurfaceKHR,

    allocator: vk_mem::Allocator,

    gpu: usize,
    gpus: Vec<GpuInfo>,
    graphics_queue: vk::Queue,
    compute_queue: vk::Queue,

    command_pool: vk::CommandPool,
    transfer_pool: vk::CommandPool,
    command_buffers: Vec<vk::CommandBuffer>,

    fences: Vec<vk::Fence>,
    acquire_semaphores: Vec<vk::Semaphore>,
    render_complete_semaphores: Vec<vk::Semaphore>,

    swapchain: vk::SwapchainKHR,
    swapchain_images: Vec<vk::Image>,
    swapchain_views: Vec<vk::ImageView>,
    surface_format: vk::SurfaceFormatKHR,
    present_mode: vk::PresentModeKHR,
    swapchain_extent: vk::Extent2D,

    depth_image: Image,

    descriptor_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,

    uniform_buffers: Vec<HostBuffer>,
}

impl State {
    unsafe extern "system" fn debug_log(
        severity: vk::DebugUtilsMessageSeverityFlagsEXT,
        types: vk::DebugUtilsMessageTypeFlagsEXT,
        callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT,
        #[allow(unused_variables)] user_data: *mut ffi::c_void,
    ) -> u32 {
        let log_level = match severity {
            vk::DebugUtilsMessageSeverityFlagsEXT::VERBOSE => log::Level::Trace,
            vk::DebugUtilsMessageSeverityFlagsEXT::INFO => log::Level::Debug,
            vk::DebugUtilsMessageSeverityFlagsEXT::WARNING => log::Level::Info,
            vk::DebugUtilsMessageSeverityFlagsEXT::ERROR => log::Level::Warn,
            _ => log::Level::Debug,
        };

        let mut location = String::new();

        if types.contains(vk::DebugUtilsMessageTypeFlagsEXT::GENERAL) {
            location += "GENERAL ";
        }
        if types.contains(vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE) {
            location += "PERFORMANCE ";
        }
        if types.contains(vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION) {
            location += "VALIDATION ";
        }

        let message_ptr = (*callback_data).p_message as *const ffi::c_char;
        let message_raw = unsafe { ffi::CStr::from_ptr(message_ptr) };
        let message = message_raw.to_str().unwrap();
        log!(log_level, "VULKAN {}MESSAGE: {}", location, message);

        vk::TRUE
    }

    fn create_instance(entry: &ash::Entry) -> ash::Instance {
        debug!("Creating Vulkan instance");

        let app_name = ffi::CString::new(crate::GAME_NAME).unwrap();
        let engine_name = ffi::CString::new("Purpl Engine").unwrap();
        let app_info = vk::ApplicationInfo {
            p_application_name: app_name.as_ptr() as *const ffi::c_char,
            application_version: vk::make_api_version(
                0,
                crate::GAME_VERSION_MAJOR.into(),
                crate::GAME_VERSION_MINOR.into(),
                crate::GAME_VERSION_PATCH.into(),
            ),
            p_engine_name: engine_name.as_ptr() as *const ffi::c_char,
            engine_version: 2,
            api_version: vk::make_api_version(0, 1, 3, 0),
            ..Default::default()
        };

        let extensions = [
            extensions::khr::Surface::name(),
            #[cfg(feature = "graphics_debug")]
            extensions::ext::DebugUtils::name(),
            #[cfg(windows)]
            extensions::khr::Win32Surface::name(),
            #[cfg(unix)]
            extensions::khr::XcbSurface::name(),
        ];

        let validation_layers = ["VK_LAYER_KHRONOS_validation"];

        let extensions_raw: Vec<*const ffi::c_char> = extensions
            .iter()
            .map(|ext_name| ext_name.as_ptr())
            .collect();
        let layers_cstr: Vec<ffi::CString> = validation_layers
            .iter()
            .map(|layer_name| ffi::CString::new(*layer_name).unwrap())
            .collect();
        let layers_raw: Vec<*const ffi::c_char> = layers_cstr
            .iter()
            .map(|layer_name| layer_name.as_ptr())
            .collect();

        let debug_messenger_info = vk::DebugUtilsMessengerCreateInfoEXT {
            message_severity: vk::DebugUtilsMessageSeverityFlagsEXT::VERBOSE
                | vk::DebugUtilsMessageSeverityFlagsEXT::INFO
                | vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
            message_type: vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE
                | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION,
            pfn_user_callback: Some(Self::debug_log),
            ..Default::default()
        };

        let mut create_info = vk::InstanceCreateInfo {
            p_application_info: ptr::addr_of!(app_info),
            enabled_extension_count: extensions.len() as u32,
            pp_enabled_extension_names: extensions_raw.as_ptr(),
            #[cfg(feature = "graphics_debug")]
            enabled_layer_count: layers_raw.len() as u32,
            #[cfg(feature = "graphics_debug")]
            pp_enabled_layer_names: layers_raw.as_ptr(),
            #[cfg(feature = "graphics_debug")]
            p_next: ptr::addr_of!(debug_messenger_info) as *const ffi::c_void,
            ..Default::default()
        };

        let result = unsafe { entry.create_instance(&create_info, Some(&ALLOCATION_CALLBACKS)) };
        let instance = match result {
            Ok(val) => val,
            Err(err) if err == vk::Result::ERROR_LAYER_NOT_PRESENT => {
                debug!("Validation layers not available, retrying with them disabled");
                create_info.enabled_layer_count = 0;
                unsafe {
                    vulkan_check!(entry.create_instance(&create_info, Some(&ALLOCATION_CALLBACKS)))
                }
            }
            Err(err) =>
                panic!("Vulkan call entry.create_instance(&create_info, Some(&ALLOCATION_CALLBACKS)) failed: {err}")
        };

        debug!(
            "Created Vulkan instance {:?} successfully",
            instance.handle()
        );
        instance
    }

    fn get_required_device_exts() -> [&'static ffi::CStr; 1] {
        [
            // TODO: fix when vk-mem supports a version of ash with this extension
            //ffi::CStr::from_bytes_with_nul(b"VK_EXT_shader_object\0").unwrap(),
            extensions::khr::Swapchain::name(),
        ]
    }

    fn get_gpus(
        instance: &ash::Instance,
        surface_loader: &extensions::khr::Surface,
        surface: &vk::SurfaceKHR,
    ) -> Vec<GpuInfo> {
        debug!("Enumerating devices");
        let devices = unsafe { vulkan_check!(instance.enumerate_physical_devices()) };
        let devices = devices
            .into_iter()
            .enumerate()
            .map(|(i, device)| (i + 1, device));

        let mut gpus: Vec<GpuInfo> = Vec::new();
        let mut usable_count = 0;
        for (i, device) in devices {
            debug!("Getting information for device {i}");
            let queue_family_props =
                unsafe { instance.get_physical_device_queue_family_properties(device) };
            if queue_family_props.is_empty() {
                error!("Ignoring GPU {i} because it has no queue families");
                continue;
            }

            let Some((graphics_family_idx, _)) = queue_family_props
                .iter()
                .enumerate()
                .map(|(i, props)| (i as u32, props))
                .find(|(_, props)| {
                    props.queue_count >= 1
                        && props.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                }) else {
                error!("Failed to get all required queue familiy indices for device {i}");
                continue;
            };

            let Some((compute_family_idx, _)) = queue_family_props
                .iter()
                .enumerate()
                .map(|(i, props)| (i as u32, props))
                .find(|(_, props)| {
                    props.queue_count >= 1
                        && props.queue_flags.contains(vk::QueueFlags::COMPUTE)
                }) else {
                error!("Failed to get all required queue familiy indices for device {i}");
                continue;
            };

            let extension_props = unsafe { instance.enumerate_device_extension_properties(device) };
            match extension_props {
                Ok(val) if val.len() >= Self::get_required_device_exts().len() => val,
                Ok(val) => {
                    error!(
                        "Ignoring device {} because it has {} extension(s) when {} are required",
                        i,
                        val.len(),
                        Self::get_required_device_exts().len()
                    );
                    continue;
                }
                Err(err) => {
                    error!("Failed to get extension properties for device {i}: {err}");
                    continue;
                }
            };

            let surface_caps = unsafe {
                surface_loader.get_physical_device_surface_capabilities(device, *surface)
            };
            let surface_caps = match surface_caps {
                Ok(val) => val,
                Err(err) => {
                    error!("Failed to get surface capabilities for device {i}: {err}");
                    continue;
                }
            };

            let fmts =
                unsafe { surface_loader.get_physical_device_surface_formats(device, *surface) };
            let surface_fmts = match fmts {
                Ok(val) if !val.is_empty() => val,
                Ok(_) => {
                    error!("Ignoring device {i} because it has no surface formats");
                    continue;
                }
                Err(err) => {
                    error!("Failed to get surface formats for device {i}: {err}");
                    continue;
                }
            };

            let present_modes = unsafe {
                surface_loader.get_physical_device_surface_present_modes(device, *surface)
            };
            let present_modes = match present_modes {
                Ok(val) if !val.is_empty() => val,
                Ok(_) => {
                    error!("Ignoring device {i} because it has no present modes");
                    continue;
                }
                Err(err) => {
                    error!("Failed to get present modes for device {i}: {err}");
                    continue;
                }
            };

            let mem_props = unsafe { instance.get_physical_device_memory_properties(device) };
            let props = unsafe { instance.get_physical_device_properties(device) };

            let mut score = (mem_props.memory_heaps[0].size / 1_000) as u32
                + (props.limits.max_viewport_dimensions[0] as u64
                    * props.limits.max_viewport_dimensions[1] as u64
                    / 1_000) as u32;
            if [
                vk::PhysicalDeviceType::DISCRETE_GPU,
                vk::PhysicalDeviceType::VIRTUAL_GPU,
            ]
            .contains(&props.device_type)
            {
                score *= 10;
            } else if props.device_type == vk::PhysicalDeviceType::INTEGRATED_GPU {
                score *= 2;
            }

            let name = unsafe {
                ffi::CStr::from_ptr(props.device_name.as_ptr())
                    .to_str()
                    .unwrap()
            };

            debug!("Device {i}:");
            debug!("\tName: {name}");
            debug!("\tScore: {score}");
            debug!("\tType: {:#?}", props.device_type);
            debug!("\tHandle: {:#?}", device);

            gpus.push(GpuInfo {
                device,
                memory_properties: mem_props,
                props,
                surface_capabilities: surface_caps,
                surface_formats: surface_fmts,
                present_modes,
                graphics_family_index: graphics_family_idx,
                compute_family_index: compute_family_idx,
                performance_score: score,
            });

            usable_count += 1;
        }

        debug!(
            "Got information of {} device(s) (of which {} are usable)",
            gpus.len(),
            usable_count
        );
        if usable_count < 1 {
            panic!("Could not find any usable Vulkan devices");
        }

        debug!("Sorting device(s)");
        gpus.sort_by_key(|gpu| -(gpu.performance_score as i32));

        gpus
    }

    fn create_device(
        instance: &ash::Instance,
        gpu: &GpuInfo,
    ) -> (ash::Device, vk::Queue, vk::Queue) {
        debug!("Creating logical device");

        let queue_priority: f32 = 1.0;
        let graphics_queue_info = vk::DeviceQueueCreateInfo {
            queue_family_index: gpu.graphics_family_index,
            p_queue_priorities: ptr::addr_of!(queue_priority),
            queue_count: 1,
            ..Default::default()
        };
        let present_queue_info = vk::DeviceQueueCreateInfo {
            queue_family_index: gpu.compute_family_index,
            p_queue_priorities: ptr::addr_of!(queue_priority),
            queue_count: 1,
            ..Default::default()
        };
        let queue_create_infos = if gpu.graphics_family_index != gpu.compute_family_index {
            vec![graphics_queue_info, present_queue_info]
        } else {
            vec![graphics_queue_info]
        };

        let device_features = vk::PhysicalDeviceFeatures {
            ..Default::default()
        };
        let device_13_features = vk::PhysicalDeviceVulkan13Features {
            dynamic_rendering: vk::TRUE,
            ..Default::default()
        };

        let device_exts: Vec<*const ffi::c_char> = Self::get_required_device_exts()
            .iter()
            .map(|ext_name| ext_name.as_ptr())
            .collect();

        let device_info = vk::DeviceCreateInfo {
            p_queue_create_infos: queue_create_infos.as_ptr(),
            queue_create_info_count: queue_create_infos.len() as u32,
            p_enabled_features: ptr::addr_of!(device_features),
            pp_enabled_extension_names: device_exts.as_ptr(),
            enabled_extension_count: device_exts.len() as u32,
            p_next: ptr::addr_of!(device_13_features) as *const ffi::c_void,
            ..Default::default()
        };

        let device = unsafe {
            vulkan_check!(instance.create_device(
                gpu.device,
                &device_info,
                Some(&ALLOCATION_CALLBACKS)
            ))
        };

        debug!("Created logical device {:#?} successfully", device.handle());

        debug!("Retrieving queues");
        let graphics_queue = unsafe { device.get_device_queue(gpu.graphics_family_index, 0) };
        let present_queue = unsafe { device.get_device_queue(gpu.compute_family_index, 0) };
        debug!(
            "Got graphics queue {:#?} and present queue {:#?}",
            graphics_queue, present_queue
        );

        (device, graphics_queue, present_queue)
    }

    fn create_fences(device: &ash::Device) -> Vec<vk::Fence> {
        debug!("Creating {FRAME_COUNT} command fences");

        let fence_create_info = vk::FenceCreateInfo {
            flags: vk::FenceCreateFlags::SIGNALED,
            ..Default::default()
        };
        let mut fences = Vec::new();
        for _ in 0..FRAME_COUNT {
            fences.push(unsafe {
                vulkan_check!(device.create_fence(&fence_create_info, Some(&ALLOCATION_CALLBACKS)))
            })
        }

        fences
    }

    fn create_semaphores(device: &ash::Device) -> (Vec<vk::Semaphore>, Vec<vk::Semaphore>) {
        debug!("Creating {} semaphores", FRAME_COUNT * 2);

        let semaphore_create_info = vk::SemaphoreCreateInfo {
            ..Default::default()
        };
        let mut acquire_semaphores = Vec::new();
        let mut complete_semaphores = Vec::new();
        for _ in 0..FRAME_COUNT {
            acquire_semaphores.push(unsafe {
                vulkan_check!(
                    device.create_semaphore(&semaphore_create_info, Some(&ALLOCATION_CALLBACKS))
                )
            });
            complete_semaphores.push(unsafe {
                vulkan_check!(
                    device.create_semaphore(&semaphore_create_info, Some(&ALLOCATION_CALLBACKS))
                )
            });
        }

        (acquire_semaphores, complete_semaphores)
    }

    fn create_cmd_pools(device: &ash::Device, gpu: &GpuInfo) -> (vk::CommandPool, vk::CommandPool) {
        let main_pool_info = vk::CommandPoolCreateInfo {
            flags: vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER,
            queue_family_index: gpu.graphics_family_index,
            ..Default::default()
        };
        let transfer_pool_info = vk::CommandPoolCreateInfo {
            flags: main_pool_info.flags | vk::CommandPoolCreateFlags::TRANSIENT,
            ..main_pool_info
        };

        let main_pool = unsafe {
            vulkan_check!(device.create_command_pool(&main_pool_info, Some(&ALLOCATION_CALLBACKS)))
        };
        let transfer_pool = unsafe {
            vulkan_check!(
                device.create_command_pool(&transfer_pool_info, Some(&ALLOCATION_CALLBACKS))
            )
        };

        (main_pool, transfer_pool)
    }

    fn allocate_command_buffers(
        device: &ash::Device,
        cmd_pool: &vk::CommandPool,
    ) -> Vec<vk::CommandBuffer> {
        debug!("Allocating command buffers");

        unsafe {
            vulkan_check!(
                device.allocate_command_buffers(&vk::CommandBufferAllocateInfo {
                    level: vk::CommandBufferLevel::PRIMARY,
                    command_pool: cmd_pool.clone(),
                    command_buffer_count: FRAME_COUNT as u32,
                    ..Default::default()
                })
            )
        }
    }

    fn create_allocator(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
    ) -> vk_mem::Allocator {
        debug!("Creating Vulkan allocator");
        vulkan_check!(vk_mem::Allocator::new(vk_mem::AllocatorCreateInfo::new(
            Rc::from(instance),
            Rc::from(device),
            physical_device
        )))
    }

    fn choose_surface_format(gpu: &GpuInfo) -> vk::SurfaceFormatKHR {
        debug!("Choosing surface format");

        if gpu.surface_formats.len() == 1 && gpu.surface_formats[0].format == vk::Format::UNDEFINED
        {
            return vk::SurfaceFormatKHR {
                format: vk::Format::B8G8R8A8_UNORM,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            };
        }

        for &format in &gpu.surface_formats {
            if format.format == vk::Format::B8G8R8A8_UNORM
                && format.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
            {
                return format;
            }
        }

        gpu.surface_formats[0]
    }

    fn choose_present_mode(gpu: &GpuInfo) -> vk::PresentModeKHR {
        debug!("Choosing presentation mode");

        for &mode in &gpu.present_modes {
            if mode == vk::PresentModeKHR::MAILBOX {
                return mode;
            }
        }

        vk::PresentModeKHR::FIFO
    }

    fn create_swapchain(
        device: &ash::Device,
        gpu: &GpuInfo,
        surface: &vk::SurfaceKHR,
        present_mode: &vk::PresentModeKHR,
        surface_format: &vk::SurfaceFormatKHR,
        image_extent: &vk::Extent2D,
        loader: &extensions::khr::Swapchain,
    ) -> (vk::SwapchainKHR, Vec<vk::Image>, Vec<vk::ImageView>) {
        debug!("Creating swap chain");

        let queue_family_indices = [gpu.graphics_family_index, gpu.compute_family_index];
        let (image_sharing_mode, queue_family_index_count, p_queue_family_indices) =
            if gpu.graphics_family_index != gpu.compute_family_index {
                (
                    vk::SharingMode::CONCURRENT,
                    2,
                    queue_family_indices.as_ptr(),
                )
            } else {
                (vk::SharingMode::EXCLUSIVE, 0, ptr::null())
            };

        let swapchain_info = vk::SwapchainCreateInfoKHR {
            surface: *surface,
            min_image_count: FRAME_COUNT as u32,
            image_format: surface_format.format,
            image_color_space: surface_format.color_space,
            image_extent: *image_extent,
            image_array_layers: 1,

            image_usage: vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,

            image_sharing_mode,
            queue_family_index_count,
            p_queue_family_indices,

            pre_transform: vk::SurfaceTransformFlagsKHR::IDENTITY,
            composite_alpha: vk::CompositeAlphaFlagsKHR::OPAQUE,
            present_mode: *present_mode,

            clipped: vk::TRUE,

            ..Default::default()
        };

        let swapchain = unsafe {
            vulkan_check!(loader.create_swapchain(&swapchain_info, Some(&ALLOCATION_CALLBACKS)))
        };
        let images = unsafe { vulkan_check!(loader.get_swapchain_images(swapchain)) };

        debug!("Creating swap chain image views");
        let mut views = Vec::new();
        let mut i = 0;
        views.resize_with(FRAME_COUNT, || unsafe {
            let image = images[i];
            i += 1;
            vulkan_check!(device.create_image_view(
                &vk::ImageViewCreateInfo {
                    image,

                    view_type: vk::ImageViewType::TYPE_2D,

                    format: surface_format.format,

                    components: vk::ComponentMapping {
                        r: vk::ComponentSwizzle::R,
                        g: vk::ComponentSwizzle::G,
                        b: vk::ComponentSwizzle::B,
                        a: vk::ComponentSwizzle::A,
                    },

                    subresource_range: vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    },

                    ..Default::default()
                },
                Some(&ALLOCATION_CALLBACKS)
            ))
        });

        (swapchain, images, views)
    }

    fn destroy_swapchain(
        device: &ash::Device,
        loader: &extensions::khr::Swapchain,
        swapchain: vk::SwapchainKHR,
        swapchain_views: Vec<vk::ImageView>,
    ) {
        debug!("Destroying {FRAME_COUNT} swap chain image views");
        swapchain_views.iter().for_each(|view| unsafe {
            device.destroy_image_view(*view, Some(&ALLOCATION_CALLBACKS))
        });

        debug!("Destroying swap chain {:#?}", swapchain);
        unsafe { loader.destroy_swapchain(swapchain, Some(&ALLOCATION_CALLBACKS)) };
    }

    fn create_render_targets(
        instance: &ash::Instance,
        gpu: &GpuInfo,
        device: &ash::Device,
        allocator: &vk_mem::Allocator,
    ) -> (Image) {
        debug!("Creating render target images");

        let depth_formats = vec![
            vk::Format::D32_SFLOAT_S8_UINT,
            vk::Format::D24_UNORM_S8_UINT,
        ];

        let depth_format = Image::choose_fmt(
            instance,
            gpu,
            &depth_formats,
            vk::ImageTiling::OPTIMAL,
            vk::FormatFeatureFlags::DEPTH_STENCIL_ATTACHMENT,
        );
        if depth_format == vk::Format::UNDEFINED {
            panic!("No supported depth formats found");
        }

        debug!("Creating depth image");
        let (width, height) = crate::platform::video::get_size();
        let depth_image = vulkan_check!(Image::new(
            device,
            allocator,
            depth_format,
            &mut vk::ImageCreateInfo {
                extent: vk::Extent3D {
                    width,
                    height,
                    depth: 1
                },
                mip_levels: 1,
                array_layers: 1,
                samples: vk::SampleCountFlags::TYPE_1,
                usage: vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT,
                image_type: vk::ImageType::TYPE_2D,
                ..Default::default()
            },
            &mut vk::ImageViewCreateInfo {
                view_type: vk::ImageViewType::TYPE_2D,
                subresource_range: vk::ImageSubresourceRange {
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                    aspect_mask: vk::ImageAspectFlags::DEPTH,
                    ..Default::default()
                },
                ..Default::default()
            },
            &vk_mem::AllocationCreateInfo {
                usage: vk_mem::MemoryUsage::AutoPreferDevice,
                ..Default::default()
            }
        ));

        (depth_image)
    }

    fn create_descriptor_layout(device: &ash::Device) -> vk::DescriptorSetLayout {
        debug!("Creating descriptor set layout");

        let ubo_layout_binding = vk::DescriptorSetLayoutBinding {
            binding: 0,
            descriptor_type: vk::DescriptorType::UNIFORM_BUFFER,
            descriptor_count: 1,
            stage_flags: vk::ShaderStageFlags::VERTEX,
            ..Default::default()
        };

        let descriptor_layout_info = vk::DescriptorSetLayoutCreateInfo {
            p_bindings: ptr::addr_of!(ubo_layout_binding),
            binding_count: 1,
            ..Default::default()
        };

        unsafe {
            vulkan_check!(device
                .create_descriptor_set_layout(&descriptor_layout_info, Some(&ALLOCATION_CALLBACKS)))
        }
    }

    fn destroy_render_targets(
        device: &ash::Device,
        allocator: &vk_mem::Allocator,
        depth_image: Image,
    ) {
        debug!("Destroying render target images");
        debug!("Destroying depth image");
        depth_image.destroy(device, allocator);
    }

    fn recreate_swapchain(
        &mut self,
        swapchain: vk::SwapchainKHR,
        swapchain_views: Vec<vk::ImageView>,
        depth_image: Image,
    ) {
        debug!("Recreating swap chain");

        Self::destroy_render_targets(&self.device, &self.allocator, depth_image);
        Self::destroy_swapchain(
            &self.device,
            &self.swapchain_loader,
            swapchain,
            swapchain_views,
        );
        (self.swapchain, self.swapchain_images, self.swapchain_views) = Self::create_swapchain(
            &self.device,
            &self.gpus[self.gpu],
            &self.surface,
            &self.present_mode,
            &self.surface_format,
            &self.swapchain_extent,
            &self.swapchain_loader,
        );
        (self.depth_image) = Self::create_render_targets(
            &self.instance,
            &self.gpus[self.gpu],
            &self.device,
            &self.allocator,
        );
    }

    fn create_descriptor_pool(device: &ash::Device) -> vk::DescriptorPool {
        debug!("Creating descriptor pool");

        const POOL_SIZES: [vk::DescriptorPoolSize; 11] = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLER,
                descriptor_count: 1000,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 1000,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLED_IMAGE,
                descriptor_count: 1000,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: 1000,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_TEXEL_BUFFER,
                descriptor_count: 1000,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_TEXEL_BUFFER,
                descriptor_count: 1000,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER,
                descriptor_count: 1000,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 1000,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC,
                descriptor_count: 1000,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER_DYNAMIC,
                descriptor_count: 1000,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::INPUT_ATTACHMENT,
                descriptor_count: 1000,
            },
        ];

        unsafe {
            vulkan_check!(device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo {
                    flags: vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET,
                    pool_size_count: POOL_SIZES.len() as u32,
                    p_pool_sizes: POOL_SIZES.as_ptr(),
                    max_sets: 1000 * POOL_SIZES.len() as u32,
                    ..Default::default()
                },
                Some(&ALLOCATION_CALLBACKS)
            ))
        }
    }

    fn allocate_uniform_buffers(allocator: &vk_mem::Allocator) -> Vec<HostBuffer> {
        debug!("Allocating {FRAME_COUNT} uniform buffers");
        let mut buffers = Vec::new();
        buffers.resize_with(3, || {
            vulkan_check!(HostBuffer::new(
                allocator,
                mem::size_of::<crate::engine::rendersystem::UniformData>() as u64,
                vk::BufferUsageFlags::UNIFORM_BUFFER,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            ))
        });

        buffers
    }

    fn allocate_descriptor_sets(device: &ash::Device, layout: &vk::DescriptorSetLayout, pool: &vk::DescriptorPool, uniform_buffers: &Vec<HostBuffer>) -> Vec<vk::DescriptorSet {
        debug!("Allocating {FRAME_COUNT} descriptor sets");

        let mut layouts = Vec::new();
        layouts.resize_with(3, || {layout});

        let descriptor_sets = unsafe {
            vulkan_check!(
                device.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo {
                        descriptor_pool: pool,
                        descriptor_set_count: FRAME_COUNT,
                        p_set_layouts: layouts.as_slice(),
                        ..Default::default()
                    }
                )
            )
        };
    }

    pub fn init() -> Self {
        debug!("Vulkan initialization started");

        debug!("Loading Vulkan library");
        let entry = unsafe { vulkan_check!(ash::Entry::load()) };

        let instance = Self::create_instance(&entry);
        let surface_loader = extensions::khr::Surface::new(&entry, &instance);
        let surface = crate::platform::video::create_vulkan_surface(
            &entry,
            &instance,
            Some(&ALLOCATION_CALLBACKS),
        );
        let gpus = Self::get_gpus(&instance, &surface_loader, &surface);
        let gpu = 0;
        let (device, graphics_queue, compute_queue) = Self::create_device(&instance, &gpus[gpu]);
        let (command_pool, transfer_pool) = Self::create_cmd_pools(&device, &gpus[gpu]);
        let command_buffers = Self::allocate_command_buffers(&device, &command_pool);
        let allocator = Self::create_allocator(&instance, &device, gpus[gpu].device);
        let fences = Self::create_fences(&device);
        let (acquire_semaphores, render_complete_semaphores) = Self::create_semaphores(&device);
        let surface_format = Self::choose_surface_format(&gpus[gpu]);
        let present_mode = Self::choose_present_mode(&gpus[gpu]);
        let video_size = crate::platform::video::get_size();
        let swapchain_extent = vk::Extent2D {
            width: video_size.0,
            height: video_size.1,
        };
        let swapchain_loader = extensions::khr::Swapchain::new(&instance, &device);
        let (swapchain, swapchain_images, swapchain_views) = Self::create_swapchain(
            &device,
            &gpus[gpu],
            &surface,
            &present_mode,
            &surface_format,
            &swapchain_extent,
            &swapchain_loader,
        );
        let (depth_image) = Self::create_render_targets(&instance, &gpus[gpu], &device, &allocator);
        let descriptor_layout = Self::create_descriptor_layout(&device);
        let descriptor_pool = Self::create_descriptor_pool(&device);
        let uniform_buffers = Self::allocate_uniform_buffers(&allocator);

        debug!("Vulkan initialization succeeded");

        let mut _self = Self {
            entry,
            instance,
            device,
            surface_loader,
            swapchain_loader,
            surface,
            gpu,
            gpus,
            graphics_queue,
            compute_queue,
            command_pool,
            transfer_pool,
            command_buffers,
            fences,
            acquire_semaphores,
            render_complete_semaphores,
            allocator,
            swapchain,
            swapchain_images,
            swapchain_views,
            surface_format,
            present_mode,
            swapchain_extent,
            depth_image,
            descriptor_layout,
            descriptor_pool,
            uniform_buffers,
        };
        _self.set_gpu(_self.gpu);

        _self
    }

    pub fn begin_cmds(&mut self) {}

    pub fn present(&mut self) {}

    pub fn shutdown(mut self) {
        debug!("Vulkan shutdown started");

        unsafe {
            debug!("Freeing {FRAME_COUNT} uniform buffers");
            for _ in 0..self.uniform_buffers.len() {
                self.uniform_buffers.remove(0).destroy(&self.allocator)
            }

            debug!("Destroying descriptor pool {:#?}", self.descriptor_pool);
            self.device.destroy_descriptor_pool(self.descriptor_pool, Some(&ALLOCATION_CALLBACKS));

            debug!(
                "Destroying descriptor set layout {:#?}",
                self.descriptor_layout
            );
            self.device
                .destroy_descriptor_set_layout(self.descriptor_layout, Some(&ALLOCATION_CALLBACKS));

            Self::destroy_render_targets(&self.device, &self.allocator, self.depth_image);
            Self::destroy_swapchain(
                &self.device,
                &self.swapchain_loader,
                self.swapchain,
                self.swapchain_views,
            );
            debug!("Destroying {} semaphores", FRAME_COUNT * 2);
            self.render_complete_semaphores
                .iter()
                .for_each(|semaphore| {
                    self.device
                        .destroy_semaphore(*semaphore, Some(&ALLOCATION_CALLBACKS))
                });

            debug!("Destroying {FRAME_COUNT} fences");
            self.fences.iter().for_each(|fence| {
                self.device
                    .destroy_fence(*fence, Some(&ALLOCATION_CALLBACKS))
            });
            self.acquire_semaphores.iter().for_each(|semaphore| {
                self.device
                    .destroy_semaphore(*semaphore, Some(&ALLOCATION_CALLBACKS))
            });
            debug!("Destroying transfer command pool {:#?}", self.transfer_pool);
            self.device
                .destroy_command_pool(self.transfer_pool, Some(&ALLOCATION_CALLBACKS));
            debug!("Destroying command pool {:#?}", self.command_pool);
            self.device
                .destroy_command_pool(self.command_pool, Some(&ALLOCATION_CALLBACKS));
            debug!("Destroying logical device {:#?}", self.device.handle());
            self.device.destroy_device(Some(&ALLOCATION_CALLBACKS));
            debug!("Destroying surface {:#?}", self.surface);
            self.surface_loader
                .destroy_surface(self.surface, Some(&ALLOCATION_CALLBACKS));
            debug!("Destroying instance {:#?}", self.instance.handle());
            self.instance.destroy_instance(Some(&ALLOCATION_CALLBACKS));
        }

        debug!("Vulkan shutdown succeeded");
    }

    pub fn set_gpu(&mut self, gpu_idx: usize) -> usize {
        let old_idx = self.gpu;
        if gpu_idx < self.gpus.len() {
            self.gpu = gpu_idx;
            let gpu = &self.gpus[self.gpu];

            let name = unsafe {
                ffi::CStr::from_ptr(gpu.props.device_name.as_ptr())
                    .to_str()
                    .unwrap()
            };
            debug!(
                "Selected {:#?} device {}, {} [{:04x}:{:04x}] with score {}",
                gpu.props.device_type,
                gpu_idx,
                name,
                gpu.props.vendor_id,
                gpu.props.device_id,
                gpu.performance_score
            );
        }

        old_idx
    }
}
