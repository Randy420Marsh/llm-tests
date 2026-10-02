//! Vulkan-based GPU benchmark focusing on compute latency and throughput
//! This implementation uses ash 0.37 and focuses on headless compute workloads to avoid
//! platform-specific surface/swapchain complexities.

use anyhow::{anyhow, Result};
use ash::{vk, Device, Entry, Instance};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use crate::cancel::{self, CancelFlag};
use crate::progress::{self, SharedProgress, SharedResults};
use crate::sensors::{Sampler, Telemetry};
use crate::timer::HighResTimer;

/// ALU steps (one multiply + one add each) executed by every shader invocation
const SHADER_ALU_STEPS: u32 = 32;
const SHADER_LOCAL_SIZE: u32 = 64;
const LCG_MUL: u32 = 1_664_525;
const LCG_ADD: u32 = 1_013_904_223;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuBenchmarkConfig {
    pub workload_sizes: Vec<u64>,
    pub iterations: u32,
    pub warmup_iterations: u32,
    /// Keep dispatching for at least this long per workload size (after `iterations` are done).
    /// One dispatch takes microseconds, so 100 of them finish inside a single sensor sample and the
    /// GPU would read ~0 % load; sustained work is what lets GPU load, clocks, power and temperature
    /// show up. 0 = stop after `iterations`.
    #[serde(default)]
    pub min_sample_ms: u64,
}

/// Upper bound on timed dispatches per size so a very fast device cannot run out of memory
const MAX_TIMED_DISPATCHES: usize = 1_000_000;

