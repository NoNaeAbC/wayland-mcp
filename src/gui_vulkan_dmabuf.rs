#![allow(dead_code)]

use std::ffi::CString;
use std::ffi::c_char;
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::ptr;

pub(crate) struct DmabufPlane {
    pub(crate) fd: RawFd,
    pub(crate) offset: u32,
    pub(crate) stride: u32,
    pub(crate) modifier: u64,
}

pub(crate) struct DmabufImage<'a> {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: u32,
    pub(crate) planes: &'a [DmabufPlane],
}

pub(crate) fn read_dmabuf_rgba(image: DmabufImage<'_>) -> Result<Vec<u8>, String> {
    if image.planes.is_empty() {
        return Err("dmabuf image has no planes".to_string());
    }
    let vk_format = vk_format_for_drm_format(image.format)
        .ok_or_else(|| format!("unsupported DRM format {}", image.format))?;
    dma_buf_sync(image.planes[0].fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ)?;
    let app_name = CString::new("wayland-mcp-capture").map_err(|err| err.to_string())?;
    let app_info = VkApplicationInfo {
        s_type: VK_STRUCTURE_TYPE_APPLICATION_INFO,
        p_next: ptr::null(),
        p_application_name: app_name.as_ptr(),
        application_version: 0,
        p_engine_name: app_name.as_ptr(),
        engine_version: 0,
        api_version: VK_API_VERSION_1_1,
    };
    let instance_info = VkInstanceCreateInfo {
        s_type: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        p_next: ptr::null(),
        flags: 0,
        p_application_info: &app_info,
        enabled_layer_count: 0,
        pp_enabled_layer_names: ptr::null(),
        enabled_extension_count: 0,
        pp_enabled_extension_names: ptr::null(),
    };
    let mut instance = ptr::null_mut();
    check_vk(
        unsafe { vkCreateInstance(&instance_info, ptr::null(), &mut instance) },
        "vkCreateInstance",
    )?;
    let sync_fd = image.planes[0].fd;
    let result = read_dmabuf_rgba_with_instance(instance, image, vk_format);
    unsafe { vkDestroyInstance(instance, ptr::null()) };
    let end_result = dma_buf_sync(sync_fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
    match (result, end_result) {
        (Ok(rgba), Ok(())) => Ok(rgba),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
    }
}

fn dma_buf_sync(fd: RawFd, flags: u64) -> Result<(), String> {
    let mut sync = DmaBufSync { flags };
    let result = unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &mut sync) };
    if result < 0 {
        return Err(format!(
            "DMA_BUF_IOCTL_SYNC flags=0x{flags:x} failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn read_dmabuf_rgba_with_instance(
    instance: VkInstance,
    image: DmabufImage<'_>,
    vk_format: VkFormat,
) -> Result<Vec<u8>, String> {
    let physical_devices = enumerate_physical_devices(instance)?;
    let required_extensions = [
        CString::new("VK_KHR_external_memory_fd").map_err(|err| err.to_string())?,
        CString::new("VK_EXT_external_memory_dma_buf").map_err(|err| err.to_string())?,
        CString::new("VK_EXT_image_drm_format_modifier").map_err(|err| err.to_string())?,
        CString::new("VK_KHR_bind_memory2").map_err(|err| err.to_string())?,
    ];
    let extension_ptrs = required_extensions
        .iter()
        .map(|extension| extension.as_ptr())
        .collect::<Vec<_>>();

    let mut last_error = "no Vulkan physical devices available".to_string();
    for physical_device in physical_devices {
        let Some(queue_family_index) = graphics_queue_family(physical_device) else {
            continue;
        };
        let priority = 1.0f32;
        let queue_info = VkDeviceQueueCreateInfo {
            s_type: VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
            p_next: ptr::null(),
            flags: 0,
            queue_family_index,
            queue_count: 1,
            p_queue_priorities: &priority,
        };
        let device_info = VkDeviceCreateInfo {
            s_type: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
            p_next: ptr::null(),
            flags: 0,
            queue_create_info_count: 1,
            p_queue_create_infos: &queue_info,
            enabled_layer_count: 0,
            pp_enabled_layer_names: ptr::null(),
            enabled_extension_count: extension_ptrs.len() as u32,
            pp_enabled_extension_names: extension_ptrs.as_ptr(),
            p_enabled_features: ptr::null(),
        };
        let mut device = ptr::null_mut();
        let create_result =
            unsafe { vkCreateDevice(physical_device, &device_info, ptr::null(), &mut device) };
        if create_result != VK_SUCCESS {
            last_error = format!("vkCreateDevice failed with VkResult {create_result}");
            continue;
        }
        let capture_result = read_dmabuf_rgba_with_device(
            physical_device,
            device,
            queue_family_index,
            &image,
            vk_format,
        );
        unsafe { vkDestroyDevice(device, ptr::null()) };
        match capture_result {
            Ok(rgba) => return Ok(rgba),
            Err(err) => last_error = err,
        }
    }
    Err(last_error)
}

fn read_dmabuf_rgba_with_device(
    physical_device: VkPhysicalDevice,
    device: VkDevice,
    queue_family_index: u32,
    image: &DmabufImage<'_>,
    vk_format: VkFormat,
) -> Result<Vec<u8>, String> {
    let mut queue = ptr::null_mut();
    unsafe { vkGetDeviceQueue(device, queue_family_index, 0, &mut queue) };
    let imported_fd = duplicate_fd(image.planes[0].fd)?;
    let plane_layouts = image
        .planes
        .iter()
        .map(|plane| VkSubresourceLayout {
            offset: plane.offset as u64,
            size: u64::from(plane.stride) * u64::from(image.height),
            row_pitch: plane.stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        })
        .collect::<Vec<_>>();
    let modifier = image.planes[0].modifier;
    let modifier_info = VkImageDrmFormatModifierExplicitCreateInfoEXT {
        s_type: VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
        p_next: ptr::null(),
        drm_format_modifier: modifier,
        drm_format_modifier_plane_count: plane_layouts.len() as u32,
        p_plane_layouts: plane_layouts.as_ptr(),
    };
    let external_info = VkExternalMemoryImageCreateInfo {
        s_type: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        p_next: (&modifier_info as *const VkImageDrmFormatModifierExplicitCreateInfoEXT).cast(),
        handle_types: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    let image_info = VkImageCreateInfo {
        s_type: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        p_next: (&external_info as *const VkExternalMemoryImageCreateInfo).cast(),
        flags: 0,
        image_type: VK_IMAGE_TYPE_2D,
        format: vk_format,
        extent: VkExtent3D {
            width: image.width,
            height: image.height,
            depth: 1,
        },
        mip_levels: 1,
        array_layers: 1,
        samples: VK_SAMPLE_COUNT_1_BIT,
        tiling: VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
        usage: VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
        sharing_mode: VK_SHARING_MODE_EXCLUSIVE,
        queue_family_index_count: 0,
        p_queue_family_indices: ptr::null(),
        initial_layout: VK_IMAGE_LAYOUT_UNDEFINED,
    };
    let mut vk_image = 0;
    if let Err(err) = check_vk(
        unsafe { vkCreateImage(device, &image_info, ptr::null(), &mut vk_image) },
        "vkCreateImage",
    ) {
        unsafe { libc::close(imported_fd) };
        return Err(err);
    }
    let result = copy_imported_image_to_rgba(
        physical_device,
        device,
        queue,
        queue_family_index,
        vk_image,
        imported_fd,
        image,
        vk_format,
    );
    unsafe { vkDestroyImage(device, vk_image, ptr::null()) };
    result
}

#[allow(clippy::too_many_arguments)]
fn copy_imported_image_to_rgba(
    physical_device: VkPhysicalDevice,
    device: VkDevice,
    queue: VkQueue,
    queue_family_index: u32,
    vk_image: VkImage,
    imported_fd: RawFd,
    image: &DmabufImage<'_>,
    vk_format: VkFormat,
) -> Result<Vec<u8>, String> {
    let mut image_reqs = MaybeUninit::<VkMemoryRequirements>::zeroed();
    unsafe { vkGetImageMemoryRequirements(device, vk_image, image_reqs.as_mut_ptr()) };
    let image_reqs = unsafe { image_reqs.assume_init() };
    let memory_type_index = memory_type_index(physical_device, image_reqs.memory_type_bits, 0)
        .ok_or_else(|| "no Vulkan memory type for imported dmabuf".to_string())?;
    let import_info = VkImportMemoryFdInfoKHR {
        s_type: VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
        p_next: ptr::null(),
        handle_type: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        fd: imported_fd,
    };
    let dedicated_info = VkMemoryDedicatedAllocateInfo {
        s_type: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        p_next: (&import_info as *const VkImportMemoryFdInfoKHR).cast(),
        image: vk_image,
        buffer: 0,
    };
    let image_alloc = VkMemoryAllocateInfo {
        s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        p_next: (&dedicated_info as *const VkMemoryDedicatedAllocateInfo).cast(),
        allocation_size: image_reqs.size,
        memory_type_index,
    };
    let mut image_memory = 0;
    let alloc_result =
        unsafe { vkAllocateMemory(device, &image_alloc, ptr::null(), &mut image_memory) };
    if alloc_result != VK_SUCCESS {
        unsafe { libc::close(imported_fd) };
        return Err(format!(
            "vkAllocateMemory imported dmabuf failed with VkResult {alloc_result}"
        ));
    }
    let bind_info = VkBindImageMemoryInfo {
        s_type: VK_STRUCTURE_TYPE_BIND_IMAGE_MEMORY_INFO,
        p_next: ptr::null(),
        image: vk_image,
        memory: image_memory,
        memory_offset: 0,
    };
    let bind_result = check_vk(
        unsafe { vkBindImageMemory2(device, 1, &bind_info) },
        "vkBindImageMemory2",
    );
    if let Err(err) = bind_result {
        unsafe { vkFreeMemory(device, image_memory, ptr::null()) };
        return Err(err);
    }

    let byte_len = u64::from(image.width)
        * u64::from(image.height)
        * u64::try_from(bytes_per_pixel(vk_format)?)
            .map_err(|_| "Vulkan format byte size overflowed u64".to_string())?;
    let buffer_info = VkBufferCreateInfo {
        s_type: VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        p_next: ptr::null(),
        flags: 0,
        size: byte_len,
        usage: VK_BUFFER_USAGE_TRANSFER_DST_BIT,
        sharing_mode: VK_SHARING_MODE_EXCLUSIVE,
        queue_family_index_count: 0,
        p_queue_family_indices: ptr::null(),
    };
    let mut buffer = 0;
    check_vk(
        unsafe { vkCreateBuffer(device, &buffer_info, ptr::null(), &mut buffer) },
        "vkCreateBuffer",
    )?;
    let result = copy_to_buffer_and_map(
        physical_device,
        device,
        queue,
        queue_family_index,
        vk_image,
        image_memory,
        buffer,
        byte_len,
        image,
        vk_format,
    );
    unsafe {
        vkDestroyBuffer(device, buffer, ptr::null());
        vkFreeMemory(device, image_memory, ptr::null());
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn copy_to_buffer_and_map(
    physical_device: VkPhysicalDevice,
    device: VkDevice,
    queue: VkQueue,
    queue_family_index: u32,
    vk_image: VkImage,
    image_memory: VkDeviceMemory,
    buffer: VkBuffer,
    byte_len: u64,
    image: &DmabufImage<'_>,
    vk_format: VkFormat,
) -> Result<Vec<u8>, String> {
    let _ = image_memory;
    let mut buffer_reqs = MaybeUninit::<VkMemoryRequirements>::zeroed();
    unsafe { vkGetBufferMemoryRequirements(device, buffer, buffer_reqs.as_mut_ptr()) };
    let buffer_reqs = unsafe { buffer_reqs.assume_init() };
    let buffer_memory_type = memory_type_index(
        physical_device,
        buffer_reqs.memory_type_bits,
        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
    )
    .ok_or_else(|| "no host-visible Vulkan memory type for capture buffer".to_string())?;
    let buffer_alloc = VkMemoryAllocateInfo {
        s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        p_next: ptr::null(),
        allocation_size: buffer_reqs.size,
        memory_type_index: buffer_memory_type,
    };
    let mut buffer_memory = 0;
    check_vk(
        unsafe { vkAllocateMemory(device, &buffer_alloc, ptr::null(), &mut buffer_memory) },
        "vkAllocateMemory capture buffer",
    )?;
    let result = (|| {
        check_vk(
            unsafe { vkBindBufferMemory(device, buffer, buffer_memory, 0) },
            "vkBindBufferMemory",
        )?;
        let pool_info = VkCommandPoolCreateInfo {
            s_type: VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
            p_next: ptr::null(),
            flags: VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
            queue_family_index,
        };
        let mut command_pool = 0;
        check_vk(
            unsafe { vkCreateCommandPool(device, &pool_info, ptr::null(), &mut command_pool) },
            "vkCreateCommandPool",
        )?;
        let command_result = record_submit_copy_and_map(
            device,
            queue,
            queue_family_index,
            command_pool,
            vk_image,
            buffer,
            buffer_memory,
            byte_len,
            image,
            vk_format,
        );
        unsafe { vkDestroyCommandPool(device, command_pool, ptr::null()) };
        command_result
    })();
    unsafe { vkFreeMemory(device, buffer_memory, ptr::null()) };
    result
}

#[allow(clippy::too_many_arguments)]
fn record_submit_copy_and_map(
    device: VkDevice,
    queue: VkQueue,
    queue_family_index: u32,
    command_pool: VkCommandPool,
    vk_image: VkImage,
    buffer: VkBuffer,
    buffer_memory: VkDeviceMemory,
    byte_len: u64,
    image: &DmabufImage<'_>,
    vk_format: VkFormat,
) -> Result<Vec<u8>, String> {
    let alloc = VkCommandBufferAllocateInfo {
        s_type: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        p_next: ptr::null(),
        command_pool,
        level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        command_buffer_count: 1,
    };
    let mut command_buffer = ptr::null_mut();
    check_vk(
        unsafe { vkAllocateCommandBuffers(device, &alloc, &mut command_buffer) },
        "vkAllocateCommandBuffers",
    )?;
    let begin = VkCommandBufferBeginInfo {
        s_type: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        p_next: ptr::null(),
        flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
        p_inheritance_info: ptr::null(),
    };
    check_vk(
        unsafe { vkBeginCommandBuffer(command_buffer, &begin) },
        "vkBeginCommandBuffer",
    )?;
    let barrier = VkImageMemoryBarrier {
        s_type: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        p_next: ptr::null(),
        src_access_mask: VK_ACCESS_MEMORY_WRITE_BIT,
        dst_access_mask: VK_ACCESS_TRANSFER_READ_BIT,
        old_layout: VK_IMAGE_LAYOUT_GENERAL,
        new_layout: VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
        src_queue_family_index: VK_QUEUE_FAMILY_FOREIGN_EXT,
        dst_queue_family_index: queue_family_index,
        image: vk_image,
        subresource_range: VkImageSubresourceRange {
            aspect_mask: VK_IMAGE_ASPECT_COLOR_BIT,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        },
    };
    unsafe {
        vkCmdPipelineBarrier(
            command_buffer,
            VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
            VK_PIPELINE_STAGE_TRANSFER_BIT,
            0,
            0,
            ptr::null(),
            0,
            ptr::null(),
            1,
            &barrier,
        )
    };
    let region = VkBufferImageCopy {
        buffer_offset: 0,
        buffer_row_length: 0,
        buffer_image_height: 0,
        image_subresource: VkImageSubresourceLayers {
            aspect_mask: VK_IMAGE_ASPECT_COLOR_BIT,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        },
        image_offset: VkOffset3D { x: 0, y: 0, z: 0 },
        image_extent: VkExtent3D {
            width: image.width,
            height: image.height,
            depth: 1,
        },
    };
    unsafe {
        vkCmdCopyImageToBuffer(
            command_buffer,
            vk_image,
            VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
            buffer,
            1,
            &region,
        )
    };
    let release_barrier = VkImageMemoryBarrier {
        s_type: VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        p_next: ptr::null(),
        src_access_mask: VK_ACCESS_TRANSFER_READ_BIT,
        dst_access_mask: VK_ACCESS_MEMORY_READ_BIT | VK_ACCESS_MEMORY_WRITE_BIT,
        old_layout: VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
        new_layout: VK_IMAGE_LAYOUT_GENERAL,
        src_queue_family_index: queue_family_index,
        dst_queue_family_index: VK_QUEUE_FAMILY_FOREIGN_EXT,
        image: vk_image,
        subresource_range: VkImageSubresourceRange {
            aspect_mask: VK_IMAGE_ASPECT_COLOR_BIT,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        },
    };
    unsafe {
        vkCmdPipelineBarrier(
            command_buffer,
            VK_PIPELINE_STAGE_TRANSFER_BIT,
            VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
            0,
            0,
            ptr::null(),
            0,
            ptr::null(),
            1,
            &release_barrier,
        )
    };
    check_vk(
        unsafe { vkEndCommandBuffer(command_buffer) },
        "vkEndCommandBuffer",
    )?;
    let submit = VkSubmitInfo {
        s_type: VK_STRUCTURE_TYPE_SUBMIT_INFO,
        p_next: ptr::null(),
        wait_semaphore_count: 0,
        p_wait_semaphores: ptr::null(),
        p_wait_dst_stage_mask: ptr::null(),
        command_buffer_count: 1,
        p_command_buffers: &command_buffer,
        signal_semaphore_count: 0,
        p_signal_semaphores: ptr::null(),
    };
    check_vk(
        unsafe { vkQueueSubmit(queue, 1, &submit, 0) },
        "vkQueueSubmit",
    )?;
    check_vk(unsafe { vkQueueWaitIdle(queue) }, "vkQueueWaitIdle")?;
    let mut mapped = ptr::null_mut();
    check_vk(
        unsafe { vkMapMemory(device, buffer_memory, 0, byte_len, 0, &mut mapped) },
        "vkMapMemory",
    )?;
    let raw = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), byte_len as usize) };
    let rgba = copied_pixels_to_rgba(raw, image.width as usize, image.height as usize, vk_format);
    unsafe { vkUnmapMemory(device, buffer_memory) };
    Ok(rgba)
}

fn copied_pixels_to_rgba(raw: &[u8], width: usize, height: usize, vk_format: VkFormat) -> Vec<u8> {
    let mut rgba = vec![0u8; width * height * 4];
    let Ok(bytes_per_pixel) = bytes_per_pixel(vk_format) else {
        return rgba;
    };
    for index in 0..(width * height) {
        let src = index * bytes_per_pixel;
        let dst = index * 4;
        match vk_format {
            VK_FORMAT_B8G8R8A8_UNORM => {
                rgba[dst] = raw[src + 2];
                rgba[dst + 1] = raw[src + 1];
                rgba[dst + 2] = raw[src];
                rgba[dst + 3] = raw[src + 3];
            }
            VK_FORMAT_R8G8B8A8_UNORM => {
                rgba[dst..dst + 4].copy_from_slice(&raw[src..src + 4]);
            }
            VK_FORMAT_A2R10G10B10_UNORM_PACK32 => {
                let pixel =
                    u32::from_le_bytes([raw[src], raw[src + 1], raw[src + 2], raw[src + 3]]);
                rgba[dst] = unorm10_to_u8((pixel >> 20) & 0x3ff);
                rgba[dst + 1] = unorm10_to_u8((pixel >> 10) & 0x3ff);
                rgba[dst + 2] = unorm10_to_u8(pixel & 0x3ff);
                rgba[dst + 3] = 255;
            }
            VK_FORMAT_A2B10G10R10_UNORM_PACK32 => {
                let pixel =
                    u32::from_le_bytes([raw[src], raw[src + 1], raw[src + 2], raw[src + 3]]);
                rgba[dst] = unorm10_to_u8(pixel & 0x3ff);
                rgba[dst + 1] = unorm10_to_u8((pixel >> 10) & 0x3ff);
                rgba[dst + 2] = unorm10_to_u8((pixel >> 20) & 0x3ff);
                rgba[dst + 3] = 255;
            }
            VK_FORMAT_R16G16B16A16_SFLOAT => {
                rgba[dst] = f16_channel_to_u8(u16::from_le_bytes([raw[src], raw[src + 1]]));
                rgba[dst + 1] = f16_channel_to_u8(u16::from_le_bytes([raw[src + 2], raw[src + 3]]));
                rgba[dst + 2] = f16_channel_to_u8(u16::from_le_bytes([raw[src + 4], raw[src + 5]]));
                rgba[dst + 3] = 255;
            }
            _ => {}
        }
    }
    rgba
}

fn bytes_per_pixel(vk_format: VkFormat) -> Result<usize, String> {
    match vk_format {
        VK_FORMAT_B8G8R8A8_UNORM
        | VK_FORMAT_R8G8B8A8_UNORM
        | VK_FORMAT_A2R10G10B10_UNORM_PACK32
        | VK_FORMAT_A2B10G10R10_UNORM_PACK32 => Ok(4),
        VK_FORMAT_R16G16B16A16_SFLOAT => Ok(8),
        _ => Err(format!("unsupported Vulkan format {vk_format}")),
    }
}

fn unorm10_to_u8(value: u32) -> u8 {
    ((value * 255 + 511) / 1023) as u8
}

fn f16_channel_to_u8(bits: u16) -> u8 {
    let sign = (bits >> 15) & 0x1;
    if sign != 0 {
        return 0;
    }
    let exponent = ((bits >> 10) & 0x1f) as i32;
    let mantissa = (bits & 0x03ff) as u32;
    let value = match exponent {
        0 => (mantissa as f32) * 2f32.powi(-24),
        0x1f => {
            if mantissa == 0 {
                f32::INFINITY
            } else {
                0.0
            }
        }
        _ => (1.0 + (mantissa as f32 / 1024.0)) * 2f32.powi(exponent - 15),
    };
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn enumerate_physical_devices(instance: VkInstance) -> Result<Vec<VkPhysicalDevice>, String> {
    let mut count = 0;
    check_vk(
        unsafe { vkEnumeratePhysicalDevices(instance, &mut count, ptr::null_mut()) },
        "vkEnumeratePhysicalDevices count",
    )?;
    let mut devices = vec![ptr::null_mut(); count as usize];
    check_vk(
        unsafe { vkEnumeratePhysicalDevices(instance, &mut count, devices.as_mut_ptr()) },
        "vkEnumeratePhysicalDevices",
    )?;
    Ok(devices)
}

fn graphics_queue_family(physical_device: VkPhysicalDevice) -> Option<u32> {
    let mut count = 0;
    unsafe {
        vkGetPhysicalDeviceQueueFamilyProperties(physical_device, &mut count, ptr::null_mut())
    };
    let mut families = vec![VkQueueFamilyProperties::default(); count as usize];
    unsafe {
        vkGetPhysicalDeviceQueueFamilyProperties(physical_device, &mut count, families.as_mut_ptr())
    };
    families
        .iter()
        .position(|family| {
            family.queue_count > 0 && family.queue_flags & VK_QUEUE_GRAPHICS_BIT != 0
        })
        .map(|index| index as u32)
}

fn memory_type_index(
    physical_device: VkPhysicalDevice,
    type_bits: u32,
    required_flags: VkMemoryPropertyFlags,
) -> Option<u32> {
    let mut properties = MaybeUninit::<VkPhysicalDeviceMemoryProperties>::zeroed();
    unsafe { vkGetPhysicalDeviceMemoryProperties(physical_device, properties.as_mut_ptr()) };
    let properties = unsafe { properties.assume_init() };
    for index in 0..properties.memory_type_count {
        let bit = 1u32.checked_shl(index).unwrap_or(0);
        let memory_type = properties.memory_types[index as usize];
        if type_bits & bit != 0 && memory_type.property_flags & required_flags == required_flags {
            return Some(index);
        }
    }
    None
}

fn duplicate_fd(fd: RawFd) -> Result<RawFd, String> {
    let duplicated = unsafe { libc::dup(fd) };
    if duplicated < 0 {
        Err(format!(
            "failed to duplicate dmabuf fd for Vulkan import: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(duplicated)
    }
}

pub(crate) fn supports_drm_format(format: u32) -> bool {
    vk_format_for_drm_format(format).is_some()
}

fn vk_format_for_drm_format(format: u32) -> Option<VkFormat> {
    match format {
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 => Some(VK_FORMAT_B8G8R8A8_UNORM),
        DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888 => Some(VK_FORMAT_R8G8B8A8_UNORM),
        DRM_FORMAT_XRGB2101010 | DRM_FORMAT_ARGB2101010 => Some(VK_FORMAT_A2R10G10B10_UNORM_PACK32),
        DRM_FORMAT_XBGR2101010 | DRM_FORMAT_ABGR2101010 => Some(VK_FORMAT_A2B10G10R10_UNORM_PACK32),
        DRM_FORMAT_XBGR16161616F => Some(VK_FORMAT_R16G16B16A16_SFLOAT),
        _ => None,
    }
}

fn check_vk(result: VkResult, operation: &str) -> Result<(), String> {
    if result == VK_SUCCESS {
        Ok(())
    } else {
        Err(format!("{operation} failed with VkResult {result}"))
    }
}

type VkInstance = *mut c_void;
type VkPhysicalDevice = *mut c_void;
type VkDevice = *mut c_void;
type VkQueue = *mut c_void;
type VkCommandBuffer = *mut c_void;
type VkResult = i32;
type VkDeviceSize = u64;
type VkFormat = u32;
type VkImage = u64;
type VkBuffer = u64;
type VkDeviceMemory = u64;
type VkCommandPool = u64;
type VkFence = u64;
type VkStructureType = u32;
type VkFlags = u32;
type VkMemoryPropertyFlags = VkFlags;

const VK_SUCCESS: VkResult = 0;
const VK_API_VERSION_1_1: u32 = (1 << 22) | (1 << 12);
const VK_STRUCTURE_TYPE_APPLICATION_INFO: VkStructureType = 0;
const VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO: VkStructureType = 1;
const VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO: VkStructureType = 2;
const VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO: VkStructureType = 3;
const VK_STRUCTURE_TYPE_SUBMIT_INFO: VkStructureType = 4;
const VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO: VkStructureType = 5;
const VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO: VkStructureType = 12;
const VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO: VkStructureType = 14;
const VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO: VkStructureType = 39;
const VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO: VkStructureType = 40;
const VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO: VkStructureType = 42;
const VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER: VkStructureType = 45;
const VK_STRUCTURE_TYPE_BIND_IMAGE_MEMORY_INFO: VkStructureType = 1_000_157_001;
const VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO: VkStructureType = 1_000_127_001;
const VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO: VkStructureType = 1_000_072_001;
const VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR: VkStructureType = 1_000_074_000;
const VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT: VkStructureType =
    1_000_158_004;
const VK_FORMAT_R8G8B8A8_UNORM: VkFormat = 37;
const VK_FORMAT_B8G8R8A8_UNORM: VkFormat = 44;
const VK_FORMAT_A2R10G10B10_UNORM_PACK32: VkFormat = 58;
const VK_FORMAT_A2B10G10R10_UNORM_PACK32: VkFormat = 64;
const VK_FORMAT_R16G16B16A16_SFLOAT: VkFormat = 97;
const VK_IMAGE_TYPE_2D: u32 = 1;
const VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT: u32 = 1_000_158_000;
const VK_IMAGE_LAYOUT_UNDEFINED: u32 = 0;
const VK_IMAGE_LAYOUT_GENERAL: u32 = 1;
const VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL: u32 = 6;
const VK_SHARING_MODE_EXCLUSIVE: u32 = 0;
const VK_SAMPLE_COUNT_1_BIT: u32 = 1;
const VK_IMAGE_USAGE_TRANSFER_SRC_BIT: VkFlags = 1;
const VK_BUFFER_USAGE_TRANSFER_DST_BIT: VkFlags = 2;
const VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT: VkMemoryPropertyFlags = 2;
const VK_MEMORY_PROPERTY_HOST_COHERENT_BIT: VkMemoryPropertyFlags = 4;
const VK_QUEUE_GRAPHICS_BIT: VkFlags = 1;
const VK_IMAGE_ASPECT_COLOR_BIT: VkFlags = 1;
const VK_ACCESS_TRANSFER_READ_BIT: VkFlags = 0x0000_0800;
const VK_ACCESS_MEMORY_READ_BIT: VkFlags = 0x0000_8000;
const VK_ACCESS_MEMORY_WRITE_BIT: VkFlags = 0x0001_0000;
const VK_PIPELINE_STAGE_TRANSFER_BIT: VkFlags = 0x0000_1000;
const VK_PIPELINE_STAGE_ALL_COMMANDS_BIT: VkFlags = 0x0001_0000;
const VK_QUEUE_FAMILY_FOREIGN_EXT: u32 = u32::MAX - 2;
const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_START: u64 = 0;
const DMA_BUF_SYNC_END: u64 = 4;
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT: VkFlags = 2;
const VK_COMMAND_BUFFER_LEVEL_PRIMARY: u32 = 0;
const VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT: VkFlags = 1;
const VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT: VkFlags = 0x200;
const DRM_FORMAT_XRGB8888: u32 = fourcc_code(b'X', b'R', b'2', b'4');
const DRM_FORMAT_XBGR8888: u32 = fourcc_code(b'X', b'B', b'2', b'4');
const DRM_FORMAT_ARGB8888: u32 = fourcc_code(b'A', b'R', b'2', b'4');
const DRM_FORMAT_ABGR8888: u32 = fourcc_code(b'A', b'B', b'2', b'4');
const DRM_FORMAT_XRGB2101010: u32 = fourcc_code(b'X', b'R', b'3', b'0');
const DRM_FORMAT_XBGR2101010: u32 = fourcc_code(b'X', b'B', b'3', b'0');
const DRM_FORMAT_ARGB2101010: u32 = fourcc_code(b'A', b'R', b'3', b'0');
const DRM_FORMAT_ABGR2101010: u32 = fourcc_code(b'A', b'B', b'3', b'0');
const DRM_FORMAT_XBGR16161616F: u32 = fourcc_code(b'X', b'B', b'4', b'H');

const fn fourcc_code(a: u8, b: u8, c: u8, d: u8) -> u32 {
    u32::from_le_bytes([a, b, c, d])
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn maps_xbgr16161616f_to_vulkan_float_rgba() {
        assert_eq!(
            vk_format_for_drm_format(DRM_FORMAT_XBGR16161616F),
            Some(VK_FORMAT_R16G16B16A16_SFLOAT)
        );
    }

    #[test]
    fn converts_xbgr16161616f_pixels_to_rgba8() {
        let raw = [
            0x00, 0x3c, // R = 1.0
            0x00, 0x38, // G = 0.5
            0x00, 0x00, // B = 0.0
            0x00, 0x3c, // X = ignored
        ];

        assert_eq!(
            copied_pixels_to_rgba(&raw, 1, 1, VK_FORMAT_R16G16B16A16_SFLOAT),
            vec![255, 128, 0, 255]
        );
    }

    #[test]
    fn maps_common_10_bit_drm_formats_to_vulkan() {
        assert_eq!(
            [
                vk_format_for_drm_format(DRM_FORMAT_XRGB2101010),
                vk_format_for_drm_format(DRM_FORMAT_ARGB2101010),
                vk_format_for_drm_format(DRM_FORMAT_XBGR2101010),
                vk_format_for_drm_format(DRM_FORMAT_ABGR2101010),
            ],
            [
                Some(VK_FORMAT_A2R10G10B10_UNORM_PACK32),
                Some(VK_FORMAT_A2R10G10B10_UNORM_PACK32),
                Some(VK_FORMAT_A2B10G10R10_UNORM_PACK32),
                Some(VK_FORMAT_A2B10G10R10_UNORM_PACK32),
            ]
        );
    }

    #[test]
    fn converts_10_bit_packed_pixels_to_rgba8() {
        let xrgb = ((1023u32 << 20) | (512 << 10)).to_le_bytes();
        let xbgr = ((1023u32 << 20) | (512 << 10)).to_le_bytes();

        assert_eq!(
            copied_pixels_to_rgba(&xrgb, 1, 1, VK_FORMAT_A2R10G10B10_UNORM_PACK32),
            vec![255, 128, 0, 255]
        );
        assert_eq!(
            copied_pixels_to_rgba(&xbgr, 1, 1, VK_FORMAT_A2B10G10R10_UNORM_PACK32),
            vec![0, 128, 255, 255]
        );
    }
}

#[repr(C)]
struct VkApplicationInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    p_application_name: *const c_char,
    application_version: u32,
    p_engine_name: *const c_char,
    engine_version: u32,
    api_version: u32,
}

#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

#[repr(C)]
struct VkInstanceCreateInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    flags: VkFlags,
    p_application_info: *const VkApplicationInfo,
    enabled_layer_count: u32,
    pp_enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    pp_enabled_extension_names: *const *const c_char,
}

#[repr(C)]
struct VkDeviceQueueCreateInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    flags: VkFlags,
    queue_family_index: u32,
    queue_count: u32,
    p_queue_priorities: *const f32,
}

#[repr(C)]
struct VkDeviceCreateInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    flags: VkFlags,
    queue_create_info_count: u32,
    p_queue_create_infos: *const VkDeviceQueueCreateInfo,
    enabled_layer_count: u32,
    pp_enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    pp_enabled_extension_names: *const *const c_char,
    p_enabled_features: *const c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VkExtent3D {
    width: u32,
    height: u32,
    depth: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VkOffset3D {
    x: i32,
    y: i32,
    z: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VkQueueFamilyProperties {
    queue_flags: VkFlags,
    queue_count: u32,
    timestamp_valid_bits: u32,
    min_image_transfer_granularity: VkExtent3D,
}

#[repr(C)]
struct VkExternalMemoryImageCreateInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    handle_types: VkFlags,
}

#[repr(C)]
struct VkSubresourceLayout {
    offset: VkDeviceSize,
    size: VkDeviceSize,
    row_pitch: VkDeviceSize,
    array_pitch: VkDeviceSize,
    depth_pitch: VkDeviceSize,
}

#[repr(C)]
struct VkImageDrmFormatModifierExplicitCreateInfoEXT {
    s_type: VkStructureType,
    p_next: *const c_void,
    drm_format_modifier: u64,
    drm_format_modifier_plane_count: u32,
    p_plane_layouts: *const VkSubresourceLayout,
}

#[repr(C)]
struct VkImageCreateInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    flags: VkFlags,
    image_type: u32,
    format: VkFormat,
    extent: VkExtent3D,
    mip_levels: u32,
    array_layers: u32,
    samples: u32,
    tiling: u32,
    usage: VkFlags,
    sharing_mode: u32,
    queue_family_index_count: u32,
    p_queue_family_indices: *const u32,
    initial_layout: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VkMemoryType {
    property_flags: VkMemoryPropertyFlags,
    heap_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VkMemoryHeap {
    size: VkDeviceSize,
    flags: VkFlags,
}

#[repr(C)]
struct VkPhysicalDeviceMemoryProperties {
    memory_type_count: u32,
    memory_types: [VkMemoryType; 32],
    memory_heap_count: u32,
    memory_heaps: [VkMemoryHeap; 16],
}

#[repr(C)]
struct VkMemoryRequirements {
    size: VkDeviceSize,
    alignment: VkDeviceSize,
    memory_type_bits: u32,
}

#[repr(C)]
struct VkImportMemoryFdInfoKHR {
    s_type: VkStructureType,
    p_next: *const c_void,
    handle_type: VkFlags,
    fd: i32,
}

#[repr(C)]
struct VkMemoryAllocateInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    allocation_size: VkDeviceSize,
    memory_type_index: u32,
}

#[repr(C)]
struct VkMemoryDedicatedAllocateInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    image: VkImage,
    buffer: VkBuffer,
}

#[repr(C)]
struct VkBindImageMemoryInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    image: VkImage,
    memory: VkDeviceMemory,
    memory_offset: VkDeviceSize,
}

#[repr(C)]
struct VkBufferCreateInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    flags: VkFlags,
    size: VkDeviceSize,
    usage: VkFlags,
    sharing_mode: u32,
    queue_family_index_count: u32,
    p_queue_family_indices: *const u32,
}

