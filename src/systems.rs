use crate::components::{ControllerInput, ControllerSettings, ControllerState};
use crate::WanderlustPhysicsTweaks;
use bevy::{math::*, prelude::*};
use bevy_rapier3d::prelude::*;

/// *Note: Most users will not need to use this directly. Use [`WanderlustPlugin`](crate::plugins::WanderlustPlugin) instead.
/// This system is useful for cases such as running on a fixed timestep.*
///
/// The system that controls movement logic.
pub fn movement(
    mut bodies: Query<(
        Entity,
        &GlobalTransform,
        &mut ExternalImpulse,
        &mut ControllerState,
        &ControllerSettings,
        &mut ControllerInput,
    )>,
    velocities: Query<&Velocity>,
    time: Res<Time>,
    ctx: Res<RapierContext>,
    mut ground_casts: Local<Vec<(Entity, Toi)>>,
) {
    for (entity, tf, mut body, mut controller, settings, mut input) in bodies.iter_mut() {
        let dt = time.delta_seconds();

        // Sometimes, such as at the beginning of the game, deltatime is 0. This
        // can cause division by 0 so I just skip those frames. A better solution
        // is a fixed framerate that has a static dt, but bevy doesn't have
        // that to my knowledge.
        if dt == 0.0 {
            return;
        }

        // Get the ground and velocities
        let ground_cast = if controller.skip_ground_check_timer == 0.0
            && !settings.skip_ground_check_override
        {
            intersections_with_shape_cast(
                &*ctx,
                tf.mul_vec3(settings.float_cast_origin),
                tf.to_scale_rotation_translation().1,
                -settings.up_vector,
                &settings.float_cast_collider,
                settings.float_cast_length,
                QueryFilter::new()
                    .predicate(&|collider| collider != entity)
                    .exclude_sensors(),
                &mut *ground_casts,
            );
            ground_casts
                .iter()
                .filter(|(_, i)| {
                    i.status != TOIStatus::Penetrating
                        && i.normal1.angle_between(settings.up_vector) <= settings.max_ground_angle
                })
                .next()
                .cloned()
        } else {
            controller.skip_ground_check_timer = (controller.skip_ground_check_timer - dt).max(0.0);
            None
        };

        // If we hit something, just get back up instead of waiting.
        if ctx.contacts_with(entity).next().is_some() {
            controller.skip_ground_check_timer = 0.0;
        }

        let float_offset = if let Some((_, toi)) = ground_cast {
            Some(toi.toi - settings.float_distance)
        } else {
            None
        };

        let grounded = float_offset
            .map(|offset| {
                offset <= settings.max_float_offset && offset >= settings.min_float_offset
            })
            .unwrap_or(false);

        if grounded {
            controller.remaining_jumps = settings.extra_jumps;
            controller.coyote_timer = settings.coyote_time_duration;
        } else {
            controller.coyote_timer = (controller.coyote_timer - dt).max(0.0);
        }

        // Gravity
        let gravity = if ground_cast.is_none() {
            settings.up_vector * -settings.gravity * dt
        } else {
            Vec3::ZERO
        };

        // Collect velocities
        let velocity = velocities
            .get(entity)
            .expect("Character controllers must have a Velocity component");
        let ground_vel;

        // Calculate "floating" force, as seen [here](https://www.youtube.com/watch?v=qdskE8PJy6Q)
        let mut float_spring = if let Some((ground, intersection)) = ground_cast {
            ground_vel = velocities.get(ground).ok();

            let vel_align = (-settings.up_vector).dot(velocity.linvel);
            let ground_vel_align =
                (-settings.up_vector).dot(ground_vel.map(|v| v.linvel).unwrap_or(Vec3::ZERO));

            let relative_align = vel_align - ground_vel_align;

            let snap = intersection.toi - settings.float_distance;

            (-settings.up_vector)
                * ((snap * settings.float_strength) - (relative_align * settings.float_dampen))
        } else {
            ground_vel = None;
            Vec3::ZERO
        };

        // Calculate horizontal movement force
        let movement = {
            let dir = input.movement.clamp_length_max(1.0);

            // let unit_vel = controller.last_goal_velocity.normalized();

            // let vel_dot = unit_dir.dot(unit_vel);

            let accel = settings.acceleration;

            let input_goal_vel = dir * settings.max_speed;

            let goal_vel = Vec3::lerp(
                controller.last_goal_velocity,
                input_goal_vel + ground_vel.map(|v| v.linvel).unwrap_or(Vec3::ZERO),
                (accel * dt).min(1.0),
            );

            let needed_accel = goal_vel - velocity.linvel;

            let max_accel_force = settings.max_acceleration_force;

            let needed_accel = needed_accel.clamp_length_max(max_accel_force);

            controller.last_goal_velocity = goal_vel;

            needed_accel * settings.force_scale
        };

        let just_jumped = input.jumping && !controller.jump_pressed_last_frame;
        if !grounded {
            if just_jumped {
                controller.jump_buffer_timer = settings.jump_buffer_duration;
            } else {
                controller.jump_buffer_timer = (controller.jump_buffer_timer - dt).max(0.0);
            }
        }

        // Calculate jump force
        let mut jump = if controller.jump_timer > 0.0 && !grounded {
            if !input.jumping {
                controller.jump_timer = 0.0;
                velocity.linvel.project_onto(settings.up_vector) * -settings.jump_stop_force
            } else {
                controller.jump_timer = (controller.jump_timer - dt).max(0.0);

                // Float force can lead to inconsistent jump power
                float_spring = Vec3::ZERO;

                settings.jump_force
                    * settings.up_vector
                    * dt
                    * (settings.jump_decay_function)(
                        (settings.jump_time - controller.jump_timer) / settings.jump_time,
                    )
            }
        } else {
            Vec3::ZERO
        };

        // Trigger a jump
        if (just_jumped || controller.jump_buffer_timer > 0.0)
            && (grounded || controller.coyote_timer > 0.0 || controller.remaining_jumps > 0)
        {
            if !grounded && controller.coyote_timer == 0.0 {
                controller.remaining_jumps -= 1;
            }

            controller.jump_buffer_timer = 0.0;
            controller.jump_timer = settings.jump_time;
            controller.skip_ground_check_timer = settings.jump_skip_ground_check_duration;
            // Negating the current velocity increases consistency for falling jumps,
            // and prevents stacking jumps to reach high upwards velocities
            jump = velocity.linvel * settings.up_vector * -1.0;
            jump += settings.jump_initial_force * settings.up_vector;
            // Float force can lead to inconsistent jump power
            float_spring = Vec3::ZERO;
        }

        // Calculate force to stay upright
        let upright = {
            let (to_goal_axis, to_goal_angle) = {
                let current = tf.up();
                (
                    current.cross(settings.up_vector).normalize_or_zero(),
                    current.angle_between(settings.up_vector),
                )
            };

            ((to_goal_axis * (to_goal_angle * settings.upright_spring_strength))
                - (velocity.angvel * settings.upright_spring_damping))
                * dt
        };

        // Apply positional force to the rigidbody
        body.impulse = movement + jump + float_spring + gravity + input.custom_impulse;
        input.custom_impulse = Vec3::ZERO;
        // Apply rotational force to the rigidbody
        body.torque_impulse = upright + input.custom_torque;
        input.custom_torque = Vec3::ZERO;

        controller.jump_pressed_last_frame = input.jumping;
    }
}

