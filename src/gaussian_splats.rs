use burn::{
    config::Config,
    module::{Module, Param},
    tensor::Device,
};
use ndarray::Axis;
use rerun::{RecordingStream, Rgba32};

use crate::splat_render::render::RenderPackage;
use crate::{
    camera::Camera,
    splat_render::{self, Backend},
    utils,
};
use burn::tensor::Distribution;
use burn::tensor::Tensor;

use anyhow::Result;

#[derive(Config)]
pub(crate) struct SplatsConfig {
    num_points: usize,
    aabb_scale: f32,
    max_sh_degree: u32,
    position_lr_scale: f32,
}

impl SplatsConfig {
    pub(crate) fn build<B: Backend>(&self, device: &Device<B>) -> Splats<B> {
        Splats::new(
            self.num_points,
            self.aabb_scale,
            self.max_sh_degree,
            0,
            device,
        )
    }
}

// A Gaussian splat model.
// This implementation wraps CUDA kernels from (Kerbel and Kopanas et al, 2023).
#[derive(Module, Debug)]
pub(crate) struct Splats<B: Backend> {
    // Current and maximum spherical harmonic degree. This is increased over
    // training.
    active_sh_degree: u32,

    // Currently maximum active sh degree.
    max_sh_degree: u32,

    // f32[n, 3]. Position.
    means: Param<Tensor<B, 2>>,

    // f32[n, sh]. SH coefficients for diffuse color.
    colors: Param<Tensor<B, 2>>,

    // f32[n, 4]. Rotation as quaternion matrices.
    rotation: Param<Tensor<B, 2>>,

    // f32[n]. Opacity parameters.
    opacity: Param<Tensor<B, 1>>,

    // f32[n, 3]. Scale matrix coefficients.
    scale: Param<Tensor<B, 2>>,

    // Non trainable params.

    // f32[n]. Maximum projected radius of each Gaussian in pixel-units. It is
    // later used during culling.
    max_radii_2d: Tensor<B, 1>,

    // Helper tensors for accumulating the viewspace_xyz gradients and the number
    // of observations per gaussian. Used in pruning and densification.
    //
    // Sum of gradient norms for each Gaussian in pixel-units. This accumulator
    // is incremented when a Gaussian is visible in a training batch.
    xyz_gradient_accum: Tensor<B, 1>,

    // Number of times a Gaussian is visible in a training batch.
    denom: Tensor<B, 1>,
}

impl<B: Backend> Splats<B> {
    pub(crate) fn new(
        num_points: usize,
        aabb_scale: f32,
        max_sh_degree: u32,
        active_sh_degree: u32,
        device: &Device<B>,
    ) -> Splats<B> {
        println!("Splats new random tensor.");
        let extent = (aabb_scale as f64) / 2.0;
        let means = Tensor::random(
            [num_points, 4],
            Distribution::Uniform(-extent, extent),
            device,
        );

        let colors = Tensor::random([num_points, 4], Distribution::Uniform(0.0, 1.0), device);

        let init_rotation = Tensor::from_floats([1.0, 0.0, 0.0, 0.0], device)
            .unsqueeze::<2>()
            .repeat(0, num_points);

        let init_opacity =
            utils::inverse_sigmoid(Tensor::from_floats([0.1], device)).repeat(0, num_points);

        // TODO: Fancy KNN init.
        let init_scale = Tensor::random([num_points, 4], Distribution::Uniform(0.1, 1.0), device);

        // Model parameters.
        Splats {
            active_sh_degree,
            max_sh_degree,
            means: means.into(),
            colors: colors.into(),
            rotation: init_rotation.into(),
            opacity: init_opacity.into(),
            scale: init_scale.into(),
            max_radii_2d: Tensor::zeros([num_points], device),
            xyz_gradient_accum: Tensor::zeros([num_points], device),
            denom: Tensor::zeros([num_points], device),
        }
    }

    // Args:
    //   cfg: ...
    //   position_lr_scale: Multiplier for learning rate for positions.  Larger
    //     values mean higher learning rates.

    // One-up sh degree.
    pub(crate) fn oneup_sh_degree(&mut self) {
        if self.active_sh_degree < self.max_sh_degree {
            self.active_sh_degree += 1
        }
    }

