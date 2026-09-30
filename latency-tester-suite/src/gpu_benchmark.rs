//! Vulkan-based GPU benchmark focusing on compute latency and throughput
//! This implementation uses ash 0.37 and focuses on headless compute workloads to avoid
//! platform-specific surface/swapchain complexities.

use anyhow::{anyhow, Result};
use ash::{vk, Device, Entry, Instance};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use crate::timer::HighResTimer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuBenchmarkConfig {
    pub workload_sizes: Vec<u64>,
    pub iterations: u32,
    pub warmup_iterations: u32,
}

impl Default for GpuBenchmarkConfig {
    fn default() -> Self {
        Self {
            workload_sizes: vec![1 << 20, 1 << 22, 1 << 24], // 1M, 4M, 16M elements
            iterations: 100,
            warmup_iterations: 10,
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
    pub throughput_geops: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuBenchmarkSummary {
    pub results: Vec<GpuBenchmarkResult>,
    pub config: GpuBenchmarkConfig,
    pub system_info: crate::system_info::SystemInfo,
    pub timestamp: String,
    pub vulkan_info: VulkanInfo,
}

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
    entry: Entry,
    instance: Instance,
    device: Device,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    timer: HighResTimer,
}

impl GpuBenchmark {
    pub fn new(config: GpuBenchmarkConfig) -> Result<Self> {
        let entry = unsafe { Entry::load() }
            .map_err(|e| anyhow!("Failed to load Vulkan entry: {}", e))?;
        let instance = Self::create_instance(&entry)?;
        let (physical_device, queue_family_index) = Self::pick_physical_device(&instance)?;
        let (device, queue) = Self::create_device(&instance, physical_device, queue_family_index)?;

        Ok(Self {
            config,
            entry,
            instance,
            device,
            physical_device,
            queue,
            timer: HighResTimer::new(),
        })
    }

    fn create_instance(entry: &Entry) -> Result<Instance> {
        let app_name = CString::new("Latency Tester Suite").unwrap();
        let engine_name = CString::new("LatencyTester").unwrap();

        let app_info = vk::ApplicationInfo {
            s_type: vk::StructureType::APPLICATION_INFO,
            p_next: std::ptr::null(),
            p_application_name: app_name.as_ptr(),
            application_version: vk::make_api_version(0, 1, 0, 0),
            p_engine_name: engine_name.as_ptr(),
            engine_version: vk::make_api_version(0, 1, 0, 0),
            api_version: vk::API_VERSION_1_3,
        };

        let create_info = vk::InstanceCreateInfo {
            s_type: vk::StructureType::INSTANCE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: vk::InstanceCreateFlags::empty(),
            p_application_info: &app_info,
            enabled_layer_count: 0,
            pp_enabled_layer_names: std::ptr::null(),
            enabled_extension_count: 0,
            pp_enabled_extension_names: std::ptr::null(),
        };

        let instance = unsafe { entry.create_instance(&create_info, None) }
            .map_err(|e| anyhow!("Failed to create Vulkan instance: {}", e))?;
        Ok(instance)
    }

    fn pick_physical_device(instance: &Instance) -> Result<(vk::PhysicalDevice, u32)> {
        let devices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| anyhow!("Failed to enumerate physical devices: {}", e))?;

        for device in &devices {
            let props = unsafe { instance.get_physical_device_properties(*device) };
            if props.device_type == vk::PhysicalDeviceType::DISCRETE_GPU {
                if let Some(queue_family) = Self::find_compute_queue_family(instance, *device) {
                    return Ok((*device, queue_family));
                }
            }
        }

        for device in &devices {
            if let Some(queue_family) = Self::find_compute_queue_family(instance, *device) {
                return Ok((*device, queue_family));
            }
        }

        Err(anyhow!("No suitable GPU found"))
    }

    fn find_compute_queue_family(instance: &Instance, device: vk::PhysicalDevice) -> Option<u32> {
        let queue_families = unsafe { instance.get_physical_device_queue_family_properties(device) };
        for (i, family) in queue_families.iter().enumerate() {
            if family.queue_flags.contains(vk::QueueFlags::COMPUTE) {
                return Some(i as u32);
            }
        }
        None
    }

    fn create_device(
        instance: &Instance,
        physical_device: vk::PhysicalDevice,
        queue_family_index: u32,
    ) -> Result<(Device, vk::Queue)> {
        let queue_priorities = [1.0];
        let queue_create_info = vk::DeviceQueueCreateInfo {
            s_type: vk::StructureType::DEVICE_QUEUE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: vk::DeviceQueueCreateFlags::empty(),
            queue_family_index,
            queue_count: 1,
            p_queue_priorities: queue_priorities.as_ptr(),
        };

        let create_info = vk::DeviceCreateInfo {
            s_type: vk::StructureType::DEVICE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: vk::DeviceCreateFlags::empty(),
            queue_create_info_count: 1,
            p_queue_create_infos: std::slice::from_ref(&queue_create_info).as_ptr(),
            enabled_extension_count: 0,
            pp_enabled_extension_names: std::ptr::null(),
            p_enabled_features: &vk::PhysicalDeviceFeatures::default(),
        };

        let device = unsafe { instance.create_device(physical_device, &create_info, None) }
            .map_err(|e| anyhow!("Failed to create logical device: {}", e))?;
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };

        Ok((device, queue))
    }