impl Default for GpuBenchmarkConfig {
    fn default() -> Self {
        Self {
            workload_sizes: vec![1 << 20, 1 << 22, 1 << 24], // 1M, 4M, 16M elements
            iterations: 100,
            warmup_iterations: 10,
            min_sample_ms: 2000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuBenchmarkResult {
    pub workload_size: u64,
    pub avg_latency_ms: f64,
    pub min_latency_ms: f64,
    pub max_latency_ms: f64,
    pub std_dev_ms: f64,
    pub percentile_50_ms: f64,
    pub percentile_95_ms: f64,
    pub percentile_99_ms: f64,
    pub throughput_geops: f64,
    /// GPU/VRAM/CPU readings taken while this size ran
    #[serde(default)]
    pub telemetry: Telemetry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuBenchmarkSummary {
    pub results: Vec<GpuBenchmarkResult>,
    pub config: GpuBenchmarkConfig,
    pub system_info: crate::system_info::SystemInfo,
    pub timestamp: String,
    pub vulkan_info: VulkanInfo,
    /// Sizes this device cannot run (bigger than its dispatch or storage-buffer limits); the others still ran
    #[serde(default)]
    pub skipped_sizes: Vec<u64>,
}

/// The device cannot run a workload of this size (its limits are too small)
#[derive(Debug)]
pub struct SizeUnsupported {
    pub size: u64,
    pub reason: &'static str,
}

impl std::fmt::Display for SizeUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Workload of {} elements exceeds {}", self.size, self.reason)
    }
}

impl std::error::Error for SizeUnsupported {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VulkanInfo {
    pub api_version: String,
    pub driver_version: String,
    pub device_name: String,
    pub device_type: String,
    pub vendor_id: u32,
    pub device_id: u32,
}

pub struct GpuBenchmark {
    config: GpuBenchmarkConfig,
    _entry: Entry,
    instance: Instance,
    device: Device,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    queue_family_index: u32,
    timer: HighResTimer,
    cancel: CancelFlag,
    progress: Option<SharedProgress>,
    partial: Option<SharedResults<GpuBenchmarkResult>>,
    sensors: Option<std::sync::Arc<Sampler>>,
}

impl Drop for GpuBenchmark {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// Vulkan objects for one compute workload; everything is released on drop
struct Resources {
    device: Device,
    shader: vk::ShaderModule,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    desc_pool: vk::DescriptorPool,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    cmd_pool: vk::CommandPool,
    fence: vk::Fence,
    cmd: vk::CommandBuffer,
    elements: u64,
}

impl Drop for Resources {
    fn drop(&mut self) {
        // Destroying VK_NULL_HANDLE objects is a no-op, so partially built sets are fine
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_descriptor_pool(self.desc_pool, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.pipeline_layout, None);
            self.device.destroy_descriptor_set_layout(self.set_layout, None);
            self.device.destroy_shader_module(self.shader, None);
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// Build the compute shader: every invocation runs a chain of LCG steps seeded with its
/// linear index and stores the result to `data[index]`. Returns SPIR-V words.
fn build_shader(row_stride: u32) -> Result<Vec<u32>> {
    use rspirv::binary::Assemble;
    use rspirv::dr::{Builder, Operand};
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };

    let mut b = Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let fn_ty = b.type_function(void, vec![]);
    let uint = b.type_int(32, 0);
    let uvec3 = b.type_vector(uint, 3);
    let rt_array = b.type_runtime_array(uint);
    b.decorate(rt_array, Decoration::ArrayStride, [Operand::LiteralBit32(4)]);
    let block = b.type_struct(vec![rt_array]);
    b.decorate(block, Decoration::Block, []);
    b.member_decorate(block, 0, Decoration::Offset, [Operand::LiteralBit32(0)]);

    let ptr_block = b.type_pointer(None, StorageClass::StorageBuffer, block);
    let data = b.variable(ptr_block, None, StorageClass::StorageBuffer, None);
    b.decorate(data, Decoration::DescriptorSet, [Operand::LiteralBit32(0)]);
    b.decorate(data, Decoration::Binding, [Operand::LiteralBit32(0)]);

    let ptr_in = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn, [Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);

    let c0 = b.constant_bit32(uint, 0);
    let c_stride = b.constant_bit32(uint, row_stride);
    let c_mul = b.constant_bit32(uint, LCG_MUL);
    let c_add = b.constant_bit32(uint, LCG_ADD);
    let ptr_elem = b.type_pointer(None, StorageClass::StorageBuffer, uint);

    let main_fn = b.begin_function(void, None, FunctionControl::NONE, fn_ty)?;
    b.begin_block(None)?;
    let gid = b.load(uvec3, None, gid_var, None, [])?;
    let x = b.composite_extract(uint, None, gid, [0])?;
    let y = b.composite_extract(uint, None, gid, [1])?;
    let row = b.i_mul(uint, None, y, c_stride)?;
    let index = b.i_add(uint, None, row, x)?;
    let mut acc = index;
    for _ in 0..SHADER_ALU_STEPS {
        let m = b.i_mul(uint, None, acc, c_mul)?;
        acc = b.i_add(uint, None, m, c_add)?;
    }
    let dst = b.access_chain(ptr_elem, None, data, [c0, index])?;
    b.store(dst, acc, None, [])?;
    b.ret()?;
    b.end_function()?;

    b.entry_point(ExecutionModel::GLCompute, main_fn, "main", vec![gid_var]);
    b.execution_mode(main_fn, ExecutionMode::LocalSize, [SHADER_LOCAL_SIZE, 1, 1]);
    Ok(b.module().assemble())
}

/// CPU reference for the shader's per-element result
fn reference_value(index: u32) -> u32 {
    let mut acc = index;
    for _ in 0..SHADER_ALU_STEPS {
        acc = acc.wrapping_mul(LCG_MUL).wrapping_add(LCG_ADD);
    }
    acc
}

fn device_type_name(t: vk::PhysicalDeviceType) -> &'static str {
    match t {
        vk::PhysicalDeviceType::DISCRETE_GPU => "DiscreteGpu",
        vk::PhysicalDeviceType::INTEGRATED_GPU => "IntegratedGpu",
        vk::PhysicalDeviceType::VIRTUAL_GPU => "VirtualGpu",
        vk::PhysicalDeviceType::CPU => "Cpu",
        _ => "Other",
    }
}

impl GpuBenchmark {
    pub fn new(config: GpuBenchmarkConfig) -> Result<Self> {
        let entry = unsafe { Entry::load() }
            .map_err(|e| anyhow!("Failed to load Vulkan library: {}", e))?;
        let instance = Self::create_instance(&entry)?;
        let picked = Self::pick_physical_device(&instance).and_then(|(pd, qf)| {
            Self::create_device(&instance, pd, qf).map(|(d, q)| (pd, qf, d, q))
        });
        let (physical_device, queue_family_index, device, queue) = match picked {
            Ok(v) => v,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(e);
            }
        };

        Ok(Self {
            config,
            _entry: entry,
            instance,
            device,
            physical_device,
            queue,
            queue_family_index,
            timer: HighResTimer::new(),
            cancel: cancel::new_flag(),
            progress: None,
            partial: None,
            sensors: None,
        })
    }

    /// Publish live progress and finished results to the GUI
    pub fn with_progress(mut self, progress: SharedProgress, partial: SharedResults<GpuBenchmarkResult>) -> Self {
        self.progress = Some(progress);
        self.partial = Some(partial);
        self
    }

    /// Attach a sensor sampler so every result carries GPU/VRAM/CPU readings
    pub fn with_sensors(mut self, sampler: std::sync::Arc<Sampler>) -> Self {
        self.sensors = Some(sampler);
        self
    }

    /// Share a flag that stops the run early when set
    pub fn with_cancel(mut self, flag: CancelFlag) -> Self {
        self.cancel = flag;
        self
    }

    fn create_instance(entry: &Entry) -> Result<Instance> {
        let app_name = CString::new("Latency Tester Suite")?;
        let engine_name = CString::new("LatencyTester")?;

        let app_info = vk::ApplicationInfo::builder()
            .application_name(&app_name)
            .application_version(vk::make_api_version(0, 1, 0, 0))
            .engine_name(&engine_name)
            .engine_version(vk::make_api_version(0, 1, 0, 0))
            .api_version(vk::API_VERSION_1_1); // SPIR-V 1.3 + StorageBuffer class

        let create_info = vk::InstanceCreateInfo::builder().application_info(&app_info);
        unsafe { entry.create_instance(&create_info, None) }
            .map_err(|e| anyhow!("Failed to create Vulkan instance: {}", e))
    }

    fn pick_physical_device(instance: &Instance) -> Result<(vk::PhysicalDevice, u32)> {
        let devices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| anyhow!("Failed to enumerate physical devices: {}", e))?;

        // Prefer discrete GPUs, then integrated, then anything with a compute queue
        let rank = |d: vk::PhysicalDevice| {
            match unsafe { instance.get_physical_device_properties(d) }.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 0,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
                _ => 2,
            }
        };
        let mut candidates: Vec<_> = devices
            .iter()
            .filter_map(|&d| Self::find_compute_queue_family(instance, d).map(|q| (rank(d), d, q)))
            .collect();
        candidates.sort_by_key(|c| c.0);
        candidates
            .first()
            .map(|&(_, d, q)| (d, q))
            .ok_or_else(|| anyhow!("No Vulkan device with a compute queue found"))
    }

    fn find_compute_queue_family(instance: &Instance, device: vk::PhysicalDevice) -> Option<u32> {
        let families = unsafe { instance.get_physical_device_queue_family_properties(device) };
        families
            .iter()
            .position(|f| f.queue_flags.contains(vk::QueueFlags::COMPUTE))
            .map(|i| i as u32)
    }

    fn create_device(
        instance: &Instance,
        physical_device: vk::PhysicalDevice,
        queue_family_index: u32,
    ) -> Result<(Device, vk::Queue)> {
        let priorities = [1.0f32];
        let queue_infos = [vk::DeviceQueueCreateInfo::builder()
            .queue_family_index(queue_family_index)
            .queue_priorities(&priorities)
            .build()];
        let create_info = vk::DeviceCreateInfo::builder().queue_create_infos(&queue_infos);

        let device = unsafe { instance.create_device(physical_device, &create_info, None) }
            .map_err(|e| anyhow!("Failed to create logical device: {}", e))?;
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };
        Ok((device, queue))
    }