    // Updates rolling statistics that we capture during rendering.
    pub(crate) fn update_rolling_statistics(&mut self, render_pkg: RenderPackage<B>) {
        let radii = render_pkg.radii;

        let visible_mask = radii.clone().greater_elem(0.0);

        // TODO: This is not as efficient as could be...
        // Want these operations to be sparse.
        // TODO: Use max_pair.
        self.max_radii_2d = radii.clone().mask_where(
            visible_mask.clone(),
            Tensor::cat(
                vec![radii.unsqueeze(), self.max_radii_2d.clone().unsqueeze()],
                0,
            )
            .max_dim(0),
        );

        // TODO: How do we get grads here? Would need to be sure B: AutoDiffBackend.
        // let grad = screenspace_points.
        // self.xyz_gradient_accum[visibility_filter] += torch.norm(
        //     screenspace_points.grad[visibility_filter, :2], dim=-1, keepdim=True
        // );

        self.denom = self.denom.clone() + visible_mask.float();
    }

    /// Resets all the opacities to 0.01.
    pub(crate) fn reset_opacity(&mut self) {
        self.opacity =
            utils::inverse_sigmoid(Tensor::zeros_like(&self.opacity.val()) + 0.01).into();

        // TODO: Wtf.
        // Update optimizer with the new tensor
        //   optimizable_tensors = gs_adam_helpers.replace_tensor_to_optimizer(
        //       self.optimizer, opacities_new, 'opacity'
        //   );
        //   // Make sure that the tensor we are storing is the same tensor the
        //   // optimizer is optimizing
        //   self.opacity = optimizable_tensors['opacity'];
    }

    // // Densifies and prunes the Gaussians.
    // // Args:
    // //   max_grad: See densify_by_clone() and densify_by_split().
    // //   min_opacity_threshold: Gaussians with an opacity lower than this will be
    // //     deleted.
    // //   max_pixel_threshold: Optional. If specified, prune Gaussians whose radius
    // //     is larger than this in pixel-units.
    // //   max_world_size_threshold: Optional. If specified, prune Gaussians whose
    // //     radius is larger than this in world coordinates.
    // //   clone_vs_split_size_threshold: See densify_by_clone() and
    // //     densify_by_split().
    // fn densify_and_prune(
    //     self,
    //     max_grad: f32,
    //     min_opacity_threshold: f32,
    //     max_pixel_threshold: f32,
    //     max_world_size_threshold: f32,
    //     clone_vs_split_size_threshold: f32,
    //     device: &Device<B>,
    // ) {

    //   // f32[n,1]. Compute average magnitude of the gradient for each Gaussian in
    //   // pixel-units while accounting for the number of times each Gaussian was
    //   // seen during training.
    //   let grads = self.xyz_gradient_accum / self.denom;
    //   grads[grads.isnan()] = 0.0;

    //   self.densify_by_clone(grads, max_grad, clone_vs_split_size_threshold, device);
    //   self.densify_by_split(grads, max_grad, clone_vs_split_size_threshold, 2, device);

    //   // bool[n]. If True, delete these Gaussians.
    //   let prune_mask = (
    //       self.opacity_activation(self.opacity) < min_opacity_threshold
    //   ).squeeze();

    //   if let Some(threshold) = max_pixel_threshold {
    //     // Delete Gaussians with too large of a radius in pixel-units.
    //     let big_points_vs = self.max_radii_2d > max_pixel_threshold;

    //     // Delete Gaussians with too large of a radius in world-units.
    //     let big_points_ws =
    //         self.scale_activation(self.scale).max_dim(1).values
    //         > max_world_size_threshold;

    //     let prune_mask = Tensor::logical_or(
    //         Tensor::logical_or(prune_mask, big_points_vs), big_points_ws
    //     );
    // }

    //   self.prune_points(prune_mask);
    // }

    // // Prunes points based on the given mask.
    // //
    // // Args:
    // //   mask: bool[n]. If True, prune this Gaussian.
    // fn prune_points(&mut self, mask: Tensor<B, 2>) {
    //     // TODO: Ehh not sure how/what.
    // //   let valid_points_mask = 1.0 - mask;

    // //   let optimizable_tensors = gs_adam_helpers.prune_optimizer(
    // //       self.optimizer, valid_points_mask
    // //   );

    // //   self.xyz = optimizable_tensors['xyz'];
    // //   self.sh_dc = optimizable_tensors['sh_dc'];
    // //   self.sh_rest = optimizable_tensors['sh_rest'];
    // //   self.opacity = optimizable_tensors['opacity'];
    // //   self.scale = optimizable_tensors['scale'];
    // //   self.rotation = optimizable_tensors['rotation'];