    pub fn run(&mut self) -> Result<GpuBenchmarkSummary> {
        let system_info = crate::system_info::collect_system_info()?;
        let vulkan_info = self.get_vulkan_info()?;
        let mut results = Vec::new();

        for &size in &self.config.workload_sizes {
            let result = self.run_compute_workload(size)?;
            results.push(result);
        }

        Ok(GpuBenchmarkSummary {
            results,
            config: self.config.clone(),
            system_info,
            timestamp: chrono::Utc::now().to_rfc3339(),
            vulkan_info,
        })
    }

    fn run_compute_workload(&self, size: u64) -> Result<GpuBenchmarkResult> {
        // Simple compute shader SPIR-V (Void main() { })
        let shader_code: &[u32] = &[
            0x07230203, 0x00010000, 0x00000001, 0x00000000,
            0x00020011, 0x00000000, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x00000000, 0x00000000,
            0x00000000, 0x00000000, 0x00000000, 0x00000000,
        ];

        let shader_module = unsafe {
            let create_info = vk::ShaderModuleCreateInfo {
                s_type: vk::StructureType::SHADER_MODULE_CREATE_INFO,
                p_next: std::ptr::null(),
                flags: vk::ShaderModuleCreateFlags::empty(),
                code_size: (shader_code.len() * 4) as usize,
                p_code: bytemuck::cast_slice(shader_code).as_ptr(),
            };
            self.device.create_shader_module(&create_info, None)
                .map_err(|e| anyhow!("Failed to create shader module: {}", e))?
        };

        let pipeline_layout = unsafe {
            let layout_info = vk::PipelineLayoutCreateInfo::default();
            self.device.create_pipeline_layout(&layout_info, None)
                .map_err(|e| anyhow!("Failed to create pipeline layout: {}", e))?
        };

        let pipeline = unsafe {
            let stage_info = vk::PipelineShaderStageCreateInfo {
                s_type: vk::StructureType::PIPELINE_SHADER_STAGE_CREATE_INFO,
                p_next: std::ptr::null(),
                flags: vk::PipelineShaderStageCreateFlags::empty(),
                stage: vk::ShaderStageFlags::COMPUTE,
                module: shader_module,
                p_name: std::ptr::null(),
                p_specialization_info: std::ptr::null(),
            };
            let pipeline_info = vk::ComputePipelineCreateInfo {
                s_type: vk::StructureType::COMPUTE_PIPELINE_CREATE_INFO,
                p_next: std::ptr::null(),
                flags: vk::PipelineCreateFlags::empty(),
                stage: &stage_info,
                layout: pipeline_layout,
                base_pipeline_handle: vk::Pipeline::null(),
                base_pipeline_index: -1,
            };

            self.device.create_compute_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
                .map_err(|e| anyhow!("Failed to create compute pipeline: {}", e))?[0]
        };

        let command_pool = unsafe {
            let pool_info = vk::CommandPoolCreateInfo {
                s_type: vk::StructureType::COMMAND_POOL_CREATE_INFO,
                p_next: std::ptr::null(),
                flags: vk::CommandPoolCreateFlags::empty(),
                queue_family_index: self.queue_family_index,
            };
            self.device.create_command_pool(&pool_info, None)
                .map_err(|e| anyhow!("Failed to create command pool: {}", e))?
        };

        let command_buffer = unsafe {
            let alloc_info = vk::CommandBufferAllocateInfo {
                s_type: vk::StructureType::COMMAND_BUFFER_ALLOCATE_INFO,
                p_next: std::ptr::null(),
                command_pool,
                level: vk::CommandBufferLevel::PRIMARY,
                command_buffer_count: 1,
            };
            self.device.allocate_command_buffers(&alloc_info)
                .map_err(|e| anyhow!("Failed to allocate command buffer: {}", e))?[0]
        };

        unsafe {
            let begin_info = vk::CommandBufferBeginInfo {
                s_type: vk::StructureType::COMMAND_BUFFER_BEGIN_INFO,
                p_next: std::ptr::null(),
                flags: vk::CommandBufferBeginFlags::empty(), // Note: Check if this exists in ash 0.37, might be vk::CommandBufferBeginFlags::empty() or similar
                p_inheritance_info: std::ptr::null(),
            };
            self.device.begin_command_buffer(command_buffer, &begin_info)
                .map_err(|e| anyhow!("Failed to begin command buffer: {}", e))?;
            self.device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::COMPUTE, pipeline);
            self.device.cmd_dispatch(command_buffer, (size as u32 / 64).max(1), 1, 1);
            self.device.end_command_buffer(command_buffer)
                .map_err(|e| anyhow!("Failed to end command buffer: {}", e))?;
        }