    /// Convenience for `GpuBenchmark::new(..).and_then(|b| b.run_owned())`
    pub fn run_owned(mut self) -> Result<GpuBenchmarkSummary> {
        self.run()
    }

    pub fn run(&mut self) -> Result<GpuBenchmarkSummary> {
        let system_info = crate::system_info::collect_system_info()?;
        let vulkan_info = self.get_vulkan_info()?;
        let mut results = Vec::new();
        let mut skipped_sizes = Vec::new();

        let total = self.config.workload_sizes.len();
        progress::update(&self.progress, |p| {
            *p = progress::RunProgress { total, started: Some(std::time::Instant::now()), ..Default::default() }
        });
        for &size in &self.config.workload_sizes {
            cancel::check(&self.cancel)?;
            progress::update(&self.progress, |p| {
                p.title = format!("{} elements · {} dispatches", size, self.config.iterations);
                if self.config.min_sample_ms > 0 {
                    p.title.push_str(&format!(" (and more for at least {:.1} s so GPU load shows)", self.config.min_sample_ms as f64 / 1000.0));
                }
                p.detail = "creating pipeline and buffer, then timing dispatches".into();
            });
            let r = match self.run_compute_workload(size) {
                Ok(r) => r,
                Err(e) if e.downcast_ref::<SizeUnsupported>().is_some() => {
                    // A small or software device: leave this size out and carry on with the rest
                    skipped_sizes.push(size);
                    progress::update(&self.progress, |p| {
                        p.done += 1;
                        p.detail = format!("{}; skipped", e);
                    });
                    continue;
                }
                Err(e) => return Err(e),
            };
            progress::update(&self.progress, |p| p.done += 1);
            if let Some(partial) = &self.partial {
                partial.lock().unwrap().push(r.clone());
            }
            results.push(r);
        }

        progress::update(&self.progress, |p| p.finished = Some(std::time::Instant::now()));
        Ok(GpuBenchmarkSummary {
            results,
            config: self.config.clone(),
            system_info,
            timestamp: chrono::Utc::now().to_rfc3339(),
            vulkan_info,
            skipped_sizes,
        })
    }