    // //   self.xyz_gradient_accum = self.xyz_gradient_accum[valid_points_mask];
    // //   self.denom = self.denom[valid_points_mask];
    // //   self.max_radii_2d = self.max_radii_2d[valid_points_mask];
    // }

    // // Densifies Gaussians by splitting.

    // // Args:
    // //   grads: f32[n,1]. Average squared magnitude of the gradient for each
    // //     Gaussian in pixel-units.
    // //   grad_threshold: Minimum gradient magnitude for
    // //   clone_vs_split_size_threshold: Threshold on scale in world units.
    // //     Gaussians which meet the gradient condition and have a scale larger than
    // //     this are split into `n_splits` new Gaussians.
    // //   n_splits: Number of new Gaussians to create for each split Gaussian.
    // fn densify_by_split(
    //     &mut self,
    //     grads: Tensor<B, 2>,
    //     grad_threshold: f32,
    //     clone_vs_split_size_threshold: f32,
    //     n_splits: i32,
    //     device: &Device<B>
    // ) {

    //   let n_init_points = self.xyz.dims()[0];
    //   // f32[n]. Extract points that satisfy the gradient condition.
    //   let padded_grad = Tensor::zeros([n_init_points], device);
    //   padded_grad.slice_assign([0..grads.dims()[0]], grads);

    //   // Decide which Gaussians are eligible for splitting or cloning based on
    //   // their gradient magnitude.
    //   let selected_pts_mask = padded_grad >= grad_threshold;

    //   // Gaussians are split if their radius in world-units exceeds a threshold.
    //   selected_pts_mask = Tensor::logical_and(
    //       selected_pts_mask,
    //       Tensor::max_dim(self.scale_activation(self.scale), 1).values
    //       > clone_vs_split_size_threshold,
    //   );

    //   // Sample position of each new Gaussian.
    //   let stds = self.scale_activation(self.scale[selected_pts_mask]).repeat(
    //       n_splits, 1
    //   );
    //   let means = torch.zeros((stds.size(0), 3), device);
    //   let samples = torch.normal(mean=means, std=stds);
    //   let rots = gs_utils.qvec2rotmat(self.rotation[selected_pts_mask]).repeat(
    //       n_splits, 1, 1
    //   );
    //   let new_xyz = torch.bmm(rots, samples.unsqueeze(-1)).squeeze(-1) + self.xyz[
    //       selected_pts_mask
    //   ].repeat(n_splits, 1);

    //   // Set the scale of each new Gaussian to approximately 1/k of its parent.
    //   let new_scale = self.inverse_scale_activation(
    //       self.scale_activation(self.scale[selected_pts_mask]).repeat(n_splits, 1)
    //       / (0.8 * n_splits)
    //   );

    //   // Split Gaussians inherit remaining properties from their parent.
    //   let new_rotation = self.rotation[selected_pts_mask].repeat(n_splits, 1);
    //   let new_sh_dc = self.sh_dc[selected_pts_mask].repeat(n_splits, 1, 1);
    //   let new_sh_rest = self.sh_rest[selected_pts_mask].repeat(n_splits, 1, 1);
    //   let new_opacity = self.opacity[selected_pts_mask].repeat(n_splits, 1);

    //   self.densification_postfix(
    //       new_xyz,
    //       new_sh_dc,
    //       new_sh_rest,
    //       new_opacity,
    //       new_scale,
    //       new_rotation,
    //   );

    //   let prune_filter = torch.cat((
    //       selected_pts_mask,
    //       torch.zeros(
    //           n_splits * selected_pts_mask.sum()
    //       ),
    //   ));

    //   self.prune_points(prune_filter);
    // }

    // // Densifies Gaussians by cloning.
    // //
    // // Args:
    // //   grads: f32[n,1]. Average squared magnitude of the gradient for each
    // //     Gaussian in pixel-units.
    // //   grad_threshold: Minimum gradient magnitude for
    // //   clone_vs_split_size_threshold: Threshold on scale in world units.
    // //     Gaussians which meet the gradient condition and have a scale smaller
    // //     than this are cloned with the exact same parameters.
    // fn densify_by_clone(
    //     &mut self,
    //     grads: Tensor<B, 2>,
    //     grad_threshold: f32,
    //     clone_vs_split_size_threshold: f32,
    //     device: &Device<B>,
    // ) {