/// *Note: Most users will not need to use this directly. Use [`WanderlustPlugin`](crate::plugins::WanderlustPlugin) instead.
/// Alternatively, if one only wants to disable the system, use [`WanderlustPhysicsTweaks`](WanderlustPhysicsTweaks).*
///
/// This system adds some tweaks to rapier's physics settings that make the character controller behave better.
pub fn setup_physics_context(
    mut ctx: ResMut<RapierContext>,
    should_change: Option<Res<WanderlustPhysicsTweaks>>,
) {
    if should_change.map(|s| s.should_do_tweaks()).unwrap_or(true) {
        let params = &mut ctx.integration_parameters;
        // This prevents any noticeable jitter when running facefirst into a wall.
        params.erp = 0.99;
        // This prevents (most) noticeable jitter when running facefirst into an inverted corner.
        params.max_velocity_iterations = 16;
        // TODO: Fix jitter that occurs when running facefirst into a normal corner.
    }
}

fn intersections_with_shape_cast(
    ctx: &RapierContext,
    shape_pos: Vec3,
    shape_rot: Quat,
    shape_vel: Vec3,
    shape: &Collider,
    max_toi: f32,
    filter: QueryFilter,
    collisions: &mut Vec<(Entity, Toi)>,
) {
    collisions.clear();

    let orig_predicate = filter.predicate;

    loop {
        let predicate = |entity| {
            !collisions.iter().any(|coll| coll.0 == entity)
                && orig_predicate.map(|pred| pred(entity)).unwrap_or(true)
        };
        let filter = filter.predicate(&predicate);

        if let Some(collision) =
            ctx.cast_shape(shape_pos, shape_rot, shape_vel, shape, max_toi, filter)
        {
            collisions.push(collision);
        } else {
            break;
        }
    }
}