    fn find_memory_type(&self, type_bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
        let props = unsafe { self.instance.get_physical_device_memory_properties(self.physical_device) };
        (0..props.memory_type_count).find(|&i| {
            type_bits & (1 << i) != 0
                && props.memory_types[i as usize].property_flags.contains(flags)
        })
    }

    /// Create pipeline, buffer and a pre-recorded command buffer for `size` invocations.
    /// Returns the resources and the actual number of elements the dispatch covers.
    fn create_resources(&self, size: u64, host_visible: bool) -> Result<Resources> {
        let d = &self.device;
        let limits = unsafe { self.instance.get_physical_device_properties(self.physical_device) }.limits;

        // Split the dispatch over X and Y so it stays under maxComputeWorkGroupCount
        let groups = size.div_ceil(SHADER_LOCAL_SIZE as u64).max(1);
        let gx = groups.min(limits.max_compute_work_group_count[0] as u64).min(65535);
        let gy = groups.div_ceil(gx);
        if gy > limits.max_compute_work_group_count[1] as u64 {
            return Err(SizeUnsupported { size, reason: "device dispatch limits" }.into());
        }
        let elements = gx * gy * SHADER_LOCAL_SIZE as u64;
        let row_stride = (gx * SHADER_LOCAL_SIZE as u64) as u32;
        let bytes = elements * 4;
        if bytes > limits.max_storage_buffer_range as u64 {
            return Err(SizeUnsupported { size, reason: "max storage buffer range" }.into());
        }

        let mut r = Resources {
            device: d.clone(),
            shader: vk::ShaderModule::null(),
            set_layout: vk::DescriptorSetLayout::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            desc_pool: vk::DescriptorPool::null(),
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            cmd_pool: vk::CommandPool::null(),
            fence: vk::Fence::null(),
            cmd: vk::CommandBuffer::null(),
            elements,
        };
        let err = |what: &'static str| move |e: vk::Result| anyhow!("Failed to {}: {}", what, e);

        unsafe {
            let code = build_shader(row_stride)?;
            r.shader = d
                .create_shader_module(&vk::ShaderModuleCreateInfo::builder().code(&code), None)
                .map_err(err("create shader module"))?;

            let bindings = [vk::DescriptorSetLayoutBinding::builder()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .build()];
            r.set_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::builder().bindings(&bindings),
                    None,
                )
                .map_err(err("create descriptor set layout"))?;
            let set_layouts = [r.set_layout];
            r.pipeline_layout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::builder().set_layouts(&set_layouts),
                    None,
                )
                .map_err(err("create pipeline layout"))?;