#[repr(C)]
struct VkCommandPoolCreateInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    flags: VkFlags,
    queue_family_index: u32,
}

#[repr(C)]
struct VkCommandBufferAllocateInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    command_pool: VkCommandPool,
    level: u32,
    command_buffer_count: u32,
}

#[repr(C)]
struct VkCommandBufferBeginInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    flags: VkFlags,
    p_inheritance_info: *const c_void,
}

#[repr(C)]
struct VkImageSubresourceLayers {
    aspect_mask: VkFlags,
    mip_level: u32,
    base_array_layer: u32,
    layer_count: u32,
}

#[repr(C)]
struct VkBufferImageCopy {
    buffer_offset: VkDeviceSize,
    buffer_row_length: u32,
    buffer_image_height: u32,
    image_subresource: VkImageSubresourceLayers,
    image_offset: VkOffset3D,
    image_extent: VkExtent3D,
}

#[repr(C)]
struct VkImageSubresourceRange {
    aspect_mask: VkFlags,
    base_mip_level: u32,
    level_count: u32,
    base_array_layer: u32,
    layer_count: u32,
}

#[repr(C)]
struct VkImageMemoryBarrier {
    s_type: VkStructureType,
    p_next: *const c_void,
    src_access_mask: VkFlags,
    dst_access_mask: VkFlags,
    old_layout: u32,
    new_layout: u32,
    src_queue_family_index: u32,
    dst_queue_family_index: u32,
    image: VkImage,
    subresource_range: VkImageSubresourceRange,
}