    //   // Extract points that satisfy the gradient condition
    //   let selected_pts_mask = Tensor::where(
    //       torch.norm(grads, dim=-1) >= grad_threshold, true, false
    //   );

    //   // From those choose only the ones that are small enough to be cloned
    //   selected_pts_mask = Tensor::logical_and(
    //       selected_pts_mask,
    //       Tensor::max_dim(self.scale_activation(self.scale), 1).values
    //       <= clone_vs_split_size_threshold,
    //   );

    //   let new_xyz = self.xyz[selected_pts_mask];
    //   let new_sh_dc = self.sh_dc[selected_pts_mask];
    //   let new_sh_rest = self.sh_rest[selected_pts_mask];
    //   let new_opacities = self.opacity[selected_pts_mask];
    //   let new_scale = self.scale[selected_pts_mask];
    //   let new_rotation = self.rotation[selected_pts_mask];

    //   self.densification_postfix(
    //       new_xyz,
    //       new_sh_dc,
    //       new_sh_rest,
    //       new_opacities,
    //       new_scale,
    //       new_rotation,
    //   );
    // }

    // // Updates the optimizer by appending the new tensors.
    // fn densification_postfix(
    //     self,
    //     new_xyz: Tensor<B, 2>,
    //     new_features_dc: Tensor<B, 3>,
    //     new_features_rest: Tensor<B, 3>,
    //     new_opacities: Tensor<B, 2>,
    //     new_scale: Tensor<B, 2>,
    //     new_rotation: Tensor<B, 2>,
    // ) {
    //   tensors_dict = {
    //       'xyz': new_xyz,
    //       'sh_dc': new_features_dc,
    //       'sh_rest': new_features_rest,
    //       'opacity': new_opacities,
    //       'scale': new_scale,
    //       'rotation': new_rotation,
    //   };

    //   optimizable_tensors = gs_adam_helpers.cat_tensors_to_optimizer(
    //       self.optimizer, tensors_dict
    //   );

    //   self.xyz = optimizable_tensors['xyz'];
    //   self.sh_dc = optimizable_tensors['sh_dc'];
    //   self.sh_rest = optimizable_tensors['sh_rest'];
    //   self.opacity = optimizable_tensors['opacity'];
    //   self.scale = optimizable_tensors['scale'];
    //   self.rotation = optimizable_tensors['rotation'];

    //   self.xyz_gradient_accum = torch.zeros((self.xyz.shape[0], 1), device='cuda');
    //   self.denom = torch.zeros((self.xyz.shape[0], 1), device='cuda');
    //   self.max_radii_2d = torch.zeros((self.xyz.shape[0]), device='cuda');
    // }

    // Renders an image by splatting the gaussians.
    // Args:
    //   camera: Camera to render.
    //   bg_color: Background color.
    // Returns:
    //   A tuple of which the first element is the rendered image and the second
    //   elements is a dictionary consisting of statistics that we need to keep
    //   track
    //   during training. More specifically:
    //   * screenspace_points: a tensor that "holds" the viewspace positional
    //     gradients
    //   * visibility_filter: a boolean tensor that indicates which gaussians
    //     participated in the rendering.
    //   * radii: the maximum screenspace radius of each gaussian
    pub(crate) fn render(&self, camera: &Camera, bg_color: glam::Vec3) -> Tensor<B, 3> {
        splat_render::render::render(
            camera,
            self.means.val(),
            self.scale.val(),
            self.rotation.val(),
            self.colors.val(),
            burn::tensor::activation::sigmoid(self.opacity.val()),
            bg_color,
        )
    }

    pub(crate) fn visualize(&self, rec: &RecordingStream) -> Result<()> {
        let points_data = utils::burn_to_ndarray(self.means.val());

        let glam_data = points_data
            .axis_iter(Axis(0))
            .map(|c| glam::vec3(c[0], c[1], c[2]))
            .collect::<Vec<_>>();

        let colors_data = utils::burn_to_ndarray(self.colors.val());
        let colors = colors_data.axis_iter(Axis(0)).map(|c| {
            Rgba32::from([
                (c[[0]] * 255.0) as u8,
                (c[[1]] * 255.0) as u8,
                (c[[2]] * 255.0) as u8,
            ])
        });
        rec.log(
            "world/splat/points",
            &rerun::Points3D::new(glam_data).with_colors(colors),
        )?;
        Ok(())
    }

    pub(crate) fn cur_num_points(&self) -> usize {
        self.means.dims()[0]
    }
}