            let entry_name = CString::new("main")?;
            let stage = vk::PipelineShaderStageCreateInfo::builder()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(r.shader)
                .name(&entry_name)
                .build();
            let pipeline_info = [vk::ComputePipelineCreateInfo::builder()
                .stage(stage)
                .layout(r.pipeline_layout)
                .build()];
            r.pipeline = d
                .create_compute_pipelines(vk::PipelineCache::null(), &pipeline_info, None)
                .map_err(|(_, e)| anyhow!("Failed to create compute pipeline: {}", e))?[0];

            // Buffer + memory
            r.buffer = d
                .create_buffer(
                    &vk::BufferCreateInfo::builder()
                        .size(bytes)
                        .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .map_err(err("create buffer"))?;
            let reqs = d.get_buffer_memory_requirements(r.buffer);
            let wanted = if host_visible {
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
            } else {
                vk::MemoryPropertyFlags::DEVICE_LOCAL
            };
            let mem_type = self
                .find_memory_type(reqs.memory_type_bits, wanted)
                .or_else(|| self.find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::empty()))
                .ok_or_else(|| anyhow!("No suitable memory type"))?;
            r.memory = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::builder()
                        .allocation_size(reqs.size)
                        .memory_type_index(mem_type),
                    None,
                )
                .map_err(err("allocate buffer memory"))?;
            d.bind_buffer_memory(r.buffer, r.memory, 0).map_err(err("bind buffer memory"))?;

            // Descriptor set
            let pool_sizes = [vk::DescriptorPoolSize::builder()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .build()];
            r.desc_pool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::builder().max_sets(1).pool_sizes(&pool_sizes),
                    None,
                )
                .map_err(err("create descriptor pool"))?;
            let set = d
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::builder()
                        .descriptor_pool(r.desc_pool)
                        .set_layouts(&set_layouts),
                )
                .map_err(err("allocate descriptor set"))?[0];
            let buffer_info = [vk::DescriptorBufferInfo::builder()
                .buffer(r.buffer)
                .offset(0)
                .range(vk::WHOLE_SIZE)
                .build()];
            d.update_descriptor_sets(
                &[vk::WriteDescriptorSet::builder()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&buffer_info)
                    .build()],
                &[],
            );

            // Command buffer, recorded once and resubmitted every iteration
            r.cmd_pool = d
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::builder().queue_family_index(self.queue_family_index),
                    None,
                )
                .map_err(err("create command pool"))?;
            r.cmd = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::builder()
                        .command_pool(r.cmd_pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .map_err(err("allocate command buffer"))?[0];
            d.begin_command_buffer(r.cmd, &vk::CommandBufferBeginInfo::builder())
                .map_err(err("begin command buffer"))?;
            d.cmd_bind_pipeline(r.cmd, vk::PipelineBindPoint::COMPUTE, r.pipeline);
            d.cmd_bind_descriptor_sets(
                r.cmd,
                vk::PipelineBindPoint::COMPUTE,
                r.pipeline_layout,
                0,
                &[set],
                &[],
            );
            d.cmd_dispatch(r.cmd, gx as u32, gy as u32, 1);
            d.end_command_buffer(r.cmd).map_err(err("end command buffer"))?;

            r.fence = d
                .create_fence(&vk::FenceCreateInfo::builder(), None)
                .map_err(err("create fence"))?;
        }
        Ok(r)
    }

    /// Submit the recorded command buffer and block until the GPU finished
    fn submit_and_wait(&self, r: &Resources) -> Result<()> {
        let cmds = [r.cmd];
        let submit = [vk::SubmitInfo::builder().command_buffers(&cmds).build()];
        unsafe {
            self.device
                .queue_submit(self.queue, &submit, r.fence)
                .map_err(|e| anyhow!("Queue submit failed: {}", e))?;
            self.device
                .wait_for_fences(&[r.fence], true, u64::MAX)
                .map_err(|e| anyhow!("Waiting for fence failed: {}", e))?;
            self.device
                .reset_fences(&[r.fence])
                .map_err(|e| anyhow!("Resetting fence failed: {}", e))?;
        }
        Ok(())
    }

    fn run_compute_workload(&self, size: u64) -> Result<GpuBenchmarkResult> {
        let res = self.create_resources(size, false)?;
        for _ in 0..self.config.warmup_iterations {
            cancel::check(&self.cancel)?;
            self.submit_and_wait(&res)?;
        }
        // telemetry covers the timed dispatches only, not buffer setup and warmup
        let sensor_start = self.sensors.as_ref().map(|s| s.now_ms());

        let iterations = self.config.iterations.max(1) as usize;
        let min_run = std::time::Duration::from_millis(self.config.min_sample_ms);
        let loop_start = std::time::Instant::now();
        let mut times = Vec::with_capacity(iterations);
        loop {
            cancel::check(&self.cancel)?;
            let start = self.timer.now_ticks();
            self.submit_and_wait(&res)?;
            let end = self.timer.now_ticks();
            times.push(self.timer.ticks_to_ms_f64(end - start));
            if times.len() >= MAX_TIMED_DISPATCHES || (times.len() >= iterations && loop_start.elapsed() >= min_run) {
                break;
            }
        }

        times.sort_by(|a, b| a.total_cmp(b));
        let n = times.len();
        let avg = times.iter().sum::<f64>() / n as f64;
        let variance = times.iter().map(|&x| (x - avg).powi(2)).sum::<f64>() / n as f64;
        let pct = |p: f64| times[((n as f64 * p) as usize).min(n - 1)];
        let ops = res.elements as f64 * SHADER_ALU_STEPS as f64 * 2.0; // mul + add per step

        Ok(GpuBenchmarkResult {
            workload_size: size,
            avg_latency_ms: avg,
            min_latency_ms: times[0],
            max_latency_ms: times[n - 1],
            std_dev_ms: variance.sqrt(),
            percentile_50_ms: pct(0.50),
            percentile_95_ms: pct(0.95),
            percentile_99_ms: pct(0.99),
            throughput_geops: if avg > 0.0 { ops / (avg / 1000.0) / 1e9 } else { 0.0 },
            telemetry: match (&self.sensors, sensor_start) {
                (Some(s), Some(t0)) => s.record("gpu", format!("GPU · {} elements", size), t0, s.now_ms()),
                _ => Telemetry::default(),
            },
        })
    }

    /// Run the shader once on `size` elements and check every output against the CPU
    pub fn verify_shader(&self, size: u64) -> Result<()> {
        let res = self.create_resources(size, true)?;
        let bytes = res.elements * 4;
        let mapped = unsafe {
            self.device
                .map_memory(res.memory, 0, bytes, vk::MemoryMapFlags::empty())
                .map_err(|e| anyhow!("Failed to map memory: {}", e))?
        } as *mut u32;
        // Poison the buffer so an unwritten element cannot pass by accident
        unsafe { std::slice::from_raw_parts_mut(mapped, res.elements as usize).fill(0xDEAD_BEEF) };

        self.submit_and_wait(&res)?;

        let out = unsafe { std::slice::from_raw_parts(mapped, res.elements as usize) };
        let bad = out
            .iter()
            .enumerate()
            .find(|&(i, &v)| v != reference_value(i as u32));
        unsafe { self.device.unmap_memory(res.memory) };
        match bad {
            None => Ok(()),
            Some((i, &v)) => Err(anyhow!(
                "GPU result mismatch at element {}: got {:#x}, expected {:#x}",
                i,
                v,
                reference_value(i as u32)
            )),
        }
    }

    fn get_vulkan_info(&self) -> Result<VulkanInfo> {
        let props = unsafe { self.instance.get_physical_device_properties(self.physical_device) };
        Ok(VulkanInfo {
            api_version: format!(
                "{}.{}.{}",
                vk::api_version_major(props.api_version),
                vk::api_version_minor(props.api_version),
                vk::api_version_patch(props.api_version)
            ),
            driver_version: format!("{}", props.driver_version),
            device_name: unsafe { std::ffi::CStr::from_ptr(props.device_name.as_ptr()) }
                .to_string_lossy()
                .to_string(),
            device_type: device_type_name(props.device_type).to_string(),
            vendor_id: props.vendor_id,
            device_id: props.device_id,
        })
    }
}