#[repr(C)]
struct VkSubmitInfo {
    s_type: VkStructureType,
    p_next: *const c_void,
    wait_semaphore_count: u32,
    p_wait_semaphores: *const u64,
    p_wait_dst_stage_mask: *const VkFlags,
    command_buffer_count: u32,
    p_command_buffers: *const VkCommandBuffer,
    signal_semaphore_count: u32,
    p_signal_semaphores: *const u64,
}

#[link(name = "vulkan")]
unsafe extern "C" {
    fn vkCreateInstance(
        p_create_info: *const VkInstanceCreateInfo,
        p_allocator: *const c_void,
        p_instance: *mut VkInstance,
    ) -> VkResult;
    fn vkDestroyInstance(instance: VkInstance, p_allocator: *const c_void);
    fn vkEnumeratePhysicalDevices(
        instance: VkInstance,
        p_physical_device_count: *mut u32,
        p_physical_devices: *mut VkPhysicalDevice,
    ) -> VkResult;
    fn vkGetPhysicalDeviceQueueFamilyProperties(
        physical_device: VkPhysicalDevice,
        p_queue_family_property_count: *mut u32,
        p_queue_family_properties: *mut VkQueueFamilyProperties,
    );
    fn vkGetPhysicalDeviceMemoryProperties(
        physical_device: VkPhysicalDevice,
        p_memory_properties: *mut VkPhysicalDeviceMemoryProperties,
    );
    fn vkCreateDevice(
        physical_device: VkPhysicalDevice,
        p_create_info: *const VkDeviceCreateInfo,
        p_allocator: *const c_void,
        p_device: *mut VkDevice,
    ) -> VkResult;
    fn vkDestroyDevice(device: VkDevice, p_allocator: *const c_void);
    fn vkGetDeviceQueue(
        device: VkDevice,
        queue_family_index: u32,
        queue_index: u32,
        p_queue: *mut VkQueue,
    );
    fn vkCreateImage(
        device: VkDevice,
        p_create_info: *const VkImageCreateInfo,
        p_allocator: *const c_void,
        p_image: *mut VkImage,
    ) -> VkResult;
    fn vkDestroyImage(device: VkDevice, image: VkImage, p_allocator: *const c_void);
    fn vkGetImageMemoryRequirements(
        device: VkDevice,
        image: VkImage,
        p_memory_requirements: *mut VkMemoryRequirements,
    );
    fn vkAllocateMemory(
        device: VkDevice,
        p_allocate_info: *const VkMemoryAllocateInfo,
        p_allocator: *const c_void,
        p_memory: *mut VkDeviceMemory,
    ) -> VkResult;
    fn vkFreeMemory(device: VkDevice, memory: VkDeviceMemory, p_allocator: *const c_void);
    fn vkBindImageMemory2(
        device: VkDevice,
        bind_info_count: u32,
        p_bind_infos: *const VkBindImageMemoryInfo,
    ) -> VkResult;
    fn vkCreateBuffer(
        device: VkDevice,
        p_create_info: *const VkBufferCreateInfo,
        p_allocator: *const c_void,
        p_buffer: *mut VkBuffer,
    ) -> VkResult;
    fn vkDestroyBuffer(device: VkDevice, buffer: VkBuffer, p_allocator: *const c_void);
    fn vkGetBufferMemoryRequirements(
        device: VkDevice,
        buffer: VkBuffer,
        p_memory_requirements: *mut VkMemoryRequirements,
    );
    fn vkBindBufferMemory(
        device: VkDevice,
        buffer: VkBuffer,
        memory: VkDeviceMemory,
        memory_offset: VkDeviceSize,
    ) -> VkResult;
    fn vkCreateCommandPool(
        device: VkDevice,
        p_create_info: *const VkCommandPoolCreateInfo,
        p_allocator: *const c_void,
        p_command_pool: *mut VkCommandPool,
    ) -> VkResult;
    fn vkDestroyCommandPool(
        device: VkDevice,
        command_pool: VkCommandPool,
        p_allocator: *const c_void,
    );
    fn vkAllocateCommandBuffers(
        device: VkDevice,
        p_allocate_info: *const VkCommandBufferAllocateInfo,
        p_command_buffers: *mut VkCommandBuffer,
    ) -> VkResult;
    fn vkBeginCommandBuffer(
        command_buffer: VkCommandBuffer,
        p_begin_info: *const VkCommandBufferBeginInfo,
    ) -> VkResult;
    fn vkEndCommandBuffer(command_buffer: VkCommandBuffer) -> VkResult;
    fn vkCmdCopyImageToBuffer(
        command_buffer: VkCommandBuffer,
        src_image: VkImage,
        src_image_layout: u32,
        dst_buffer: VkBuffer,
        region_count: u32,
        p_regions: *const VkBufferImageCopy,
    );
    fn vkCmdPipelineBarrier(
        command_buffer: VkCommandBuffer,
        src_stage_mask: VkFlags,
        dst_stage_mask: VkFlags,
        dependency_flags: VkFlags,
        memory_barrier_count: u32,
        p_memory_barriers: *const c_void,
        buffer_memory_barrier_count: u32,
        p_buffer_memory_barriers: *const c_void,
        image_memory_barrier_count: u32,
        p_image_memory_barriers: *const VkImageMemoryBarrier,
    );
    fn vkQueueSubmit(
        queue: VkQueue,
        submit_count: u32,
        p_submits: *const VkSubmitInfo,
        fence: VkFence,
    ) -> VkResult;
    fn vkQueueWaitIdle(queue: VkQueue) -> VkResult;
    fn vkMapMemory(
        device: VkDevice,
        memory: VkDeviceMemory,
        offset: VkDeviceSize,
        size: VkDeviceSize,
        flags: VkFlags,
        pp_data: *mut *mut c_void,
    ) -> VkResult;
    fn vkUnmapMemory(device: VkDevice, memory: VkDeviceMemory);
}
