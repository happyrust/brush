// While most wgsl binding code is autogenerated, some more glue is needed for
// wgsl burn interop. This file contains some of this glue code, it's mainly
// generated by the macro below.
mod shaders;

pub use cubecl::compute::{CompiledKernel, CubeTask};
use cubecl::{client::ComputeClient, server::ComputeServer, CubeCount, CubeDim};

use burn::tensor::Shape;

use burn_jit::{tensor::JitTensor, JitElement, JitRuntime};
use bytemuck::Pod;

pub fn calc_cube_count<const D: usize, S: ComputeServer>(
    sizes: [u32; D],
    workgroup_size: [u32; 3],
) -> CubeCount<S> {
    let execs = [
        sizes.first().unwrap_or(&1).div_ceil(workgroup_size[0]),
        sizes.get(1).unwrap_or(&1).div_ceil(workgroup_size[1]),
        sizes.get(2).unwrap_or(&1).div_ceil(workgroup_size[2]),
    ];
    CubeCount::Static(execs[0], execs[1], execs[2])
}

pub fn module_to_compiled(module: naga::Module, workgroup_size: [u32; 3]) -> CompiledKernel {
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::empty(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .unwrap();

    let shader_string =
        naga::back::wgsl::write_string(&module, &info, naga::back::wgsl::WriterFlags::empty())
            .expect("failed to convert naga module to source");

    CompiledKernel {
        source: shader_string,
        cube_dim: CubeDim::new(workgroup_size[0], workgroup_size[1], workgroup_size[2]),
        // This is just a compiler hint for burn, but doesn't have to be set.
        shared_mem_bytes: 0,
    }
}

#[macro_export]
macro_rules! kernel_source_gen {
    ($struct_name:ident { $($field_name:ident),* }, $module:ident) => {
        #[derive(Debug, Copy, Clone)]
        pub(crate) struct $struct_name {
            $(
                $field_name: bool,
            )*
        }

        impl $struct_name {
            pub fn task($($field_name: bool),*) -> Box<$struct_name> {
                let kernel = Self {
                    $(
                        $field_name,
                    )*
                };

                Box::new(kernel)
            }

            fn create_shader_hashmap(&self) -> std::collections::HashMap<String, naga_oil::compose::ShaderDefValue> {
                let map = std::collections::HashMap::new();
                $(
                    let mut map = map;

                    if self.$field_name {
                        map.insert(stringify!($field_name).to_owned().to_uppercase(), naga_oil::compose::ShaderDefValue::Bool(true));
                    }
                )*
                map
            }

            pub const WORKGROUP_SIZE: [u32; 3] = $module::WORKGROUP_SIZE;

            fn source(&self) -> naga::Module {
                let shader_defs = self.create_shader_hashmap();
                $module::create_shader_source(shader_defs)
            }
        }

        impl brush_kernel::CubeTask for $struct_name {
            fn id(&self) -> String {
                let ids = stringify!($struct_name).to_owned();
                $(
                    let mut ids = ids;
                    ids.push(
                        if self.$field_name {
                            '0'
                        } else {
                            '1'
                        }
                    );
                )*
                ids
            }

            fn compile(&self) -> brush_kernel::CompiledKernel {
                let module = self.source();
                brush_kernel::module_to_compiled(module, Self::WORKGROUP_SIZE)
            }
        }
    };
}

// Convert a tensors type. This only reinterprets the data, and doesn't
// do any actual conversions.
pub fn bitcast_tensor<const D: usize, R: JitRuntime, EIn: JitElement, EOut: JitElement>(
    tensor: JitTensor<R, EIn, D>,
) -> JitTensor<R, EOut, D> {
    JitTensor::new(
        tensor.client,
        tensor.handle,
        tensor.shape,
        tensor.device,
        tensor.strides,
    )
}

// Reserve a buffer from the client for the given shape.
pub fn create_tensor<E: JitElement, const D: usize, R: JitRuntime>(
    shape: [usize; D],
    device: &R::Device,
    client: &ComputeClient<R::Server, R::Channel>,
) -> JitTensor<R, E, D> {
    let shape = Shape::new(shape);
    let bufsize = shape.num_elements() * core::mem::size_of::<E>();
    let buffer = client.empty(bufsize);

    #[cfg(test)]
    {
        use burn::tensor::ops::FloatTensorOps;
        use burn_jit::JitBackend;
        // for tests - make doubly sure we're not accidentally relying on values
        // being initialized to zero by adding in some random noise.
        let f =
            JitTensor::<R, f32, D>::new_contiguous(client.clone(), device.clone(), shape, buffer);
        bitcast_tensor(JitBackend::<R, f32, i32>::float_add_scalar(f, -12345.0))
    }

    #[cfg(not(test))]
    JitTensor::new_contiguous(client.clone(), device.clone(), shape, buffer)
}

pub fn create_uniform_buffer<R: JitRuntime, T: Pod>(
    val: T,
    device: &R::Device,
    client: &ComputeClient<R::Server, R::Channel>,
) -> JitTensor<R, u32, 1> {
    let bytes = bytemuck::bytes_of(&val);
    let shape = bytes.len() / 4;

    JitTensor::new_contiguous(
        client.clone(),
        device.clone(),
        Shape::new([shape]),
        client.create(bytes),
    )
}

use shaders::wg;

#[derive(Debug, Copy, Clone)]
pub(crate) struct CreateDispatchBuffer {}

impl CubeTask for CreateDispatchBuffer {
    fn id(&self) -> String {
        "CreateDispatchBuffer".to_owned()
    }

    fn compile(&self) -> CompiledKernel {
        module_to_compiled(wg::create_shader_source(Default::default()), [1, 1, 1])
    }
}

pub fn create_dispatch_buffer<R: JitRuntime>(
    thread_nums: JitTensor<R, u32, 1>,
    wg_size: [u32; 3],
) -> JitTensor<R, u32, 1> {
    let client = thread_nums.client;
    let uniforms_buffer = create_uniform_buffer::<R, _>(
        wg::Uniforms {
            wg_size_x: wg_size[0],
            wg_size_y: wg_size[1],
            wg_size_z: wg_size[2],
        },
        &thread_nums.device,
        &client,
    );
    let ret = create_tensor([3], &thread_nums.device, &client);
    client.execute(
        Box::new(CreateDispatchBuffer {}),
        CubeCount::Static(1, 1, 1),
        vec![
            uniforms_buffer.handle.binding(),
            thread_nums.handle.binding(),
            ret.clone().handle.binding(),
        ],
    );

    ret
}