#[allow(dead_code)]
pub fn quick_gpu_test() -> Result<f64> {
    let config = GpuBenchmarkConfig {
        workload_sizes: vec![1 << 20],
        iterations: 20,
        warmup_iterations: 3,
        min_sample_ms: 0,
    };
    let mut benchmark = GpuBenchmark::new(config)?;
    let summary = benchmark.run()?;
    summary
        .results
        .first()
        .map(|r| r.avg_latency_ms)
        .ok_or_else(|| anyhow!("GPU benchmark produced no results"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reference_value_is_lcg() {
        assert_eq!(reference_value(0), {
            let mut a = 0u32;
            for _ in 0..SHADER_ALU_STEPS {
                a = a.wrapping_mul(LCG_MUL).wrapping_add(LCG_ADD);
            }
            a
        });
    }

    #[test]
    fn test_shader_is_valid_spirv_header() {
        let words = build_shader(4096).unwrap();
        assert_eq!(words[0], 0x0723_0203);
        assert!(words.len() > 20);
    }

    /// Needs a Vulkan device (e.g. lavapipe); skipped when none is available
    #[test]
    fn test_gpu_shader_matches_cpu() {
        let bench = match GpuBenchmark::new(GpuBenchmarkConfig::default()) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vulkan device ({e})");
                return;
            }
        };
        // Odd size exercises the X/Y dispatch rounding
        bench.verify_shader(100_000).unwrap();
        bench.verify_shader(1).unwrap();
    }

    #[test]
    fn test_gpu_run_small() {
        let mut bench = match GpuBenchmark::new(GpuBenchmarkConfig {
            workload_sizes: vec![1 << 16],
            iterations: 5,
            warmup_iterations: 1,
            min_sample_ms: 0,
        }) {
            Ok(b) => b,
            Err(_) => return,
        };
        let s = bench.run().unwrap();
        assert_eq!(s.results.len(), 1);
        assert!(s.results[0].avg_latency_ms > 0.0);
    }

    #[test]
    fn test_gpu_keeps_dispatching_for_min_sample_ms() {
        let mut bench = match GpuBenchmark::new(GpuBenchmarkConfig {
            workload_sizes: vec![1 << 16],
            iterations: 5,
            warmup_iterations: 1,
            min_sample_ms: 400,
        }) {
            Ok(b) => b,
            Err(_) => return,
        };
        let t = std::time::Instant::now();
        let s = bench.run().unwrap();
        // 5 tiny dispatches take microseconds; the run must have been stretched to keep the GPU busy
        assert!(t.elapsed() >= std::time::Duration::from_millis(400), "took {:?}", t.elapsed());
        assert!(s.results[0].avg_latency_ms > 0.0);
    }

    #[test]
    fn test_sizes_the_device_cannot_run_are_skipped_not_fatal() {
        let mut bench = match GpuBenchmark::new(GpuBenchmarkConfig {
            workload_sizes: vec![1 << 16, 1u64 << 45, 1 << 17],
            iterations: 3,
            warmup_iterations: 0,
            min_sample_ms: 0,
        }) {
            Ok(b) => b,
            Err(_) => return,
        };
        let s = bench.run().expect("one impossible size must not fail the whole run");
        assert_eq!(s.results.len(), 2, "the two sizes that fit ran");
        assert_eq!(s.skipped_sizes, vec![1u64 << 45]);
    }

    #[test]
    fn old_saved_configs_without_min_sample_ms_still_load() {
        let c: GpuBenchmarkConfig = serde_json::from_str(r#"{"workload_sizes":[1024],"iterations":10,"warmup_iterations":2}"#).unwrap();
        assert_eq!(c.min_sample_ms, 0);
    }
}