        let submit_info = vk::SubmitInfo {
            s_type: vk::StructureType::SUBMIT_INFO,
            p_next: std::ptr::null(),
            command_buffer_count: 1,
            p_command_buffers: std::slice::from_ref(&command_buffer).as_ptr(),
            signal_semaphore_count: 0,
            p_signal_semaphores: std::ptr::null(),
            wait_semaphore_count: 0,
            p_wait_semaphores: std::ptr::null(),
            p_wait_dst_stage_mask: std::ptr::null(),
        };

        let mut frame_times = Vec::with_capacity(self.config.iterations as usize);
        let timer = HighResTimer::new();

        // Warmup
        for _ in 0..self.config.warmup_iterations {
            unsafe {
                self.device.queue_submit(self.queue, &[submit_info], vk::Fence::null())
                    .map_err(|e| anyhow!("Warmup submit failed: {}", e))?;
                self.device.queue_wait_idle().map_err(|e| anyhow!("Warmup wait failed: {}", e))?;
            }
        }

        // Benchmark
        for _ in 0..self.config.iterations {
            let start = timer.now();
            unsafe {
                self.device.queue_submit(self.queue, &[submit_info], vk::Fence::null())
                    .map_err(|e| anyhow!("Submit failed: {}", e))?;
                self.device.queue_wait_idle().map_err(|e| anyhow!("Wait idle failed: {}", e))?;
            }
            let end = timer.now();
            frame_times.push(end.duration_since(start).as_secs_f64() * 1000.0);
        }

        // Cleanup
        unsafe {
            self.device.destroy_pipeline(pipeline, None);
            self.device.destroy_pipeline_layout(pipeline_layout, None);
            self.device.destroy_shader_module(shader_module, None);
            self.device.destroy_command_pool(command_pool, None);
        }

        frame_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let sum: f64 = frame_times.iter().sum();
        let avg = sum / frame_times.len() as f64;
        let min = frame_times[0];
        let max = frame_times[frame_times.len() - 1];
        let variance: f64 = frame_times.iter().map(|&x| (x - avg).powi(2)).sum::<f64>() / frame_times.len() as f64;
        let std_dev = variance.sqrt();

        Ok(GpuBenchmarkResult {
            workload_size: size,
            avg_latency_ms: avg,
            min_latency_ms: min,
            max_latency_ms: max,
            std_dev_ms: std_dev,
            throughput_geops: (size as f64 / (avg / 1000.0)) / 1e9,
        })
    }

    fn get_vulkan_info(&self) -> Result<VulkanInfo> {
        let props = unsafe { self.instance.get_physical_device_properties(self.physical_device) };
        Ok(VulkanInfo {
            api_version: format!("{}.{}.{}", 
                vk::api_version_major(props.api_version),
                vk::api_version_minor(props.api_version),
                vk::api_version_patch(props.api_version)
            ),
            driver_version: format!("{}", props.driver_version),
            device_name: unsafe { std::ffi::CStr::from_ptr(props.device_name.as_ptr()) }
                .to_string_lossy().to_string(),
            device_type: format!("{:?}", props.device_type),
            vendor_id: props.vendor_id,
            device_id: props.device_id,
        })
    }
}

pub fn quick_gpu_test() -> Result<f64> {
    let config = GpuBenchmarkConfig::default();
    let mut benchmark = GpuBenchmark::new(config)?;
    let summary = benchmark.run()?;
    Ok(summary.results[0].avg_latency_ms)
}
